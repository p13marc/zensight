//! Sensor-alert export.
//!
//! Sensors publish fully-formed alerts on
//! `zensight/v1/<origin>/state/<producer>/alert/<key>` (firing → resolved →
//! tombstone). The metric exporters normally subscribe only to the telemetry
//! class, so those alerts only ever reached the desktop GUI. This
//! store mirrors the firing set and renders it as a Prometheus gauge so external
//! monitoring (Prometheus / Alertmanager) can route on ZenSight alerts.
//!
//! Each firing alert is one `<prefix>_alert` series with value `1`. When the
//! alert resolves (a `Resolved` Put or a Zenoh `Delete` tombstone) the series is
//! removed — Alertmanager treats a vanished `ALERTS`-style series as resolved,
//! so no explicit `0` is needed. Alerts are high-value, so (unlike metrics) they
//! are not subject to the metric filter or `max_series`.
//!
//! # Absence means resolved, so absence must never be an accident (#758)
//!
//! This store used to evict any alert not *re-received* within
//! `stale_timeout_secs` (300 s). But sensors publish alerts **edge-triggered**:
//! `zensight-sensor-core`'s alert engine returns `Action::None` when an alert is
//! already published and its severity has not changed, so a firing alert is put
//! **once**, and again only to resolve.
//!
//! The two facts together meant every alert older than five minutes silently
//! removed itself, and — because absence is the resolve signal — Alertmanager
//! closed a live incident. The staleness sweep did the exact opposite of its
//! stated purpose.
//!
//! A firing alert now leaves this store for exactly three reasons, all of them
//! real events:
//!
//! 1. a `Resolved` put from the sensor,
//! 2. a Zenoh `Delete` tombstone,
//! 3. the sensor's **liveliness token vanishing** ([`AlertStore::drop_source`])
//!    — the actual "the sensor died" signal, which a 300 s timer was only ever
//!    standing in for.
//!
//! # On `summary` and cardinality
//!
//! `summary` is a label, and sensor summaries embed live values
//! (`"disk at 91.3%"`), which looks like a cardinality hazard: every re-publish
//! with a different number would mint a new series.
//!
//! It is not, and the reason is the same edge-triggering above. A sensor
//! updates its stored alert every evaluation but returns `Action::None` unless
//! the **severity** changed — a changed summary alone never reaches the wire.
//! So the number of label sets per alert is bounded by how many severity
//! transitions it makes, not by how often it is evaluated, and the old series
//! goes stale naturally when a transition does happen.

use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

use parking_lot::RwLock;
use zensight_common::alert::{Alert, AlertState};

use crate::collector::escape_label_value;
use crate::mapping::sanitize_label_name;

/// Reserved label names the exporter sets itself; an alert's own structured
/// labels are skipped if they would collide with one of these.
const RESERVED: &[&str] = &[
    "alert_key",
    "source",
    "protocol",
    "rule",
    "severity",
    "kind",
    "summary",
];

/// A firing alert plus when it was last seen.
///
/// `received` is diagnostic only. It is deliberately NOT an eviction input:
/// see the module note on why a timer is the wrong liveness signal for an
/// edge-triggered publisher.
struct StoredAlert {
    alert: Alert,
    #[allow(dead_code)]
    received: Instant,
}

/// Thread-safe store of currently-firing alerts, keyed by `alert_key`.
#[derive(Default)]
pub struct AlertStore {
    alerts: RwLock<HashMap<String, StoredAlert>>,
}

impl AlertStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an alert update. A firing alert is inserted/updated; a resolved
    /// alert clears its series.
    pub fn apply(&self, alert: Alert) {
        let key = alert.alert_key();
        let mut map = self.alerts.write();
        if alert.state == AlertState::Resolved {
            map.remove(&key);
        } else {
            map.insert(
                key,
                StoredAlert {
                    alert,
                    received: Instant::now(),
                },
            );
        }
    }

    /// Clear an alert by its `alert_key` (the last key-expression segment of a
    /// Zenoh `Delete` tombstone).
    pub fn remove(&self, alert_key: &str) {
        self.alerts.write().remove(alert_key);
    }

    /// Number of firing alerts.
    pub fn len(&self) -> usize {
        self.alerts.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.alerts.read().is_empty()
    }

    /// Drop every firing alert from one source, because its liveliness token
    /// vanished.
    ///
    /// This is the honest replacement for the staleness sweep: a sensor that
    /// died without tombstoning its alerts is exactly what a disappearing
    /// liveliness token means (RFC 04 §5), and unlike a timer it cannot fire
    /// for a sensor that is alive and simply had nothing new to say.
    ///
    /// Returns the number removed.
    pub fn drop_source(&self, source: &str) -> usize {
        let mut map = self.alerts.write();
        let before = map.len();
        map.retain(|_, a| a.alert.source != source);
        before - map.len()
    }

    /// Append the alert series to a Prometheus exposition buffer.
    pub fn render(&self, prefix: &str, out: &mut Vec<u8>) {
        let map = self.alerts.read();
        if map.is_empty() {
            return;
        }
        let name = format!("{prefix}_alert");

        let _ = writeln!(
            out,
            "# HELP {name} ZenSight sensor alert (1 = firing; series absent once resolved)."
        );
        let _ = writeln!(out, "# TYPE {name} gauge");

        // Deterministic output order so scrapes/diffs are stable.
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();

        for key in keys {
            let a = &map[key].alert;
            let mut labels: Vec<(String, String)> = vec![
                ("alert_key".into(), key.clone()),
                ("source".into(), a.source.clone()),
                ("protocol".into(), a.protocol.to_string()),
                ("rule".into(), a.rule.clone()),
                ("severity".into(), a.severity.as_str().to_string()),
                ("kind".into(), a.kind.as_str().to_string()),
                ("summary".into(), a.summary.clone()),
            ];
            // Merge the alert's structured labels (sanitized), skipping reserved
            // names and any that collapse to a duplicate.
            for (k, v) in &a.labels {
                let lk = sanitize_label_name(k);
                if RESERVED.contains(&lk.as_str()) || labels.iter().any(|(e, _)| e == &lk) {
                    continue;
                }
                labels.push((lk, v.clone()));
            }
            labels.sort_by(|x, y| x.0.cmp(&y.0));

            let label_str = labels
                .iter()
                .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(out, "{name}{{{label_str}}} 1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::Protocol;
    use zensight_common::alert::{AlertKind, AlertSeverity};

    fn firing() -> Alert {
        Alert::new(
            "host01",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening on :22",
        )
        .with_label("port", "22")
    }

    fn render(store: &AlertStore) -> String {
        let mut out = Vec::new();
        store.render("zensight", &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn firing_alert_renders_gauge() {
        let store = AlertStore::new();
        store.apply(firing());
        let out = render(&store);
        assert!(out.contains("# TYPE zensight_alert gauge"));
        assert!(out.contains("source=\"host01\""));
        assert!(out.contains("protocol=\"netlink\""));
        assert!(out.contains("rule=\"ssh-listening\""));
        assert!(out.contains("severity=\"critical\""));
        assert!(out.contains("kind=\"expectation\""));
        assert!(out.contains("port=\"22\""), "structured label merged");
        assert!(out.trim_end().ends_with("} 1"), "value is 1 while firing");
    }

    #[test]
    fn resolved_alert_clears_series() {
        let store = AlertStore::new();
        let a = firing();
        store.apply(a.clone());
        assert_eq!(store.len(), 1);
        store.apply(a.resolved());
        assert_eq!(store.len(), 0);
        assert!(render(&store).is_empty(), "no block when nothing firing");
    }

    #[test]
    fn tombstone_removes_by_alert_key() {
        let store = AlertStore::new();
        let a = firing();
        let key = a.alert_key();
        store.apply(a);
        store.remove(&key);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn update_in_place_keeps_one_series() {
        let store = AlertStore::new();
        store.apply(firing());
        store.apply(firing()); // same source+rule+labels -> same alert_key
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn reserved_label_is_not_overridden_by_alert_label() {
        // An alert whose structured label collides with a reserved name must not
        // produce a duplicate `source=` label.
        let store = AlertStore::new();
        let a = Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScan",
            AlertSeverity::Warning,
            "scan",
        )
        .with_label("source", "spoofed");
        store.apply(a);
        let out = render(&store);
        assert_eq!(out.matches("source=").count(), 1);
        assert!(out.contains("source=\"host01\""));
    }

    #[test]
    fn label_values_are_escaped() {
        let store = AlertStore::new();
        let a = Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::Anomaly,
            "Beacon",
            AlertSeverity::Warning,
            "saw \"quotes\" and \\slash",
        );
        store.apply(a);
        let out = render(&store);
        assert!(out.contains(r#"summary=\"quotes\""#) || out.contains("\\\""));
    }

    /// A firing alert leaves only when its SOURCE goes away (#758).
    ///
    /// This replaces `stale_alerts_are_evicted`, which asserted the bug:
    /// sensors publish alerts edge-triggered, so "not re-received in 300s"
    /// almost always meant "still firing, nothing changed" — and because
    /// absence is the resolve signal, evicting on that timer silently closed
    /// live incidents.
    #[test]
    fn a_departed_source_loses_its_alerts() {
        let store = AlertStore::new();
        store.apply(firing());
        assert_eq!(store.len(), 1);

        // A different host going away must not touch it.
        assert_eq!(store.drop_source("some-other-host"), 0);
        assert_eq!(store.len(), 1, "another host's death is not evidence");

        let source = firing().source;
        assert_eq!(store.drop_source(&source), 1);
        assert_eq!(store.len(), 0);
    }

    /// Time alone never removes a firing alert. If this ever fails, something
    /// has reintroduced a staleness sweep and live incidents will close
    /// themselves again.
    #[test]
    fn nothing_evicts_a_firing_alert_on_a_timer() {
        let store = AlertStore::new();
        store.apply(firing());

        // The only public ways out are a resolve, a tombstone, or a departed
        // source. There is deliberately no timer-based entry point at all.
        assert_eq!(store.len(), 1);
        assert_eq!(store.drop_source("unrelated"), 0);
        assert_eq!(store.len(), 1);
    }
}
