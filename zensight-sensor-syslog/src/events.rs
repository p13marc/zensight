//! Known systemd-event detection → alerts (#61).
//!
//! systemd journal entries for well-known events carry a stable `MESSAGE_ID`
//! (catalog UUID). This module recognizes the highest-signal ones (coredump,
//! unit failure, OOM) and raises structured alerts on
//! `zensight/syslog/@/alerts/*` via the shared [`AlertReporter`].
//!
//! These are *point* events, not ongoing conditions: each is fired once
//! (immediately) and auto-resolved after `event_dedup_secs` so it shows up as a
//! brief incident and bursts of the same `(event, unit)` are coalesced.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::telemetry::Protocol;
use zensight_sensor_core::AlertReporter;

use crate::parser::SyslogMessage;

/// Single rule namespace for all journald events (reconciled together).
const RULE: &str = "journald-event";
/// How often expired event alerts are auto-resolved.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// Max length of the message excerpt placed in the alert summary.
const SUMMARY_MAX: usize = 160;

/// A recognized systemd journal event.
struct KnownEvent {
    name: &'static str,
    severity: AlertSeverity,
}

/// Map a `MESSAGE_ID` (32-char hex) to a known event, or `None`.
///
/// IDs verified against systemd's `catalog/systemd.catalog.in`.
/// `process-exited` (`98e32220…`) is intentionally omitted: it fires on every
/// normal service stop and would be pure noise; `unit-failed` covers failures.
fn known_event(message_id: &str) -> Option<KnownEvent> {
    let (name, severity) = match message_id.trim().to_ascii_lowercase().as_str() {
        "fc2e22bc6ee647b6b90729ab34a250b1" => ("coredump", AlertSeverity::Critical),
        "d9b373ed55a64feb8242e02dbe79a49c" => ("unit-failed", AlertSeverity::Warning),
        "d989611b15e44c9dbf31e3c81256e4ed" => ("oomd-kill", AlertSeverity::Critical),
        "fe6faa94e7774663a0da52717891d8ef" => ("kernel-oom", AlertSeverity::Critical),
        _ => return None,
    };
    Some(KnownEvent { name, severity })
}

/// Known-event category name for a `MESSAGE_ID`, or `None` when unrecognized.
///
/// Reuses the same catalog as the alert path so the aggregated `LogEvent.category`
/// stays consistent with the alerts the sensor raises. Feature-gated: only the
/// `aggregate-publishers` build needs it. `pub` (not `pub(crate)`) so the lib
/// target sees it as reachable public API — it is consumed by the binary.
#[cfg(feature = "aggregate-publishers")]
pub fn known_event_category(message_id: &str) -> Option<&'static str> {
    known_event(message_id).map(|e| e.name)
}

/// Parse a config severity override.
fn parse_severity(s: &str) -> Option<AlertSeverity> {
    match s.trim().to_ascii_lowercase().as_str() {
        "info" => Some(AlertSeverity::Info),
        "warning" | "warn" => Some(AlertSeverity::Warning),
        "critical" | "crit" => Some(AlertSeverity::Critical),
        _ => None,
    }
}

/// Truncate `s` to `max` chars (with an ellipsis) for the alert summary.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Build the alert for a known-event message, or `None` if it isn't one.
///
/// Pure (no reporter / no clock) so it is unit-testable.
fn detect_alert(
    msg: &SyslogMessage,
    source: &str,
    overrides: &HashMap<String, AlertSeverity>,
) -> Option<Alert> {
    let id = msg.msg_id.as_deref()?.trim().to_ascii_lowercase();
    let event = known_event(&id)?;
    let severity = overrides.get(&id).copied().unwrap_or(event.severity);

    let mut alert = Alert::new(
        source.to_string(),
        Protocol::Syslog,
        AlertKind::Anomaly,
        RULE,
        severity,
        format!("{}: {}", event.name, truncate(&msg.message, SUMMARY_MAX)),
    )
    .with_label("event", event.name)
    .with_label("message_id", id);

    if let Some(unit) = msg
        .structured_data
        .get("journald")
        .and_then(|m| m.get("unit"))
    {
        alert = alert.with_label("unit", unit.clone());
    }
    if let Some(app) = &msg.app_name {
        alert = alert.with_label("app", app.clone());
    }
    Some(alert)
}

/// Decide whether to fire `key` now, recording its dedup expiry if so.
///
/// Pure (operates on the passed map) so the dedup window is unit-testable.
fn should_fire(
    active: &mut HashMap<String, Instant>,
    key: &str,
    now: Instant,
    window: Duration,
) -> bool {
    match active.get(key) {
        Some(expiry) if *expiry > now => false, // still within the dedup window
        _ => {
            active.insert(key.to_string(), now + window);
            true
        }
    }
}

/// Detects known systemd events and raises one-shot, auto-resolving alerts.
pub struct EventDetector {
    reporter: Arc<AlertReporter>,
    window: Duration,
    severity_overrides: HashMap<String, AlertSeverity>,
    /// alert_key → expiry (dedup + auto-resolve bookkeeping).
    active: Mutex<HashMap<String, Instant>>,
}

impl EventDetector {
    /// Build a detector. `overrides_cfg` maps `MESSAGE_ID` → severity name.
    pub fn new(
        reporter: Arc<AlertReporter>,
        dedup_secs: u64,
        overrides_cfg: &HashMap<String, String>,
    ) -> Self {
        let severity_overrides = overrides_cfg
            .iter()
            .filter_map(|(id, sev)| {
                parse_severity(sev).map(|s| (id.trim().to_ascii_lowercase(), s))
            })
            .collect();
        Self {
            reporter,
            window: Duration::from_secs(dedup_secs.max(1)),
            severity_overrides,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Inspect a message; fire an alert if it's a known event not already seen
    /// within the dedup window. No-op for ordinary messages.
    pub async fn on_message(&self, msg: &SyslogMessage, source: &str) {
        let Some(alert) = detect_alert(msg, source, &self.severity_overrides) else {
            return;
        };
        let key = alert.alert_key();
        let fire = {
            let mut active = self.active.lock().unwrap();
            should_fire(&mut active, &key, Instant::now(), self.window)
        };
        if fire {
            tracing::info!(alert = %key, "journald: firing known-event alert");
            if let Err(e) = self.reporter.observe(alert, Some(Duration::ZERO)).await {
                tracing::warn!(error = %e, "journald: failed to publish event alert");
            }
        }
    }

    /// Periodically resolve event alerts whose dedup window has expired.
    pub async fn run_reconcile_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(RECONCILE_INTERVAL).await;
            let now = Instant::now();
            let still_active: Vec<String> = {
                let mut active = self.active.lock().unwrap();
                active.retain(|_, expiry| *expiry > now);
                active.keys().cloned().collect()
            };
            if let Err(e) = self.reporter.reconcile(RULE, &still_active).await {
                tracing::warn!(error = %e, "journald: event alert reconcile failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Facility, Severity, SyslogVersion};

    fn msg_with(message_id: Option<&str>, unit: Option<&str>) -> SyslogMessage {
        let mut structured_data = HashMap::new();
        if let Some(u) = unit {
            let mut jd = HashMap::new();
            jd.insert("unit".to_string(), u.to_string());
            structured_data.insert("journald".to_string(), jd);
        }
        SyslogMessage {
            facility: Facility::Daemon,
            severity: Severity::Critical,
            timestamp: None,
            hostname: Some("host1".into()),
            app_name: Some("systemd".into()),
            proc_id: None,
            msg_id: message_id.map(String::from),
            structured_data,
            message: "the quick brown fox".into(),
            raw: String::new(),
            version: SyslogVersion::Rfc5424,
        }
    }

    #[test]
    fn known_event_table() {
        assert!(known_event("fc2e22bc6ee647b6b90729ab34a250b1").is_some());
        assert_eq!(
            known_event("d9b373ed55a64feb8242e02dbe79a49c")
                .unwrap()
                .name,
            "unit-failed"
        );
        // process-exited is intentionally not an alert.
        assert!(known_event("98e322203f7a4ed290d09fe03c09fe15").is_none());
        assert!(known_event("not-a-known-id").is_none());
    }

    #[test]
    fn parse_severity_names() {
        assert_eq!(parse_severity("critical"), Some(AlertSeverity::Critical));
        assert_eq!(parse_severity("WARN"), Some(AlertSeverity::Warning));
        assert_eq!(parse_severity("info"), Some(AlertSeverity::Info));
        assert_eq!(parse_severity("bogus"), None);
    }

    #[test]
    fn detect_alert_coredump_is_critical_with_labels() {
        let m = msg_with(
            Some("fc2e22bc6ee647b6b90729ab34a250b1"),
            Some("nginx.service"),
        );
        let a = detect_alert(&m, "host1", &HashMap::new()).unwrap();
        assert_eq!(a.severity, AlertSeverity::Critical);
        assert_eq!(a.labels.get("event").map(String::as_str), Some("coredump"));
        assert_eq!(
            a.labels.get("unit").map(String::as_str),
            Some("nginx.service")
        );
        assert!(a.summary.starts_with("coredump:"));
    }

    #[test]
    fn detect_alert_none_for_ordinary_message() {
        assert!(detect_alert(&msg_with(None, None), "h", &HashMap::new()).is_none());
        assert!(detect_alert(&msg_with(Some("deadbeef"), None), "h", &HashMap::new()).is_none());
    }

    #[test]
    fn detect_alert_honors_severity_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "fc2e22bc6ee647b6b90729ab34a250b1".to_string(),
            AlertSeverity::Info,
        );
        let m = msg_with(Some("fc2e22bc6ee647b6b90729ab34a250b1"), None);
        let a = detect_alert(&m, "h", &overrides).unwrap();
        assert_eq!(a.severity, AlertSeverity::Info);
    }

    #[test]
    fn should_fire_dedups_within_window() {
        let mut active = HashMap::new();
        let t0 = Instant::now();
        let window = Duration::from_secs(30);
        assert!(should_fire(&mut active, "k", t0, window)); // first → fire
        assert!(!should_fire(
            &mut active,
            "k",
            t0 + Duration::from_secs(5),
            window
        )); // within → no
        // After the window expires, it fires again.
        assert!(should_fire(
            &mut active,
            "k",
            t0 + Duration::from_secs(31),
            window
        ));
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
