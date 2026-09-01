//! Alert lifecycle management and publishing.
//!
//! [`AlertReporter`] is the sensor-side counterpart to
//! [`zensight_common::Alert`]: it owns a [`Publisher`], tracks which alerts are
//! currently firing, applies a "must be violated continuously for N" debounce,
//! and publishes firing/resolved transitions to the v1 state key
//! `<base>/v1/<origin>/state/<producer>/alert/<alert_key>` (a `Put` to raise/update, a `Put`
//! with state `Resolved` followed by a `Delete` tombstone to clear).
//!
//! Usage from an evaluator sweep:
//! ```ignore
//! // Each violation this tick:
//! reporter.observe(alert, exp.for_duration()).await?;
//! // After evaluating a rule, resolve anything that's no longer violated:
//! reporter.reconcile(rule, &still_firing_keys).await?;
//! ```
//!
//! # Alerts outlive the process; the firing set must too (#882)
//!
//! A firing alert is a **claim this producer is making**, stored at a key only
//! this producer writes. `reconcile` retracts it when the condition clears —
//! but only while the process that raised it is still running. A restart
//! begins with an empty `active` map, so an alert that was firing beforehand
//! and is no longer true is never fired again *and therefore never resolved*:
//! the `Firing` document is simply abandoned. Without a storage nobody notices
//! (the sample ages out of the network). With a `latest` storage on
//! `v1/*/state/**` it is durable, and served to every late joiner forever.
//!
//! So the reporter owns both ends of its own lifetime:
//!
//! - [`AlertReporter::adopt_persisted`] at startup — GET this producer's own
//!   alert selector and take ownership of whatever the previous incarnation
//!   left there. Adopted alerts enter `active` already `published`, so the very
//!   next `reconcile` retracts the ones that are no longer true and re-observing
//!   one that still is publishes nothing. No new lifecycle state: the existing
//!   sweep does the work.
//! - [`AlertReporter::resolve_all`] at shutdown — retract and tombstone
//!   everything still firing, so a clean stop leaves nothing behind at all.
//!
//! Adoption is what covers the cases a shutdown hook cannot: SIGKILL, an OOM
//! kill, a panic, and the common one — a restart whose *config* no longer
//! defines the target the alert was about, so nothing will ever evaluate it
//! again. [`SensorRunner`](crate::SensorRunner) drives both for any reporter
//! registered with `with_alert_reporter`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zensight_common::v1::V1ContextExt;
use zensight_common::{Alert, AlertSeverity, AlertState, Format, Protocol, encode};

use crate::error::Result;
use crate::publisher::Publisher;

/// Internal state for a single tracked alert.
struct ActiveAlert {
    rule: String,
    severity: AlertSeverity,
    first_seen: Instant,
    /// The most recent firing payload (republished on resolve as `Resolved`).
    last: Alert,
    /// Whether a `Put(Firing)` has actually been published yet (false while the
    /// `for:` debounce window is still open).
    published: bool,
}

/// What the synchronous bookkeeping decided we should do on the wire.
enum Action {
    None,
    PublishFiring(Alert),
    Resolve(Vec<Alert>),
}

/// Owns alert publishing + firing/resolved lifecycle for one sensor namespace.
pub struct AlertReporter {
    publisher: Publisher,
    protocol: Protocol,
    format: Format,
    debounce: Duration,
    identity: Option<crate::identity::SharedIdentity>,
    /// The rule slugs this build can still raise, when the producer declares
    /// them. Empty means "unknown", not "none" — see [`AlertReporter::with_known_rules`].
    known_rules: Vec<String>,
    active: Mutex<HashMap<String, ActiveAlert>>,
}

impl AlertReporter {
    /// Create a reporter. `publisher`'s v1 context keys the alert state
    /// (`state/<producer>/alert/<key>`); the telemetry prefix is ignored for
    /// alert keys (we build the full key from `protocol`).
    pub fn new(publisher: Publisher, protocol: Protocol, format: Format) -> Self {
        Self {
            publisher,
            protocol,
            format,
            debounce: Duration::ZERO,
            identity: None,
            known_rules: Vec::new(),
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Set the default "must be violated continuously for" debounce window.
    pub fn with_debounce(mut self, d: Duration) -> Self {
        self.debounce = d;
        self
    }

    /// Declare the rule slugs this build can still raise — the producer's
    /// `ALL_RULES` table, where it has one.
    ///
    /// Used by [`adopt_persisted`](Self::adopt_persisted) only, and for one
    /// case: a rule *deleted from the build*. Its inherited alerts would
    /// otherwise be adopted and then never reconciled, because no sweep of
    /// that rule will ever run again — firing forever for the same reason
    /// #882 was filed about. A producer that declares its rule table lets
    /// adoption retire those on the spot.
    ///
    /// Declaring nothing is not "no rules": an empty table means the producer
    /// has not said, and adoption keeps every inherited alert. Silence must
    /// never read as a licence to delete.
    pub fn with_known_rules<S: Into<String>>(mut self, rules: impl IntoIterator<Item = S>) -> Self {
        self.known_rules = rules.into_iter().map(Into::into).collect();
        // The framework raises `sensor-budget` (#811) on whichever reporter the
        // runner was handed, so it belongs to every table without a producer
        // having to remember it. Forgetting it would retire a live alert.
        let budget = crate::health::SENSOR_BUDGET_RULE.to_string();
        if !self.known_rules.contains(&budget) {
            self.known_rules.push(budget);
        }
        self
    }

    /// Stamp the host identity onto every observed alert as the `host.id`
    /// annotation label (identity envelope, #301). Annotation labels are
    /// excluded from `alert_key()`, so stamping never changes alert identity —
    /// firing/resolve pairs stay matched across identity refreshes.
    pub fn with_identity(mut self, identity: crate::identity::SharedIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The state selector this producer's alerts live under:
    /// `<base>/v1/<origin>/state/<producer>/alert/*`. One string, used by both
    /// the late-joiner seed queryable and the startup adoption GET, so the two
    /// can never disagree about what this reporter owns.
    pub fn alert_selector(&self) -> String {
        format!("{}/*", self.publisher.v1().const_state_key(&["alert"]))
    }

    fn alert_key_expr(&self, alert_key: &str) -> String {
        // v1 (RFC 04 §1.2): alerts are LWW state under the producer, keyed by
        // the origin — the legacy protocol-shared channel is gone.
        // `alert_key` is a 16-hex digest, so the reserved-token refusal
        // zenkey 0.7 added is unreachable here (see `V1ContextExt`).
        self.publisher
            .v1()
            .const_state_key(&["alert", alert_key])
            .into()
    }

    /// Report that `alert` is currently violated. Publishes a `Put(Firing)` once
    /// the alert has been continuously observed for `for_duration` (or the
    /// reporter default). Idempotent within the debounce window; re-publishes if
    /// the severity escalates after firing.
    pub async fn observe(&self, mut alert: Alert, for_duration: Option<Duration>) -> Result<()> {
        // Stamp the identity annotation once at entry: `entry.last` then carries
        // it through the firing publication, the `state alert selector` seed, and the
        // eventual resolve — one stamp site, consistent everywhere.
        if let Some(host_id) = self.identity.as_ref().and_then(|i| i.get().host_id) {
            alert.labels.insert("host.id".to_string(), host_id);
        }
        let key = alert.alert_key();
        let dur = for_duration.unwrap_or(self.debounce);
        let action = {
            let mut active = self.active.lock().unwrap();
            let now = Instant::now();
            let entry = active.entry(key.clone()).or_insert_with(|| ActiveAlert {
                rule: alert.rule.clone(),
                severity: alert.severity,
                first_seen: now,
                last: alert.clone(),
                published: false,
            });
            let severity_changed = entry.published && entry.severity != alert.severity;
            entry.severity = alert.severity;
            entry.last = alert.clone();
            if !entry.published && now.duration_since(entry.first_seen) >= dur {
                entry.published = true;
                Action::PublishFiring(alert)
            } else if severity_changed {
                Action::PublishFiring(alert)
            } else {
                Action::None
            }
        };
        self.apply(&key, action).await
    }

    /// After evaluating all violations for `rule` this sweep, resolve any
    /// previously-firing alert under that rule whose key is no longer in
    /// `still_firing`.
    pub async fn reconcile(&self, rule: &str, still_firing: &[String]) -> Result<()> {
        let action = {
            let mut active = self.active.lock().unwrap();
            Self::retire(&mut active, |k, a| {
                a.rule == rule && !still_firing.iter().any(|s| s == k)
            })
        };
        // `apply` keys off the alert itself for Resolve; key arg unused there.
        self.apply("", action).await
    }

    /// Drop every entry `no_longer_violated` selects. A **published** entry
    /// yields a `Resolved` payload; an **unpublished** one — still inside its
    /// `for:` window — is simply forgotten, so the next observation starts a
    /// fresh debounce clock.
    ///
    /// Forgetting the unpublished ones is what makes "continuously observed
    /// for N" true rather than "seen once ≥ N ago": before this, an entry that
    /// blipped for one sweep kept its `first_seen` forever and the next blip
    /// an hour later published immediately. It is also what bounds `active`:
    /// a grader that (wrongly, but it happened — probe's `duration_ms`,
    /// systemd's `overdue_secs`) put a per-sweep measurement into the labels
    /// minted a new key every sweep, none of which could ever be evicted, so
    /// the sensor watching for leaks leaked through its own alerting.
    fn retire(
        active: &mut HashMap<String, ActiveAlert>,
        no_longer_violated: impl Fn(&str, &ActiveAlert) -> bool,
    ) -> Action {
        let to_drop: Vec<String> = active
            .iter()
            .filter(|(k, a)| no_longer_violated(k, a))
            .map(|(k, _)| k.clone())
            .collect();
        let mut payloads = Vec::new();
        for k in to_drop {
            if let Some(a) = active.remove(&k)
                && a.published
            {
                payloads.push(a.last.resolved());
            }
        }
        if payloads.is_empty() {
            Action::None
        } else {
            Action::Resolve(payloads)
        }
    }

    /// Like [`reconcile`](Self::reconcile), but scoped to alerts carrying
    /// `label_key == label_value` — for proxy sensors (snmp, modbus, gnmi)
    /// where several observed devices share one reporter and each device's
    /// sweep must not resolve the others' firing alerts.
    pub async fn reconcile_labeled(
        &self,
        rule: &str,
        label_key: &str,
        label_value: &str,
        still_firing: &[String],
    ) -> Result<()> {
        let action = {
            let mut active = self.active.lock().unwrap();
            Self::retire(&mut active, |k, a| {
                a.rule == rule
                    && a.last.labels.get(label_key).map(String::as_str) == Some(label_value)
                    && !still_firing.iter().any(|s| s == k)
            })
        };
        self.apply("", action).await
    }

    /// Resolve every published alert under `rule` whose labels contain ALL
    /// of `labels` — the event-driven counterpart to the sweep-style
    /// [`reconcile`](Self::reconcile): a linkUp trap resolves exactly the
    /// linkDown alert(s) for that device+interface, nothing else.
    /// Returns the `alert_key`s actually resolved (empty when nothing
    /// matched), so a caller can name the alert it cleared (#651). Reporting
    /// what was resolved beats re-deriving it: a clear with no prior fire then
    /// links to nothing, rather than to a key that never existed.
    pub async fn resolve_matching(
        &self,
        rule: &str,
        labels: &[(&str, &str)],
    ) -> Result<Vec<String>> {
        let mut resolved = Vec::new();
        let action = {
            let mut active = self.active.lock().unwrap();
            let to_resolve: Vec<String> = active
                .iter()
                .filter(|(_, a)| {
                    a.rule == rule
                        && labels
                            .iter()
                            .all(|(k, v)| a.last.labels.get(*k).map(String::as_str) == Some(*v))
                })
                .map(|(k, _)| k.clone())
                .collect();
            let mut payloads = Vec::new();
            for k in to_resolve {
                // A matching entry still inside its debounce window is
                // dropped without a payload: the clear says the condition
                // is gone, and nothing was ever published to retract.
                if let Some(a) = active.remove(&k)
                    && a.published
                {
                    payloads.push(a.last.resolved());
                    resolved.push(k);
                }
            }
            if payloads.is_empty() {
                Action::None
            } else {
                Action::Resolve(payloads)
            }
        };
        self.apply("", action).await?;
        Ok(resolved)
    }

    /// Take ownership of the firing set a previous incarnation of this
    /// producer left on the bus (#882).
    ///
    /// One GET on [`Self::alert_selector`] — answered by a `latest` storage
    /// holding `v1/*/state/**`, and by nobody at all in a deployment without
    /// one. Every `Firing` document that comes back enters `active` already
    /// marked `published`, which is the truth: it *is* published, by us, at
    /// that key. From there the ordinary sweep finishes the job — the first
    /// [`reconcile`](Self::reconcile) of each rule retracts what is no longer
    /// violated, and re-[`observe`](Self::observe)ing what still is publishes
    /// nothing, because the key and severity are unchanged.
    ///
    /// Two documents get retired on the spot instead of adopted, because no
    /// sweep can ever reach them:
    ///
    /// - a `Resolved` document with no tombstone behind it — the retraction
    ///   landed but its `Delete` did not;
    /// - a document whose key does not match the `alert_key` this build derives
    ///   from its own payload — a phantom from an older derivation, which is
    ///   exactly the #737 re-key stranding that `RELEASING.md` otherwise asks
    ///   an operator to sweep by hand;
    /// - a document naming a rule this build no longer has, when the producer
    ///   declared its rule table with [`with_known_rules`](Self::with_known_rules).
    ///
    /// Call this **before** declaring [`serve_alerts_query`], so the only
    /// answers are the bus's rather than our own empty set. Returns how many
    /// alerts were adopted. Never fatal: a failed GET means starting with an
    /// empty firing set, which is today's behaviour.
    pub async fn adopt_persisted(&self, timeout: Duration) -> usize {
        let selector = self.alert_selector();
        let replies = match self
            .publisher
            .session()
            .get(&selector)
            .target(zenoh::query::QueryTarget::All)
            .timeout(timeout)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e, key = %selector,
                    "alert adoption GET failed; starting with an empty firing set"
                );
                return 0;
            }
        };

        let mut inherited: Vec<(String, Alert)> = Vec::new();
        while let Ok(reply) = replies.recv_async().await {
            let Ok(sample) = reply.result() else { continue };
            if sample.kind() == zenoh::sample::SampleKind::Delete {
                continue;
            }
            match zensight_common::decode_auto::<Alert>(&sample.payload().to_bytes()) {
                Ok(alert) => inherited.push((sample.key_expr().to_string(), alert)),
                Err(e) => tracing::warn!(
                    error = %e, key = %sample.key_expr(),
                    "alert adoption: undecodable document left at our own key"
                ),
            }
        }

        let mut adopted = 0usize;
        let mut retire: Vec<String> = Vec::new();
        {
            let mut active = self.active.lock().unwrap();
            let now = Instant::now();
            for (key_expr, alert) in inherited {
                let derived = self.alert_key_expr(&alert.alert_key());
                let retired_rule =
                    !self.known_rules.is_empty() && !self.known_rules.contains(&alert.rule);
                if alert.state != AlertState::Firing || derived != key_expr || retired_rule {
                    retire.push(key_expr);
                    continue;
                }
                // A live observation this process already made outranks the
                // inherited copy; adoption never overwrites present tense.
                let key = alert.alert_key();
                if active.contains_key(&key) {
                    continue;
                }
                active.insert(
                    key,
                    ActiveAlert {
                        rule: alert.rule.clone(),
                        severity: alert.severity,
                        first_seen: now,
                        last: alert,
                        published: true,
                    },
                );
                adopted += 1;
            }
        }

        for key in &retire {
            // The key the reply arrived on, verbatim — never a pattern built
            // from it (RFC 04 §1.2).
            if let Err(e) = self
                .publisher
                .delete(key, zensight_common::QosClass::Alert)
                .await
            {
                tracing::warn!(error = %e, key = %key, "alert adoption: tombstone failed");
            }
        }

        if adopted > 0 || !retire.is_empty() {
            tracing::info!(
                adopted,
                retired = retire.len(),
                key = %selector,
                "adopted the firing set left by a previous incarnation"
            );
        }
        adopted
    }

    /// Resolve and tombstone every active alert (graceful shutdown).
    pub async fn resolve_all(&self) -> Result<()> {
        let payloads = {
            let mut active = self.active.lock().unwrap();
            let p: Vec<Alert> = active
                .drain()
                .filter(|(_, a)| a.published)
                .map(|(_, a)| a.last.resolved())
                .collect();
            p
        };
        for alert in payloads {
            self.publish_state(&alert).await?;
            self.publisher
                .delete(
                    &self.alert_key_expr(&alert.alert_key()),
                    zensight_common::QosClass::Alert,
                )
                .await?;
        }
        Ok(())
    }

    /// Number of currently-firing (published) alerts — for sensor health/status.
    pub fn active_count(&self) -> usize {
        self.active
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.published)
            .count()
    }

    /// Every entry the reporter is tracking, published or still inside its
    /// `for:` window. The debounce bookkeeping must stay bounded by what is
    /// currently violated — this is the number that proves it.
    pub fn tracked_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    /// The current set of firing (published) alerts.
    ///
    /// Used to answer the `state alert selector` queryable so a late-joining consumer
    /// (a GUI opened *after* an alert fired) can seed its firing set — alerts are
    /// only published on state change, so without this seed a late joiner would
    /// never see an already-firing alert.
    pub fn firing_alerts(&self) -> Vec<Alert> {
        self.active
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.published)
            .map(|a| a.last.clone())
            .collect()
    }

    /// The protocol namespace this reporter publishes under.
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// A reference to the underlying publisher (for declaring the alerts query
    /// on the same session).
    pub fn publisher(&self) -> &Publisher {
        &self.publisher
    }

    async fn apply(&self, _key: &str, action: Action) -> Result<()> {
        match action {
            Action::None => Ok(()),
            Action::PublishFiring(alert) => self.publish_state(&alert).await,
            Action::Resolve(alerts) => {
                for alert in alerts {
                    self.publish_state(&alert).await?;
                    self.publisher
                        .delete(
                            &self.alert_key_expr(&alert.alert_key()),
                            zensight_common::QosClass::Alert,
                        )
                        .await?;
                }
                Ok(())
            }
        }
    }

    async fn publish_state(&self, alert: &Alert) -> Result<()> {
        let key = self.alert_key_expr(&alert.alert_key());
        let payload = encode(alert, self.format)
            .map_err(|e| crate::error::SensorError::Serialization(e.to_string()))?;
        self.publisher
            .publish_raw(
                &key,
                payload,
                zensight_common::QosClass::Alert,
                self.format.encoding(),
            )
            .await
    }
}

/// Serve the late-joiner seed for this producer's firing alerts — RFC 05 §4
/// style: not a bespoke procedure but a queryable on the **alert state
/// selector** (`state/<producer>/alert/*`), replying one sample per firing
/// alert on its concrete state key — exactly the answer a router latest-value
/// storage would give, so plain-GET seeding works with or without one (the
/// producer-side leg covers live producers; the storage covers crashed ones).
pub async fn serve_alerts_query(reporter: std::sync::Arc<AlertReporter>) {
    let session = reporter.publisher().session().clone();
    let selector = reporter.alert_selector();
    let queryable = match zensight_common::served::serve_state_queryable(&session, &selector).await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %selector, "failed to declare alert seed queryable");
            return;
        }
    };
    tracing::info!(key = %selector, "alert state seed ready");
    while let Ok(query) = queryable.recv_async().await {
        // The stamp is taken WITH the snapshot, not per reply (#782). Stamping
        // each reply as it goes out would let an alert that fires mid-loop have
        // its live `put` stamped earlier than this loop's stale copy of the
        // same key, and LWW would keep the stale one. Drawn from the same
        // session as every alert `put`, so the two are totally ordered.
        let (firing, stamp) = (
            reporter.firing_alerts(),
            zensight_common::served::seed_stamp(&session),
        );
        for alert in firing {
            let key = reporter.alert_key_expr(&alert.alert_key());
            // The seed must ride the same format as the live samples on the
            // key — a JSON seed under CBOR puts (or vice versa) is schema
            // drift a consumer can only see as a decode failure (#830).
            match encode(&alert, reporter.format) {
                Ok(payload) => {
                    // One reply per firing alert on its concrete state key —
                    // storage-shaped (RFC 05 §2.1 reply-key discipline), and
                    // stamped, because a storage's samples are (RFC 04 §3.2).
                    if let Err(e) = query.reply_state(&key, payload, stamp).await {
                        tracing::warn!(error = %e, "failed to reply alert seed");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to serialize alert"),
            }
        }
    }
}
