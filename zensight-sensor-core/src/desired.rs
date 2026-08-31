//! Reconciling `@desired` fleet configuration (#816, RFC 07 §3 / RFC 12 §3).
//!
//! A controller publishes per-host runtime POLICY under
//! `v1/@desired/state/<this-host>/<producer>/<topic>`; this module is the
//! consumer half: subscribe + seed, decode, validate via the caller's
//! `apply`, and publish the [`AppliedConfig`] marker
//! (`state/<producer>/applied/<topic>`) that says what is actually in force
//! and why — so drift between desired and effective is visible rather than
//! assumed.
//!
//! Convergence discipline:
//! - **The storage GET is the primary path.** A seed GET at startup plus a
//!   periodic re-GET (`refresh_secs`) is level-triggered: it survives any
//!   missed sample, reconnect or router restart. The live
//!   AdvancedSubscriber (history + recovery — the correlator's recipe) is
//!   the latency accelerator, not the correctness mechanism.
//! - **LWW by sample timestamp.** Re-seeds and history replays are
//!   idempotent: a sample at or before the last applied timestamp is a
//!   no-op. An operator's `@rpc/<topic>/set` is a second writer to the same
//!   handle; the rule between the two writers is LWW **by arrival**, and
//!   the marker's `source` field is what says who won last — honest about
//!   the race instead of hiding it.
//! - **Invalid desired state is rejected loudly and kept OFF the handle**:
//!   the previous good config keeps running, and the rejection rides the
//!   marker (`last_rejected`) — on the bus, not only in a log. Never a
//!   crash-loop, never silent partial application.
//! - **A `Delete` reverts to the file-config baseline** (`source: file`).
//! - **The kill switch wins first**: `desired.enabled = false` in FILE
//!   config means no subscriber, no GETs, marker `source: file` — the
//!   mechanism that could misbehave is disarmable from outside itself.
//!
//! **The structural never-list** (#816's most important constraint): this
//! module can only deserialize the caller's `Doc` type and only invoke the
//! caller's `apply`. Session endpoints, TLS and the namespace are consumed
//! by `zensight_common::session` long before any reconciler runs and have
//! no writer here — one bad desired publish cannot lock the fleet out of
//! its own supervision.

use std::time::Duration;

use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};
use zensight_common::desired::{AppliedConfig, AppliedSource, DesiredConfig, RejectedDesired};
use zensight_common::{QosClass, decode_auto};

use crate::publisher::Publisher;

/// One reconcilable topic. The caller builds `desired_key` from the
/// generated registry (`registry::desired::key(&Subject::…(host_id))`) so
/// this module stays registry-agnostic.
pub struct DesiredTopic {
    /// The topic chunk (`"expectations"`, `"rules"`) — names the marker key.
    pub topic: &'static str,
    /// The concrete `v1/@desired/state/<this-host>/<producer>/<topic>` key.
    pub desired_key: zenkey::Key,
}

/// Publishes the `applied/<topic>` marker. Handed back to the caller so its
/// `@rpc/<topic>/set` surface can stamp `source: rpc` when the operator
/// writer wins — the two writers share one marker.
#[derive(Clone)]
pub struct AppliedMarker {
    publisher: Publisher,
    key: String,
    topic: &'static str,
}

impl AppliedMarker {
    pub fn new(publisher: Publisher, topic: &'static str) -> AppliedMarker {
        use zensight_common::v1::V1ContextExt;
        let key = publisher
            .v1()
            .const_state_key(&["applied", topic])
            .to_string();
        AppliedMarker {
            publisher,
            key,
            topic,
        }
    }

    /// Publish the marker. `effective` is serialized to the JSON-string
    /// field ([`AppliedConfig::effective_json`] explains why a string).
    pub async fn publish<Doc: serde::Serialize>(
        &self,
        source: AppliedSource,
        effective: &Doc,
        desired_timestamp: Option<String>,
        last_rejected: Option<RejectedDesired>,
    ) {
        let marker = AppliedConfig {
            topic: self.topic.to_string(),
            source,
            applied_at: zensight_common::current_timestamp_millis(),
            desired_timestamp,
            effective_json: serde_json::to_string(effective).unwrap_or_default(),
            last_rejected,
        };
        if let Err(e) = self
            .publisher
            .publish_json(&self.key, &marker, QosClass::Command)
            .await
        {
            tracing::warn!(error = %e, key = %self.key, "desired: failed to publish applied marker");
        }
    }
}

/// Reconcile one topic until the session closes. Returns the marker handle
/// (for the RPC writer) and the join handle of the reconcile task.
///
/// `baseline` is the file/default config — what a `Delete` reverts to.
/// `apply` may refuse with a human-readable reason; a refusal keeps the
/// previous good config and rides the marker.
pub fn reconcile_topic<Doc, A, AF>(
    session: std::sync::Arc<zenoh::Session>,
    publisher: Publisher,
    spec: DesiredTopic,
    cfg: DesiredConfig,
    baseline: Doc,
    apply: A,
) -> (AppliedMarker, tokio::task::JoinHandle<()>)
where
    Doc: serde::de::DeserializeOwned + serde::Serialize + Clone + Send + Sync + 'static,
    A: Fn(Doc) -> AF + Send + Sync + 'static,
    AF: Future<Output = Result<(), String>> + Send + 'static,
{
    let marker = AppliedMarker::new(publisher, spec.topic);
    let m = marker.clone();
    let task = tokio::spawn(async move {
        run(session, spec, cfg, baseline, apply, m).await;
    });
    (marker, task)
}

async fn run<Doc, A, AF>(
    session: std::sync::Arc<zenoh::Session>,
    spec: DesiredTopic,
    cfg: DesiredConfig,
    baseline: Doc,
    apply: A,
    marker: AppliedMarker,
) where
    Doc: serde::de::DeserializeOwned + serde::Serialize + Clone + Send + Sync + 'static,
    A: Fn(Doc) -> AF + Send + Sync + 'static,
    AF: Future<Output = Result<(), String>> + Send + 'static,
{
    // Baseline marker first, kill switch second: even a disabled reconciler
    // says what is in force (the file config) — drift is visible before any
    // desired doc exists, and "disabled" never reads as "silent".
    marker
        .publish(AppliedSource::File, &baseline, None, None)
        .await;
    if !cfg.enabled {
        tracing::info!(
            topic = spec.topic,
            "desired-state reconcile disabled by file config (kill switch)"
        );
        return;
    }

    let key = spec.desired_key.to_string();
    // The accelerator: history + recovery, the correlator's late-joiner
    // recipe. If the advanced machinery ever misbehaves across the verbatim
    // `@desired` chunk, nothing is lost but latency — the GET below is the
    // correctness path.
    let sub = match session
        .declare_subscriber(&key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "desired: subscriber failed — GET-only reconcile");
            // Degrade to pure polling rather than giving up: the refresh
            // loop below still converges.
            return poll_only(session, &key, cfg, baseline, apply, marker).await;
        }
    };

    let mut state = ReconcileState::new(baseline);
    // Seed GET before the loop: the storage answers even when the
    // controller is long dead.
    seed_get(&session, &key, &mut state, &apply, &marker).await;

    let mut refresh = tokio::time::interval(Duration::from_secs(cfg.refresh_secs.max(1)));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh.tick().await; // arm (first tick fires immediately)
    loop {
        tokio::select! {
            sample = sub.recv_async() => match sample {
                Ok(sample) => state.consider(&sample, &apply, &marker).await,
                Err(_) => break, // session closing
            },
            _ = refresh.tick() => {
                seed_get(&session, &key, &mut state, &apply, &marker).await;
            }
        }
    }
}

async fn poll_only<Doc, A, AF>(
    session: std::sync::Arc<zenoh::Session>,
    key: &str,
    cfg: DesiredConfig,
    baseline: Doc,
    apply: A,
    marker: AppliedMarker,
) where
    Doc: serde::de::DeserializeOwned + serde::Serialize + Clone + Send + Sync + 'static,
    A: Fn(Doc) -> AF + Send + Sync + 'static,
    AF: Future<Output = Result<(), String>> + Send + 'static,
{
    let mut state = ReconcileState::new(baseline);
    let mut refresh = tokio::time::interval(Duration::from_secs(cfg.refresh_secs.max(1)));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        refresh.tick().await;
        seed_get(&session, key, &mut state, &apply, &marker).await;
    }
}

async fn seed_get<Doc, A, AF>(
    session: &zenoh::Session,
    key: &str,
    state: &mut ReconcileState<Doc>,
    apply: &A,
    marker: &AppliedMarker,
) where
    Doc: serde::de::DeserializeOwned + serde::Serialize + Clone + Send + Sync + 'static,
    A: Fn(Doc) -> AF + Send + Sync + 'static,
    AF: Future<Output = Result<(), String>> + Send + 'static,
{
    match session.get(key).timeout(Duration::from_secs(5)).await {
        Ok(replies) => {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    state.consider(sample, apply, marker).await;
                }
            }
        }
        Err(e) => tracing::debug!(error = %e, key = %key, "desired: seed GET failed (no storage?)"),
    }
}

/// The per-topic fold: LWW guard + decode/apply/revert + marker.
struct ReconcileState<Doc> {
    baseline: Doc,
    last_applied: Option<zenoh::time::Timestamp>,
    /// What is actually in force right now — restated (with the rejection
    /// attached) when a bad doc is refused, so the marker never guesses.
    effective: (AppliedSource, Doc, Option<String>),
}

impl<Doc> ReconcileState<Doc>
where
    Doc: serde::de::DeserializeOwned + serde::Serialize + Clone + Send + Sync + 'static,
{
    fn new(baseline: Doc) -> Self {
        ReconcileState {
            baseline: baseline.clone(),
            last_applied: None,
            effective: (AppliedSource::File, baseline, None),
        }
    }

    async fn consider<A, AF>(
        &mut self,
        sample: &zenoh::sample::Sample,
        apply: &A,
        marker: &AppliedMarker,
    ) where
        A: Fn(Doc) -> AF + Send + Sync + 'static,
        AF: Future<Output = Result<(), String>> + Send + 'static,
    {
        // LWW: an unstamped sample cannot be ordered, so it is refused (the
        // deployment's storage and cached publishers always stamp; an
        // unstamped desired doc is a misconfigured author).
        let Some(ts) = sample.timestamp().copied() else {
            tracing::warn!(key = %sample.key_expr(), "desired: unstamped sample refused (cannot LWW-order)");
            return;
        };
        if self.last_applied.is_some_and(|last| ts <= last) {
            return; // replay / re-seed — idempotent
        }
        match sample.kind() {
            zenoh::sample::SampleKind::Delete => {
                let doc = self.baseline.clone();
                match apply(doc.clone()).await {
                    Ok(()) => {
                        self.last_applied = Some(ts);
                        tracing::info!(
                            topic = marker.topic,
                            "desired: delete — reverted to file baseline"
                        );
                        self.effective = (AppliedSource::File, doc.clone(), None);
                        marker.publish(AppliedSource::File, &doc, None, None).await;
                    }
                    Err(e) => {
                        // The baseline came from validated file config; a
                        // refusal here is a bug worth shouting about, but
                        // never a crash.
                        tracing::error!(error = %e, topic = marker.topic, "desired: baseline re-apply refused");
                    }
                }
            }
            zenoh::sample::SampleKind::Put => {
                let bytes = sample.payload().to_bytes();
                let outcome = decode_auto::<Doc>(&bytes).map_err(|e| format!("decode: {e}"));
                let doc = match outcome {
                    Ok(doc) => doc,
                    Err(error) => {
                        tracing::warn!(error = %error, topic = marker.topic,
                            "desired: invalid document rejected — keeping previous config");
                        self.reject(marker, ts, error).await;
                        return;
                    }
                };
                match apply(doc.clone()).await {
                    Ok(()) => {
                        self.last_applied = Some(ts);
                        tracing::info!(topic = marker.topic, ts = %ts, "desired: applied");
                        self.effective =
                            (AppliedSource::Desired, doc.clone(), Some(ts.to_string()));
                        marker
                            .publish(AppliedSource::Desired, &doc, Some(ts.to_string()), None)
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, topic = marker.topic,
                            "desired: document refused by validation — keeping previous config");
                        self.reject(marker, ts, error).await;
                    }
                }
            }
        }
    }

    async fn reject(&self, marker: &AppliedMarker, ts: zenoh::time::Timestamp, error: String) {
        // The marker restates what IS in force — the last GOOD apply, never
        // a guess — with the rejection riding beside it.
        let (source, doc, desired_ts) = &self.effective;
        marker
            .publish(
                *source,
                doc,
                desired_ts.clone(),
                Some(RejectedDesired {
                    at: zensight_common::current_timestamp_millis(),
                    timestamp: Some(ts.to_string()),
                    error,
                }),
            )
            .await;
    }
}
