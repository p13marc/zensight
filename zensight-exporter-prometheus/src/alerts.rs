//! Sensor-alert export.
//!
//! Sensors publish fully-formed alerts on `zensight/<protocol>/@/alerts/<key>`
//! (firing → resolved → tombstone). The metric exporters normally drop the
//! `@/` control plane, so those alerts only ever reached the desktop GUI. This
//! store mirrors the firing set and renders it as a Prometheus gauge so external
//! monitoring (Prometheus / Alertmanager) can route on ZenSight alerts.
//!
//! Each firing alert is one `<prefix>_alert` series with value `1`. When the
//! alert resolves (a `Resolved` Put or a Zenoh `Delete` tombstone) the series is
//! removed — Alertmanager treats a vanished `ALERTS`-style series as resolved,
//! so no explicit `0` is needed. Alerts are low-cardinality and high-value, so
//! (unlike metrics) they are not subject to the metric filter or `max_series`.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

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

/// A firing alert plus when it was last seen (for staleness eviction).
struct StoredAlert {
    alert: Alert,
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

    /// Evict alerts not refreshed within `timeout` (a sensor that died without
    /// tombstoning its alerts shouldn't leave them firing forever). Returns the
    /// number removed.
    pub fn cleanup_stale(&self, timeout: Duration) -> usize {
        let mut map = self.alerts.write();
        let before = map.len();
        map.retain(|_, a| a.received.elapsed() < timeout);
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

    #[test]
    fn stale_alerts_are_evicted() {
        let store = AlertStore::new();
        store.apply(firing());
        assert_eq!(store.cleanup_stale(Duration::from_secs(3600)), 0);
        assert_eq!(store.cleanup_stale(Duration::ZERO), 1);
        assert_eq!(store.len(), 0);
    }
}
