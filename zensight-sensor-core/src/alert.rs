//! Alert lifecycle management and publishing.
//!
//! [`AlertReporter`] is the sensor-side counterpart to
//! [`zensight_common::Alert`]: it owns a [`Publisher`], tracks which alerts are
//! currently firing, applies a "must be violated continuously for N" debounce,
//! and publishes firing/resolved transitions to the v1 state key
//! `<base>/@v1/<origin>/state/<producer>/alert/<alert_key>` (a `Put` to raise/update, a `Put`
//! with state `Resolved` followed by a `Delete` tombstone to clear).
//!
//! Usage from an evaluator sweep:
//! ```ignore
//! // Each violation this tick:
//! reporter.observe(alert, exp.for_duration()).await?;
//! // After evaluating a rule, resolve anything that's no longer violated:
//! reporter.reconcile(rule, &still_firing_keys).await?;
//! ```

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zensight_common::{Alert, AlertSeverity, Format, Protocol, encode};

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
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Set the default "must be violated continuously for" debounce window.
    pub fn with_debounce(mut self, d: Duration) -> Self {
        self.debounce = d;
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

    fn alert_key_expr(&self, alert_key: &str) -> String {
        // v1 (RFC 04 §1.2): alerts are LWW state under the producer, keyed by
        // the origin — the legacy protocol-shared channel is gone.
        self.publisher.v1().state_key(&["alert", alert_key])
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
            let to_resolve: Vec<String> = active
                .iter()
                .filter(|(k, a)| a.rule == rule && a.published && !still_firing.contains(k))
                .map(|(k, _)| k.clone())
                .collect();
            let mut payloads = Vec::new();
            for k in to_resolve {
                if let Some(a) = active.remove(&k) {
                    payloads.push(a.last.resolved());
                }
            }
            if payloads.is_empty() {
                Action::None
            } else {
                Action::Resolve(payloads)
            }
        };
        // `apply` keys off the alert itself for Resolve; key arg unused there.
        self.apply("", action).await
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
            .publish_raw(&key, payload, zensight_common::QosClass::Alert)
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
    let selector = format!("{}/*", reporter.publisher().v1().state_key(&["alert"]));
    let queryable = match session.declare_queryable(&selector).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %selector, "failed to declare alert seed queryable");
            return;
        }
    };
    tracing::info!(key = %selector, "alert state seed ready");
    while let Ok(query) = queryable.recv_async().await {
        let firing = reporter.firing_alerts();
        for alert in firing {
            let key = reporter.alert_key_expr(&alert.alert_key());
            match serde_json::to_vec(&alert) {
                Ok(payload) => {
                    // One reply per firing alert on its concrete state key —
                    // storage-shaped (RFC 05 §2.1 reply-key discipline).
                    if let Err(e) = query.reply(key, payload).await {
                        tracing::warn!(error = %e, "failed to reply alert seed");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to serialize alert"),
            }
        }
    }
}
