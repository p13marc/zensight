//! Sensor-emitted alerts.
//!
//! Unlike the frontend's local threshold-rule alerts (which only evaluate while
//! the desktop app is open), an [`Alert`] is a *durable*, fully-formed decision
//! made by a sensor or the sentinel: the sensor already determined something is
//! wrong (a port scan, a missing listener, a downed interface) and publishes the
//! alert on the bus. This is the wire type for that channel.
//!
//! Alerts are published, keyed by [`Alert::alert_key`], at
//! `zensight/<protocol>/@/alerts/<alert_key>`:
//! - a `Put` with [`AlertState::Firing`] raises or updates an alert,
//! - a `Put` with [`AlertState::Resolved`] (then a Zenoh `Delete` tombstone)
//!   clears it.
//!
//! High-cardinality detail (offending IP, domain, JA4, expected/actual values)
//! belongs in [`Alert::labels`] / [`Alert::summary`], never in a metric series
//! name — keep the `alert_key` bucketed so a 1000-port scan is one alert.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::Protocol;
use crate::telemetry::current_timestamp_millis;

/// Severity of a sensor-emitted alert.
///
/// Plain (no `iced` dependency) so it lives in `zensight-common`; the frontend
/// maps it 1:1 onto its display `Severity`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, Hash,
)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    #[default]
    Warning,
    Critical,
}

impl AlertSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertSeverity::Info => "info",
            AlertSeverity::Warning => "warning",
            AlertSeverity::Critical => "critical",
        }
    }
}

impl std::fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// What produced the alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// Pillar A — a netring detector (port scan, beacon, DGA, ...).
    Anomaly,
    /// Pillar B — an expectation about machine state was violated.
    Expectation,
    /// The sensor's own health (e.g. capture drop-rate) crossed a threshold.
    SensorHealth,
}

impl AlertKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertKind::Anomaly => "anomaly",
            AlertKind::Expectation => "expectation",
            AlertKind::SensorHealth => "sensor_health",
        }
    }
}

/// Firing vs resolved. Drives auto-clear in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    #[default]
    Firing,
    Resolved,
}

/// A fully-formed, sensor-decided alert. The wire type published on
/// `zensight/<protocol>/@/alerts/<alert_key>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Alert {
    /// Unix epoch millis of the latest state transition.
    pub timestamp: i64,
    /// Host / sensor identifier (the same value used as `source` in telemetry).
    pub source: String,
    /// Namespace the alert lives under (`netlink` for expectations, `netring`
    /// for anomalies). Also the `<protocol>` key segment.
    pub protocol: Protocol,
    pub kind: AlertKind,
    /// Stable rule identifier, e.g. "ssh-listening" or "PortScanDetector".
    pub rule: String,
    pub severity: AlertSeverity,
    #[serde(default)]
    pub state: AlertState,
    /// Human-readable one-liner for the alert row / toast.
    pub summary: String,
    /// Structured context (ip, port, peer, sni, expected, actual, ...).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl Alert {
    /// Create a new firing alert with the current timestamp.
    pub fn new(
        source: impl Into<String>,
        protocol: Protocol,
        kind: AlertKind,
        rule: impl Into<String>,
        severity: AlertSeverity,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: current_timestamp_millis(),
            source: source.into(),
            protocol,
            kind,
            rule: rule.into(),
            severity,
            state: AlertState::Firing,
            summary: summary.into(),
            labels: HashMap::new(),
        }
    }

    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels.extend(labels);
        self
    }

    /// Mark this alert resolved (state transition; timestamp refreshed).
    pub fn resolved(mut self) -> Self {
        self.state = AlertState::Resolved;
        self.timestamp = current_timestamp_millis();
        self
    }

    pub fn is_firing(&self) -> bool {
        self.state == AlertState::Firing
    }

    /// Stable key for this logical alert: derived from `source` + `rule` +
    /// sorted labels.
    ///
    /// Two alerts that describe the same underlying condition on the same host
    /// (same source, rule, labels) share a key, so a `Put` replaces the prior
    /// state in place and a later `Resolved`/`Delete` clears exactly that alert.
    /// Including `source` keeps alerts from different hosts on distinct keys (no
    /// cross-host collisions). The key is stable under label reordering (labels
    /// are sorted before hashing).
    pub fn alert_key(&self) -> String {
        let mut hasher = Fnv1a::new();
        hasher.update(self.source.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.rule.as_bytes());
        hasher.update(b"\0");
        // BTreeMap iterates in sorted key order → deterministic.
        let sorted: BTreeMap<&String, &String> = self.labels.iter().collect();
        for (k, v) in sorted {
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
            hasher.update(b"\0");
        }
        format!("{}-{:016x}", sanitize_segment(&self.rule), hasher.finish())
    }
}

/// Replace characters that are not safe in a single key-expression segment.
fn sanitize_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Tiny FNV-1a 64-bit hasher — stable across runs/platforms (unlike
/// `DefaultHasher`), so the same alert always yields the same `alert_key`.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_key_stable_under_label_reordering() {
        let a = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening",
        )
        .with_label("port", "22")
        .with_label("expected", "listen");
        let b = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening",
        )
        .with_label("expected", "listen")
        .with_label("port", "22");
        assert_eq!(a.alert_key(), b.alert_key());
    }

    #[test]
    fn alert_key_differs_by_rule_and_labels() {
        let base = Alert::new(
            "h",
            Protocol::Netring,
            AlertKind::Anomaly,
            "port_scan",
            AlertSeverity::Warning,
            "scan",
        );
        let with_src = base.clone().with_label("src", "10.0.0.5");
        let with_other = base.clone().with_label("src", "10.0.0.6");
        assert_ne!(base.alert_key(), with_src.alert_key());
        assert_ne!(with_src.alert_key(), with_other.alert_key());
    }

    #[test]
    fn resolved_transition() {
        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "r",
            AlertSeverity::Info,
            "s",
        );
        assert!(a.is_firing());
        let r = a.resolved();
        assert_eq!(r.state, AlertState::Resolved);
        assert!(!r.is_firing());
    }

    #[test]
    fn serde_roundtrip_json_and_cbor() {
        let a = Alert::new(
            "host1",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScanDetector",
            AlertSeverity::Critical,
            "Port scan from 10.0.0.5 (37 ports)",
        )
        .with_label("src", "10.0.0.5");
        let json = crate::encode(&a, crate::Format::Json).unwrap();
        let back: Alert = crate::decode(&json, crate::Format::Json).unwrap();
        assert_eq!(a, back);
        let cbor = crate::encode(&a, crate::Format::Cbor).unwrap();
        let back2: Alert = crate::decode(&cbor, crate::Format::Cbor).unwrap();
        assert_eq!(a, back2);
    }

    #[test]
    fn alert_key_is_key_expr_safe() {
        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd/22",
            AlertSeverity::Warning,
            "s",
        );
        let key = a.alert_key();
        assert!(!key.contains('/'));
        assert!(!key.contains('*'));
    }
}
