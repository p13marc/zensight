//! Alert lifecycle management and publishing.
//!
//! [`AlertReporter`] is the sensor-side counterpart to
//! [`zensight_common::Alert`]: it owns a [`Publisher`], tracks which alerts are
//! currently firing, applies a "must be violated continuously for N" debounce,
//! and publishes firing/resolved transitions to
//! `zensight/<protocol>/@/alerts/<alert_key>` (a `Put` to raise/update, a `Put`
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
    active: Mutex<HashMap<String, ActiveAlert>>,
}

impl AlertReporter {
    /// Create a reporter. `publisher`'s session is used to publish to
    /// `zensight/<protocol>/@/alerts/<key>`; the publisher prefix is ignored for
    /// alert keys (we build the full key from `protocol`).
    pub fn new(publisher: Publisher, protocol: Protocol, format: Format) -> Self {
        Self {
            publisher,
            protocol,
            format,
            debounce: Duration::ZERO,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Set the default "must be violated continuously for" debounce window.
    pub fn with_debounce(mut self, d: Duration) -> Self {
        self.debounce = d;
        self
    }

    fn alert_key_expr(&self, alert_key: &str) -> String {
        format!("zensight/{}/@/alerts/{}", self.protocol.as_str(), alert_key)
    }

    /// Report that `alert` is currently violated. Publishes a `Put(Firing)` once
    /// the alert has been continuously observed for `for_duration` (or the
    /// reporter default). Idempotent within the debounce window; re-publishes if
    /// the severity escalates after firing.
    pub async fn observe(&self, alert: Alert, for_duration: Option<Duration>) -> Result<()> {
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
                .delete(&self.alert_key_expr(&alert.alert_key()))
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

    async fn apply(&self, _key: &str, action: Action) -> Result<()> {
        match action {
            Action::None => Ok(()),
            Action::PublishFiring(alert) => self.publish_state(&alert).await,
            Action::Resolve(alerts) => {
                for alert in alerts {
                    self.publish_state(&alert).await?;
                    self.publisher
                        .delete(&self.alert_key_expr(&alert.alert_key()))
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
        self.publisher.publish_raw(&key, payload).await
    }
}
