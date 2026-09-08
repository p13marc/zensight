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

/// Default minimum gap between content refreshes of one firing alert (#1081).
///
/// Sensors sweep every 5-60 s and the default debounce is `ZERO`, so without a
/// floor a drifting summary would put one document on the bus per sweep.
pub const DEFAULT_CONTENT_REFRESH: Duration = Duration::from_secs(30);

/// Internal state for a single tracked alert.
struct ActiveAlert {
    rule: String,
    severity: AlertSeverity,
    first_seen: Instant,
    /// **The payload that is on the bus right now** — the last `Put(Firing)`
    /// this reporter made for this key. Republished as `Resolved` on retire,
    /// and served verbatim by the late-joiner seed.
    ///
    /// Assigned only when we publish, never on a mere observation (#1081).
    /// It used to track the freshest *observation* instead, which had two
    /// consequences: the seed queryable — which stands in for a `latest`
    /// storage, and a storage answers with the last value **written** —
    /// answered with a document that had never been put; and a content change
    /// held back by the refresh interval was *lost* rather than deferred,
    /// because the next observation compared itself against the change it had
    /// already absorbed.
    last: Alert,
    /// Whether a `Put(Firing)` has actually been published yet (false while the
    /// `for:` debounce window is still open).
    published: bool,
    /// When [`Self::last`] went on the bus — the content-refresh rate
    /// limiter's clock (#1081). Any publish resets it, so an escalation and a
    /// refresh cannot stack into two puts a moment apart.
    last_published: Instant,
    /// When the condition was first seen clear, while a recovery window is
    /// open (#929). `None` means "currently violated" — which is also what a
    /// re-fire restores, because an alert that flickers clear and back was
    /// never really clear.
    clear_since: Option<Instant>,
}

/// Whether two payloads differ in anything a consumer renders — everything
/// except the two clocks (#1081).
///
/// Destructured rather than field-compared on purpose: a new [`Alert`] field
/// becomes a compile error here, instead of a field that silently never
/// refreshes.
///
/// Note what this *can* differ in. Labels are alert identity — `alert_key()`
/// hashes every one that is not host-scoped — so a discriminating label cannot
/// change without minting a different key, and a refresh is therefore usually a
/// `summary` change. But `host.*` labels are excluded from the derivation, and
/// `observe` stamps `host.id` from a `SharedIdentity` that can refresh
/// mid-run: a late-arriving or re-minted host id is a real content change on a
/// key that stays the same.
fn content_differs(on_bus: &Alert, observed: &Alert) -> bool {
    let Alert {
        timestamp: _,
        observed_at_ms: _,
        source,
        protocol,
        kind,
        rule,
        severity,
        state,
        summary,
        labels,
    } = on_bus;
    *source != observed.source
        || *protocol != observed.protocol
        || *kind != observed.kind
        || *rule != observed.rule
        || *severity != observed.severity
        || *state != observed.state
        || *summary != observed.summary
        || *labels != observed.labels
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
    /// How long a condition must stay clear before the alert resolves (#929).
    /// `ZERO` — the default — resolves on the first clear sweep, which is
    /// exactly the behaviour every caller had before this existed.
    recovery: Duration,
    /// Minimum gap between content refreshes of one firing alert (#1081).
    /// `ZERO` means no limit — every content change republishes.
    content_refresh: Duration,
    active: Mutex<HashMap<String, ActiveAlert>>,
}

/// Per-call overrides for one reconcile (#929).
///
/// A rule whose own hysteresis is not a timer — the sensor budget's 80/95/75
/// ratio band is the in-tree example — opts out here rather than inheriting the
/// reporter default, so a per-sensor `with_recovery` cannot silently stack a
/// delay on top of a band that already handles flapping.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileOpts {
    /// `Some(ZERO)` resolves immediately; `None` uses the reporter's default.
    pub recover_after: Option<Duration>,
}

impl ReconcileOpts {
    /// Resolve as soon as the condition clears, whatever the reporter default.
    pub fn immediate() -> Self {
        Self {
            recover_after: Some(Duration::ZERO),
        }
    }
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
            recovery: Duration::ZERO,
            content_refresh: DEFAULT_CONTENT_REFRESH,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Rate-limit content refreshes of a still-firing alert (#1081).
    ///
    /// Deliberately **not** the `for:` window. That window answers "how long
    /// before I believe it"; this one answers "how often may I correct the
    /// text", and they are unrelated — tying them would give a rule with
    /// `for: 1h` an hour-long refresh interval, so the summary would stay
    /// wrong longest on exactly the alerts that took longest to confirm. It is
    /// also per-*reporter* rather than per-entry, because netlink passes a
    /// per-expectation `for` and the same entry would otherwise get a
    /// different interval depending on which call site observed it last.
    ///
    /// `ZERO` means no limit, by analogy with [`Self::with_recovery`].
    ///
    /// The interval is a floor on incident-document churn too: the correlator
    /// hashes a representative member's `summary` into its incident content
    /// hash, so every refresh that changes a summary can move an incident
    /// document as well.
    pub fn with_content_refresh(mut self, d: Duration) -> Self {
        self.content_refresh = d;
        self
    }

    /// Set the default "must be violated continuously for" debounce window.
    pub fn with_debounce(mut self, d: Duration) -> Self {
        self.debounce = d;
        self
    }

    /// Set the default "must stay clear for" recovery window (#929).
    ///
    /// This is **time** hysteresis, and it is generic — which is why it lives
    /// here rather than in any one expectation kind. Value hysteresis (fire
    /// above 90, clear below 80) is numeric and belongs to the rule that knows
    /// what the number means.
    ///
    /// The default is `ZERO`, which is precisely today's behaviour: a
    /// condition that clears resolves on that sweep. Nothing changes for a
    /// caller that does not ask.
    ///
    /// **Cost of asking:** a cleared alert is held in `active` for up to this
    /// long instead of being dropped at once. That is bounded by the window
    /// and by the number of distinct alert keys, but it is not free — see the
    /// note on `retire` about what unbounded key minting does here.
    pub fn with_recovery(mut self, d: Duration) -> Self {
        self.recovery = d;
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
            let now_ms = zensight_common::current_timestamp_millis();
            let entry = active.entry(key.clone()).or_insert_with(|| ActiveAlert {
                rule: alert.rule.clone(),
                severity: alert.severity,
                first_seen: now,
                last: alert.clone(),
                published: false,
                last_published: now,
                clear_since: None,
            });
            Self::decide(entry, alert, now, now_ms, dur, self.content_refresh)
        };
        self.apply(&key, action).await
    }

    /// The synchronous decision for one observation: pure over the entry and
    /// the injected clock.
    ///
    /// Extracted for the reason [`Self::retire`] is — the windows here are
    /// measured in seconds, and a test that sleeps through one is a test
    /// nobody runs twice.
    ///
    /// The arms are ordered, and the order carries meaning:
    ///
    /// 1. **raise** — the debounce has elapsed and nothing is on the bus yet.
    /// 2. **escalate** — the severity moved. A real transition, so it takes a
    ///    fresh `timestamp` (which correctly un-acknowledges the alert) and is
    ///    never rate-limited: a Warning that became Critical should page now.
    /// 3. **refresh** (#1081) — same severity, different content, and the
    ///    refresh interval has passed. The alert never left `Firing`, so the
    ///    transition `timestamp` is **carried over unchanged** and the fresh
    ///    reading goes in `observed_at_ms`.
    fn decide(
        entry: &mut ActiveAlert,
        alert: Alert,
        now: Instant,
        now_ms: i64,
        dur: Duration,
        content_refresh: Duration,
    ) -> Action {
        entry.severity = alert.severity;
        // A re-fire inside the recovery window resets the clock and emits
        // NOTHING by itself: the alert never left `Firing`, so there is no
        // transition to publish. This is the whole anti-flap — a value
        // oscillating across the threshold produces one document on the bus,
        // not one per crossing (#929).
        entry.clear_since = None;

        // 1. Raise.
        if !entry.published {
            if now.duration_since(entry.first_seen) >= dur {
                return Self::publish(entry, alert, now);
            }
            // Still inside the debounce window. Nothing is on the bus, so
            // `last` must not move — it is what the bus holds, and the bus
            // holds nothing yet. The *next* raise publishes whatever is
            // observed then, which is the freshest reading by construction.
            return Action::None;
        }

        // 2. Escalate. Compared against what was published, not against a
        // field assigned one line earlier.
        if entry.last.severity != alert.severity {
            return Self::publish(entry, alert, now);
        }

        // 3. Refresh.
        if content_differs(&entry.last, &alert)
            && now.duration_since(entry.last_published) >= content_refresh
        {
            let mut refreshed = alert;
            refreshed.timestamp = entry.last.timestamp;
            refreshed.observed_at_ms = Some(now_ms);
            return Self::publish(entry, refreshed, now);
        }

        Action::None
    }

    /// Record `out` as the payload now on the bus, and emit it.
    ///
    /// Every publishing arm goes through here so [`ActiveAlert::last`] and the
    /// wire can never drift: the refresh arm publishes a *modified* copy, and
    /// storing the unmodified observation instead would leave `last.timestamp`
    /// at build time — so the next refresh would carry the wrong transition
    /// instant and the seed would serve a document that was never put.
    fn publish(entry: &mut ActiveAlert, out: Alert, now: Instant) -> Action {
        entry.published = true;
        entry.last_published = now;
        entry.last = out.clone();
        Action::PublishFiring(out)
    }

    /// After evaluating all violations for `rule` this sweep, resolve any
    /// previously-firing alert under that rule whose key is no longer in
    /// `still_firing`.
    pub async fn reconcile(&self, rule: &str, still_firing: &[String]) -> Result<()> {
        self.reconcile_opts(rule, still_firing, ReconcileOpts::default())
            .await
    }

    /// [`reconcile`](Self::reconcile) with a per-call recovery override (#929).
    pub async fn reconcile_opts(
        &self,
        rule: &str,
        still_firing: &[String],
        opts: ReconcileOpts,
    ) -> Result<()> {
        let recovery = opts.recover_after.unwrap_or(self.recovery);
        let action = {
            let mut active = self.active.lock().unwrap();
            let now = Instant::now();
            Self::retire(&mut active, now, recovery, |k, a| {
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
    /// With a non-zero `recovery` (#929) a published entry is **not dropped on
    /// the first clear sweep**: it is marked `clear_since` and held, resolving
    /// only once it has stayed clear for the window. [`Self::observe`] clears
    /// that mark, so a re-fire resets the clock and publishes nothing — the
    /// alert never left `Firing`.
    ///
    /// With `recovery == ZERO`, the default and what every caller had before
    /// this existed, the marking step is skipped and the behaviour is exactly
    /// what it was.
    fn retire(
        active: &mut HashMap<String, ActiveAlert>,
        now: Instant,
        recovery: Duration,
        no_longer_violated: impl Fn(&str, &ActiveAlert) -> bool,
    ) -> Action {
        let selected: Vec<String> = active
            .iter()
            .filter(|(k, a)| no_longer_violated(k, a))
            .map(|(k, _)| k.clone())
            .collect();
        let mut payloads = Vec::new();
        for k in selected {
            let Some(entry) = active.get_mut(&k) else {
                continue;
            };
            // An unpublished entry has nothing to retract and no window worth
            // waiting out — dropping it is what resets its debounce clock.
            if !entry.published {
                active.remove(&k);
                continue;
            }
            if recovery.is_zero() {
                if let Some(a) = active.remove(&k) {
                    payloads.push(a.last.resolved());
                }
                continue;
            }
            match entry.clear_since {
                // First clear sweep: start the clock, publish nothing. The
                // alert stays Firing on the bus, and truthfully so — the
                // condition has been gone for one sweep, not for the window.
                None => entry.clear_since = Some(now),
                Some(since) if now.duration_since(since) >= recovery => {
                    if let Some(a) = active.remove(&k) {
                        payloads.push(a.last.resolved());
                    }
                }
                // Still inside the window. Hold.
                Some(_) => {}
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
        self.reconcile_labeled_opts(
            rule,
            label_key,
            label_value,
            still_firing,
            ReconcileOpts::default(),
        )
        .await
    }

    /// [`reconcile_labeled`](Self::reconcile_labeled) with a per-call recovery
    /// override (#929).
    pub async fn reconcile_labeled_opts(
        &self,
        rule: &str,
        label_key: &str,
        label_value: &str,
        still_firing: &[String],
        opts: ReconcileOpts,
    ) -> Result<()> {
        let recovery = opts.recover_after.unwrap_or(self.recovery);
        let action = {
            let mut active = self.active.lock().unwrap();
            let now = Instant::now();
            Self::retire(&mut active, now, recovery, |k, a| {
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
    /// **Bypasses the recovery window** (#929), deliberately.
    ///
    /// This path is driven by an explicit clear *event* — a linkUp trap, a
    /// resolve notification — not by the absence of a violation in a sweep. A
    /// recovery window exists to distinguish "gone" from "gone for a moment",
    /// and an event that says the condition is over is not an absence of
    /// evidence. Holding it for a timer would delay a fact the device has
    /// already told us.
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
                        // Already the on-bus payload — it came off the bus.
                        // Nothing to reconstruct, which is the second dividend
                        // of `last` meaning "what was published" (#1081).
                        last: alert,
                        published: true,
                        // Deliberately `now`, not the inherited timestamp: a
                        // restart that adopts a large firing set must not
                        // republish all of it in its first sweep just because
                        // summaries drifted while it was down. The cost is one
                        // refresh interval of staleness after a restart.
                        last_published: now,
                        // Adopted because it is firing NOW, per the seed.
                        clear_since: None,
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
    /// Bypasses the recovery window (#929): a shutdown must leave nothing
    /// firing behind, and "wait and see whether it recovers" is not something
    /// a process that is exiting can offer.
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
    /// published only on a state change or a rate-limited content refresh
    /// (#1081), so without this seed a late joiner would never see an
    /// already-firing alert.
    ///
    /// It answers with the payloads that are **on the bus**, which is what a
    /// `latest` storage would do — see [`ActiveAlert::last`]. Serving the
    /// reporter's freshest private belief instead is how a page reload came to
    /// show a different number from the live subscription.
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

#[cfg(test)]
mod recovery_tests {
    //! The recovery-window state machine (#929), tested against `retire`
    //! directly with an injected clock.
    //!
    //! Injected rather than slept: the window is measured in seconds, and a
    //! test that sleeps through one is a test nobody runs twice.

    use super::*;
    use zensight_common::{AlertKind, AlertSeverity, Protocol};

    fn alert(rule: &str) -> Alert {
        Alert::new(
            "host1",
            Protocol::Sysinfo,
            AlertKind::Expectation,
            rule,
            AlertSeverity::Warning,
            "over".to_string(),
        )
    }

    fn firing(now: Instant, rule: &str) -> (String, ActiveAlert) {
        let a = alert(rule);
        (
            a.alert_key(),
            ActiveAlert {
                rule: rule.to_string(),
                severity: a.severity,
                first_seen: now,
                last: a,
                published: true,
                last_published: now,
                clear_since: None,
            },
        )
    }

    fn resolved_count(action: &Action) -> usize {
        match action {
            Action::Resolve(v) => v.len(),
            _ => 0,
        }
    }

    /// With no window configured, behaviour is byte-for-byte what it was: the
    /// first clear sweep resolves. Every existing caller depends on this.
    #[test]
    fn a_zero_window_resolves_on_the_first_clear_sweep() {
        let now = Instant::now();
        let mut active: HashMap<String, ActiveAlert> = [firing(now, "r")].into_iter().collect();
        let action = AlertReporter::retire(&mut active, now, Duration::ZERO, |_, a| a.rule == "r");
        assert_eq!(resolved_count(&action), 1);
        assert!(active.is_empty());
    }

    /// **The flap the window exists for.** Clear, then clear again inside the
    /// window: still nothing on the bus, and the entry is still held.
    #[test]
    fn a_clear_inside_the_window_publishes_nothing_and_holds_the_entry() {
        let t0 = Instant::now();
        let mut active: HashMap<String, ActiveAlert> = [firing(t0, "r")].into_iter().collect();
        let window = Duration::from_secs(30);

        let action = AlertReporter::retire(&mut active, t0, window, |_, a| a.rule == "r");
        assert_eq!(
            resolved_count(&action),
            0,
            "the first clear sweep says nothing"
        );
        assert_eq!(active.len(), 1, "and the alert is still Firing on the bus");
        assert!(active.values().next().unwrap().clear_since.is_some());

        let action =
            AlertReporter::retire(&mut active, t0 + Duration::from_secs(10), window, |_, a| {
                a.rule == "r"
            });
        assert_eq!(resolved_count(&action), 0, "still inside the window");
        assert_eq!(active.len(), 1);
    }

    /// …and once it has genuinely stayed clear for the window, it resolves.
    #[test]
    fn the_window_elapsing_resolves_exactly_once() {
        let t0 = Instant::now();
        let mut active: HashMap<String, ActiveAlert> = [firing(t0, "r")].into_iter().collect();
        let window = Duration::from_secs(30);

        AlertReporter::retire(&mut active, t0, window, |_, a| a.rule == "r");
        let action =
            AlertReporter::retire(&mut active, t0 + Duration::from_secs(30), window, |_, a| {
                a.rule == "r"
            });
        assert_eq!(resolved_count(&action), 1);
        assert!(active.is_empty(), "and nothing is left to resolve twice");
    }

    /// **The clock resets on a re-fire, and nothing is published** — the alert
    /// never left `Firing`, so there is no transition. A value oscillating
    /// across its threshold produces ONE document, not one per crossing.
    #[test]
    fn a_re_fire_inside_the_window_resets_the_clock_silently() {
        let t0 = Instant::now();
        let (key, entry) = firing(t0, "r");
        let mut active: HashMap<String, ActiveAlert> = [(key.clone(), entry)].into_iter().collect();
        let window = Duration::from_secs(30);

        // Clear at t0 — clock starts.
        AlertReporter::retire(&mut active, t0, window, |_, a| a.rule == "r");
        assert!(active[&key].clear_since.is_some());

        // Re-fire at t0+10. `observe` is what clears the mark; simulate the one
        // line it runs, since the rest of `observe` needs a bus.
        active.get_mut(&key).unwrap().clear_since = None;

        // Clear again at t0+20. Had the clock NOT reset, t0+20 would be inside
        // the original window but t0+40 would resolve; with the reset, the
        // window restarts here.
        AlertReporter::retire(&mut active, t0 + Duration::from_secs(20), window, |_, a| {
            a.rule == "r"
        });
        let action =
            AlertReporter::retire(&mut active, t0 + Duration::from_secs(45), window, |_, a| {
                a.rule == "r"
            });
        assert_eq!(
            resolved_count(&action),
            0,
            "45s after the first clear but only 25s after the re-fire — still held"
        );

        let action =
            AlertReporter::retire(&mut active, t0 + Duration::from_secs(51), window, |_, a| {
                a.rule == "r"
            });
        assert_eq!(resolved_count(&action), 1, "31s after the re-fire");
    }

    /// An entry still inside its `for:` debounce window has nothing to retract
    /// and no window worth waiting out — it is dropped at once, whatever the
    /// recovery setting, so its debounce clock resets.
    #[test]
    fn an_unpublished_entry_is_dropped_at_once_even_with_a_window() {
        let t0 = Instant::now();
        let (key, mut entry) = firing(t0, "r");
        entry.published = false;
        let mut active: HashMap<String, ActiveAlert> = [(key, entry)].into_iter().collect();

        let action = AlertReporter::retire(&mut active, t0, Duration::from_secs(30), |_, a| {
            a.rule == "r"
        });
        assert_eq!(resolved_count(&action), 0, "nothing was ever published");
        assert!(active.is_empty(), "and the debounce clock resets");
    }

    /// The window holds an entry for at most its own length — it does not turn
    /// a cleared alert into a permanent one. Selecting nothing leaves the
    /// marked entry alone, so a rule that stops being evaluated entirely keeps
    /// its alert rather than resolving it by silence.
    #[test]
    fn a_rule_that_is_not_reconciled_is_not_resolved_by_omission() {
        let t0 = Instant::now();
        let mut active: HashMap<String, ActiveAlert> = [firing(t0, "r")].into_iter().collect();
        let window = Duration::from_secs(30);
        AlertReporter::retire(&mut active, t0, window, |_, a| a.rule == "r");

        // A different rule's sweep must not touch it, even long after.
        let action = AlertReporter::retire(
            &mut active,
            t0 + Duration::from_secs(600),
            window,
            |_, a| a.rule == "other",
        );
        assert_eq!(resolved_count(&action), 0);
        assert_eq!(active.len(), 1);
    }

    /// Two alerts under one rule keep independent clocks — the proxy-sensor
    /// case, where one device clearing must not resolve another's.
    #[test]
    fn each_alert_keeps_its_own_recovery_clock() {
        let t0 = Instant::now();
        let a = alert("r").with_label("device", "one");
        let b = alert("r").with_label("device", "two");
        let mut active: HashMap<String, ActiveAlert> = [&a, &b]
            .into_iter()
            .map(|al| {
                (
                    al.alert_key(),
                    ActiveAlert {
                        rule: "r".to_string(),
                        severity: al.severity,
                        first_seen: t0,
                        last: al.clone(),
                        published: true,
                        last_published: t0,
                        clear_since: None,
                    },
                )
            })
            .collect();
        let window = Duration::from_secs(30);
        let a_key = a.alert_key();

        // Only `one` clears.
        AlertReporter::retire(&mut active, t0, window, |k, _| k == a_key);
        assert!(active[&a_key].clear_since.is_some());
        assert!(active[&b.alert_key()].clear_since.is_none());

        // …and only `one` resolves when its window elapses.
        let action =
            AlertReporter::retire(&mut active, t0 + Duration::from_secs(31), window, |k, _| {
                k == a_key
            });
        assert_eq!(resolved_count(&action), 1);
        assert_eq!(active.len(), 1, "the other device is untouched");
    }

    /// A per-call override beats the reporter default in both directions.
    #[test]
    fn the_per_call_override_wins_over_the_reporter_default() {
        assert_eq!(
            ReconcileOpts::immediate().recover_after,
            Some(Duration::ZERO)
        );
        assert_eq!(
            ReconcileOpts::default().recover_after,
            None,
            "the default defers to the reporter"
        );
    }
}

/// The content-refresh decision (#1081), tested against `decide` directly with
/// an injected clock — the same reason `recovery_tests` tests `retire` that way.
#[cfg(test)]
mod refresh_tests {
    use super::*;
    use zensight_common::{AlertKind, AlertSeverity, Protocol};

    const REFRESH: Duration = Duration::from_secs(30);

    fn alert_with(summary: &str, severity: AlertSeverity) -> Alert {
        Alert::new(
            "host1",
            Protocol::Sysinfo,
            AlertKind::SensorHealth,
            "sensor-budget",
            severity,
            summary.to_string(),
        )
    }

    fn alert(summary: &str) -> Alert {
        alert_with(summary, AlertSeverity::Warning)
    }

    /// Fire `first` at `t0` and return the entry holding it, as the bus does.
    fn fired(t0: Instant, first: &Alert) -> ActiveAlert {
        let mut entry = ActiveAlert {
            rule: first.rule.clone(),
            severity: first.severity,
            first_seen: t0,
            last: first.clone(),
            published: false,
            last_published: t0,
            clear_since: None,
        };
        let action = AlertReporter::decide(
            &mut entry,
            first.clone(),
            t0,
            1_000,
            Duration::ZERO,
            REFRESH,
        );
        assert!(matches!(action, Action::PublishFiring(_)), "did not fire");
        entry
    }

    fn published(action: &Action) -> Option<&Alert> {
        match action {
            Action::PublishFiring(a) => Some(a),
            _ => None,
        }
    }

    /// #1081: a firing alert whose summary moves inside one severity band is
    /// republished, once the refresh interval has passed.
    ///
    /// `sensor-budget` is the in-tree case: it fires at 80 % with "rss 320 MiB
    /// at 80 % of 400 MiB budget", RSS climbs to 94 % inside the same band, and
    /// the operator's row said 80 % until the severity finally changed.
    #[test]
    fn a_content_change_republishes_once_the_refresh_window_has_passed() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let mut entry = fired(t0, &first);

        // One second later: changed, but rate-limited.
        let held = AlertReporter::decide(
            &mut entry,
            alert("rss at 94%"),
            t0 + Duration::from_secs(1),
            2_000,
            Duration::ZERO,
            REFRESH,
        );
        assert!(published(&held).is_none(), "refreshed inside the interval");

        // Past the interval: it goes out.
        let out = AlertReporter::decide(
            &mut entry,
            alert("rss at 94%"),
            t0 + Duration::from_secs(31),
            3_000,
            Duration::ZERO,
            REFRESH,
        );
        let a = published(&out).expect("a changed summary must republish");
        assert_eq!(a.summary, "rss at 94%");
    }

    /// A rate-limited change is **deferred, not lost**.
    ///
    /// `entry.last` used to be overwritten on every observation, before any
    /// decision — so the held-back change became the comparison basis, the next
    /// observation found nothing different, and the bus kept the *first*
    /// summary forever. `last` is the payload on the bus now, assigned only
    /// when we publish.
    #[test]
    fn a_rate_limited_content_change_is_deferred_not_lost() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let mut entry = fired(t0, &first);

        for (secs, ms) in [(1u64, 2_000i64), (2, 3_000), (3, 4_000)] {
            let held = AlertReporter::decide(
                &mut entry,
                alert("rss at 94%"),
                t0 + Duration::from_secs(secs),
                ms,
                Duration::ZERO,
                REFRESH,
            );
            assert!(published(&held).is_none(), "published inside the interval");
        }

        // The condition has not changed again — it is still 94 % — and that is
        // the whole point: the refresh must still happen.
        let out = AlertReporter::decide(
            &mut entry,
            alert("rss at 94%"),
            t0 + Duration::from_secs(31),
            5_000,
            Duration::ZERO,
            REFRESH,
        );
        let a = published(&out).expect("the deferred change was lost");
        assert_eq!(a.summary, "rss at 94%");
    }

    /// A refresh is not a transition: `timestamp` is carried over and the
    /// fresh reading goes in `observed_at_ms`.
    ///
    /// An acknowledgement applies while `timestamp <= fired_at`, so a moving
    /// timestamp would un-acknowledge every acked alert on every refresh.
    #[test]
    fn a_refresh_does_not_move_the_transition_timestamp() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let fired_ts = first.timestamp;
        let mut entry = fired(t0, &first);

        // The injected wall clock tracks the real one the alert was built
        // with, so the >= invariant below is a statement about the code rather
        // than about the test's fixtures.
        let refreshed_at = fired_ts + 31_000;
        let out = AlertReporter::decide(
            &mut entry,
            alert("rss at 94%"),
            t0 + Duration::from_secs(31),
            refreshed_at,
            Duration::ZERO,
            REFRESH,
        );
        let a = published(&out).expect("refresh");
        assert_eq!(
            a.timestamp, fired_ts,
            "a refresh moved the transition clock"
        );
        assert_eq!(a.observed_at_ms, Some(refreshed_at));
        assert!(
            a.observed_at_ms.unwrap() >= a.timestamp,
            "observed_at_ms must never precede the transition it refreshes"
        );
    }

    /// An escalation *is* a transition: fresh timestamp, no `observed_at_ms`,
    /// and never rate-limited — a Warning that became Critical pages now.
    #[test]
    fn an_escalation_is_a_transition_and_is_never_rate_limited() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let fired_ts = first.timestamp;
        let mut entry = fired(t0, &first);

        let out = AlertReporter::decide(
            &mut entry,
            alert_with("rss at 96%", AlertSeverity::Critical),
            t0 + Duration::from_secs(1),
            2_000,
            Duration::ZERO,
            REFRESH,
        );
        let a = published(&out).expect("an escalation must publish immediately");
        assert_eq!(a.severity, AlertSeverity::Critical);
        assert_eq!(a.observed_at_ms, None, "an escalation is not a refresh");
        assert!(
            a.timestamp >= fired_ts,
            "an escalation takes a fresh transition clock, and so un-acks"
        );
    }

    /// An unchanged alert never republishes, however often it is observed.
    #[test]
    fn an_unchanged_alert_never_republishes() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let mut entry = fired(t0, &first);

        for i in 1..200u64 {
            let action = AlertReporter::decide(
                &mut entry,
                alert("rss at 80%"),
                t0 + Duration::from_secs(i * 30),
                1_000 + i as i64,
                Duration::ZERO,
                REFRESH,
            );
            assert!(published(&action).is_none(), "republished at sweep {i}");
        }
    }

    /// Any publish resets the refresh clock, so an escalation and a refresh
    /// cannot stack into two puts a moment apart.
    #[test]
    fn any_publish_resets_the_refresh_clock() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let mut entry = fired(t0, &first);

        let escalated = AlertReporter::decide(
            &mut entry,
            alert_with("rss at 96%", AlertSeverity::Critical),
            t0 + Duration::from_secs(20),
            2_000,
            Duration::ZERO,
            REFRESH,
        );
        assert!(published(&escalated).is_some());

        // 15 s after the escalation: past 30 s from the *raise*, but not from
        // the last publish.
        let held = AlertReporter::decide(
            &mut entry,
            alert_with("rss at 97%", AlertSeverity::Critical),
            t0 + Duration::from_secs(35),
            3_000,
            Duration::ZERO,
            REFRESH,
        );
        assert!(
            published(&held).is_none(),
            "the refresh clock was not reset by the escalation"
        );
    }

    /// A `host.*` label is excluded from `alert_key`, so it can change without
    /// re-keying — which makes it the one label change that is a *content*
    /// change rather than a different alert. A late-arriving or re-minted
    /// `host.id` used to diverge the bus copy silently.
    #[test]
    fn a_host_annotation_change_is_a_content_refresh() {
        let t0 = Instant::now();
        let first = alert("rss at 80%").with_label("host.id", "h-aaaaaaaaaaaa");
        let restamped = alert("rss at 80%").with_label("host.id", "h-bbbbbbbbbbbb");
        assert_eq!(
            first.alert_key(),
            restamped.alert_key(),
            "a host.* annotation must not re-key"
        );
        let mut entry = fired(t0, &first);

        let out = AlertReporter::decide(
            &mut entry,
            restamped,
            t0 + Duration::from_secs(31),
            4_000,
            Duration::ZERO,
            REFRESH,
        );
        let a = published(&out).expect("a re-stamped identity must refresh");
        assert_eq!(a.labels.get("host.id").unwrap(), "h-bbbbbbbbbbbb");
    }

    /// Nothing is on the bus during the debounce window, so `last` must not
    /// move: the raise publishes what is observed at the moment it fires.
    #[test]
    fn the_debounce_window_publishes_the_reading_it_fires_on() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let mut entry = ActiveAlert {
            rule: first.rule.clone(),
            severity: first.severity,
            first_seen: t0,
            last: first.clone(),
            published: false,
            last_published: t0,
            clear_since: None,
        };
        let held = AlertReporter::decide(
            &mut entry,
            first,
            t0,
            1_000,
            Duration::from_secs(60),
            REFRESH,
        );
        assert!(published(&held).is_none(), "fired inside the for: window");

        let out = AlertReporter::decide(
            &mut entry,
            alert("rss at 91%"),
            t0 + Duration::from_secs(61),
            2_000,
            Duration::from_secs(60),
            REFRESH,
        );
        let a = published(&out).expect("the debounce elapsed");
        assert_eq!(
            a.summary, "rss at 91%",
            "the raise published a stale reading"
        );
        assert_eq!(
            a.observed_at_ms, None,
            "a raise is a transition, not a refresh"
        );
    }

    /// A resolve carries the payload that was actually published — not a
    /// summary the bus never saw.
    #[test]
    fn a_resolve_carries_the_payload_that_was_published() {
        let t0 = Instant::now();
        let first = alert("rss at 80%");
        let key = first.alert_key();
        let mut entry = fired(t0, &first);

        // A change held back by the rate limiter.
        let _ = AlertReporter::decide(
            &mut entry,
            alert("rss at 94%"),
            t0 + Duration::from_secs(1),
            2_000,
            Duration::ZERO,
            REFRESH,
        );

        let mut active: HashMap<String, ActiveAlert> = HashMap::new();
        active.insert(key, entry);
        let action = AlertReporter::retire(
            &mut active,
            t0 + Duration::from_secs(2),
            Duration::ZERO,
            |_, _| true,
        );
        match action {
            Action::Resolve(alerts) => {
                assert_eq!(alerts.len(), 1);
                assert_eq!(
                    alerts[0].summary, "rss at 80%",
                    "the resolve retracted content that was never published"
                );
                assert_eq!(
                    alerts[0].observed_at_ms, None,
                    "a resolve is its own observation"
                );
            }
            _ => panic!("expected a resolve"),
        }
    }
}
