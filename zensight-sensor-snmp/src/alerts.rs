//! Threshold alerting for polled SNMP devices (#528).
//!
//! Mirrors the sysinfo/netlink pattern: a **pure** [`evaluate`] function maps
//! one poll cycle's observations ([`CycleObservation`]) + thresholds
//! ([`SnmpAlertsConfig`]) to per-rule firing [`Alert`]s (unit-testable, no
//! session), and an [`AlertEvaluator`] per device drives the lifecycle
//! through the shared [`AlertReporter`]: `observe` every violation, then
//! reconcile each rule *scoped to this device* so one device's sweep never
//! resolves another's alerts (`reconcile_labeled` on the `device` label).
//!
//! Alert keys stay stable per condition: bucketing labels only (`device`,
//! `if_index`, `direction`, `kind`, `storage_index`, `cpu_index`) — live
//! values ride the summary, so oscillation updates one alert in place.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::warn;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

// Stable rule slugs.
const UNREACHABLE_RULE: &str = "device_unreachable";
const IF_DOWN_RULE: &str = "interface_down";
const IF_ERRORS_RULE: &str = "interface_errors";
const IF_UTILIZATION_RULE: &str = "interface_utilization";
const REBOOT_RULE: &str = "device_rebooted";
const STORAGE_RULE: &str = "storage_usage";
const CPU_RULE: &str = "processor_load";

// ===========================================================================
// Configuration
// ===========================================================================

fn default_true() -> bool {
    true
}

/// Top-level alerting configuration (JSON5 `snmp.alerts`), overridable per
/// device via `devices[].alerts` (a full replacement, not a field merge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnmpAlertsConfig {
    /// Master switch (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// "Violated continuously for N seconds" debounce before publishing a
    /// firing alert (default 0 = first violation publishes).
    #[serde(default)]
    pub for_secs: u64,

    #[serde(default)]
    pub unreachable: UnreachableRule,
    #[serde(default)]
    pub interface_down: SimpleRule,
    #[serde(default)]
    pub interface_errors: ErrorRateRule,
    #[serde(default)]
    pub utilization: PercentRule,
    #[serde(default)]
    pub reboot: RebootRule,
    #[serde(default)]
    pub storage: PercentRule,
    #[serde(default)]
    pub processor: PercentRule,
}

impl Default for SnmpAlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: 0,
            unreachable: UnreachableRule::default(),
            interface_down: SimpleRule::default(),
            interface_errors: ErrorRateRule::default(),
            utilization: PercentRule::default(),
            reboot: RebootRule::default(),
            storage: PercentRule::default(),
            processor: PercentRule::default(),
        }
    }
}

/// Device-unreachable: N consecutive poll cycles where every request failed
/// at the transport level (timeouts/network) — not SNMP-level errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnreachableRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Consecutive fully-failed cycles before firing (default 3).
    #[serde(default = "default_unreachable_cycles")]
    pub cycles: u32,
}

fn default_unreachable_cycles() -> u32 {
    3
}

impl Default for UnreachableRule {
    fn default() -> Self {
        Self {
            enabled: true,
            cycles: default_unreachable_cycles(),
        }
    }
}

/// A rule with only an on/off switch (interface oper-down while admin-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for SimpleRule {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Interface error/discard rate above a per-second threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRateRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Errors (or discards) per second (default 1.0).
    #[serde(default = "default_error_rate")]
    pub per_sec: f64,
}

fn default_error_rate() -> f64 {
    1.0
}

impl Default for ErrorRateRule {
    fn default() -> Self {
        Self {
            enabled: true,
            per_sec: default_error_rate(),
        }
    }
}

/// A percentage watermark (utilization vs speed, storage used, cpu load).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PercentRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_percent")]
    pub percent: f64,
}

fn default_percent() -> f64 {
    90.0
}

impl Default for PercentRule {
    fn default() -> Self {
        Self {
            enabled: true,
            percent: default_percent(),
        }
    }
}

/// Device-rebooted (sysUpTime went backwards): fires Info and stays visible
/// for `hold_secs`, then auto-resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebootRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_reboot_hold")]
    pub hold_secs: u64,
}

fn default_reboot_hold() -> u64 {
    300
}

impl Default for RebootRule {
    fn default() -> Self {
        Self {
            enabled: true,
            hold_secs: default_reboot_hold(),
        }
    }
}

// ===========================================================================
// Cycle observation (assembled by the poller)
// ===========================================================================

/// What one poll cycle saw, in evaluator terms.
#[derive(Debug, Default)]
pub struct CycleObservation {
    /// Every request this cycle failed at the transport level.
    pub all_transport_failed: bool,
    /// sysUpTime went backwards this cycle (device rebooted).
    pub reset_detected: bool,
    /// Per-interface state, keyed by ifIndex.
    pub interfaces: HashMap<u32, IfObservation>,
    /// HOST-RESOURCES storage rows, keyed by hrStorageIndex.
    pub storage: HashMap<u32, StorageObservation>,
    /// hrProcessorLoad (percent), keyed by processor row index.
    pub cpu_load: HashMap<u32, f64>,
}

#[derive(Debug, Default)]
pub struct IfObservation {
    /// ifName (preferred) or ifDescr.
    pub name: Option<String>,
    pub admin_up: Option<bool>,
    pub oper_up: Option<bool>,
    /// Link speed in bits/s (ifHighSpeed preferred over ifSpeed).
    pub speed_bits: Option<f64>,
    /// Octet rates in bytes/s (HC preferred), by direction.
    pub in_octet_rate: Option<f64>,
    pub out_octet_rate: Option<f64>,
    /// (direction, kind) → per-second rate for errors/discards.
    pub error_rates: HashMap<(&'static str, &'static str), f64>,
}

#[derive(Debug, Default)]
pub struct StorageObservation {
    pub descr: Option<String>,
    pub used: Option<f64>,
    pub size: Option<f64>,
}

// OID prefixes ingested into the observation.
const IF_TABLE: &str = "1.3.6.1.2.1.2.2.1";
const IF_X_TABLE: &str = "1.3.6.1.2.1.31.1.1.1";
const HR_STORAGE: &str = "1.3.6.1.2.1.25.2.3.1";
const HR_PROCESSOR_LOAD: &str = "1.3.6.1.2.1.25.3.3.1.2";

/// Walked columns the interface rules need. [`SnmpPoller`] auto-adds any of
/// these not already covered by a configured walk when alerting is on.
///
/// [`SnmpPoller`]: crate::poller::SnmpPoller
pub const INTERFACE_RULE_COLUMNS: [&str; 13] = [
    "1.3.6.1.2.1.2.2.1.2",     // ifDescr (naming)
    "1.3.6.1.2.1.2.2.1.5",     // ifSpeed
    "1.3.6.1.2.1.2.2.1.7",     // ifAdminStatus
    "1.3.6.1.2.1.2.2.1.8",     // ifOperStatus
    "1.3.6.1.2.1.2.2.1.10",    // ifInOctets
    "1.3.6.1.2.1.2.2.1.13",    // ifInDiscards
    "1.3.6.1.2.1.2.2.1.14",    // ifInErrors
    "1.3.6.1.2.1.2.2.1.16",    // ifOutOctets
    "1.3.6.1.2.1.2.2.1.19",    // ifOutDiscards
    "1.3.6.1.2.1.2.2.1.20",    // ifOutErrors
    "1.3.6.1.2.1.31.1.1.1.6",  // ifHCInOctets
    "1.3.6.1.2.1.31.1.1.1.10", // ifHCOutOctets
    "1.3.6.1.2.1.31.1.1.1.15", // ifHighSpeed
];

impl CycleObservation {
    /// Ingest one polled value (+ derived rate, when the poller computed
    /// one). Unrecognized OIDs are ignored — the observation only cares
    /// about the columns the rules read.
    pub fn ingest(&mut self, oid: &str, value: &async_snmp::Value, rate: Option<f64>) {
        use async_snmp::Value;

        if let Some((column, index)) = split_column(oid, IF_TABLE) {
            let entry = self.interfaces.entry(index).or_default();
            match (column, value) {
                (2, Value::OctetString(s)) if entry.name.is_none() => {
                    entry.name = String::from_utf8(s.to_vec()).ok();
                }
                (5, Value::Gauge32(n)) if entry.speed_bits.is_none() => {
                    entry.speed_bits = Some(f64::from(*n));
                }
                (7, Value::Integer(n)) => entry.admin_up = Some(*n == 1),
                (8, Value::Integer(n)) => entry.oper_up = Some(*n == 1),
                (10, _) => {
                    if let Some(r) = rate
                        && entry.in_octet_rate.is_none()
                    {
                        entry.in_octet_rate = Some(r);
                    }
                }
                (16, _) => {
                    if let Some(r) = rate
                        && entry.out_octet_rate.is_none()
                    {
                        entry.out_octet_rate = Some(r);
                    }
                }
                (13, _) => insert_rate(entry, ("in", "discards"), rate),
                (14, _) => insert_rate(entry, ("in", "errors"), rate),
                (19, _) => insert_rate(entry, ("out", "discards"), rate),
                (20, _) => insert_rate(entry, ("out", "errors"), rate),
                _ => {}
            }
        } else if let Some((column, index)) = split_column(oid, IF_X_TABLE) {
            let entry = self.interfaces.entry(index).or_default();
            match (column, value) {
                // ifName beats ifDescr for naming.
                (1, Value::OctetString(s)) => entry.name = String::from_utf8(s.to_vec()).ok(),
                // ifHighSpeed (Mb/s) beats ifSpeed.
                (15, Value::Gauge32(n)) => entry.speed_bits = Some(f64::from(*n) * 1e6),
                // HC octet counters beat the 32-bit ones.
                (6, _) => {
                    if let Some(r) = rate {
                        entry.in_octet_rate = Some(r);
                    }
                }
                (10, _) => {
                    if let Some(r) = rate {
                        entry.out_octet_rate = Some(r);
                    }
                }
                _ => {}
            }
        } else if let Some((column, index)) = split_column(oid, HR_STORAGE) {
            let entry = self.storage.entry(index).or_default();
            match (column, value) {
                (3, Value::OctetString(s)) => entry.descr = String::from_utf8(s.to_vec()).ok(),
                (5, Value::Integer(n)) => entry.size = Some(f64::from(*n)),
                (6, Value::Integer(n)) => entry.used = Some(f64::from(*n)),
                _ => {}
            }
        } else if let Some(rest) = oid.strip_prefix(HR_PROCESSOR_LOAD)
            && let Some(index) = rest.strip_prefix('.').and_then(|s| s.parse().ok())
            && let Value::Integer(n) = value
        {
            self.cpu_load.insert(index, f64::from(*n));
        }
    }
}

fn insert_rate(entry: &mut IfObservation, key: (&'static str, &'static str), rate: Option<f64>) {
    if let Some(r) = rate {
        entry.error_rates.insert(key, r);
    }
}

/// Split `<prefix>.<column>.<index>` → (column, index).
fn split_column(oid: &str, prefix: &str) -> Option<(u32, u32)> {
    let rest = oid.strip_prefix(prefix)?.strip_prefix('.')?;
    let (column, index) = rest.split_once('.')?;
    Some((column.parse().ok()?, index.parse().ok()?))
}

// ===========================================================================
// Pure evaluation
// ===========================================================================

/// The firing alerts for one rule this sweep (possibly empty — the driver
/// still reconciles the rule so recovered conditions resolve).
pub struct RuleAlerts {
    pub rule: &'static str,
    pub alerts: Vec<Alert>,
}

/// Evaluator scratch state that spans cycles.
#[derive(Default)]
struct EvalState {
    consecutive_transport_failures: u32,
    reboot_seen_at: Option<Instant>,
}

/// Map one cycle's observations to firing alerts. Pure except for the
/// `state` scratch (consecutive-failure counter, reboot hold window).
fn evaluate(
    device: &str,
    cfg: &SnmpAlertsConfig,
    obs: &CycleObservation,
    state: &mut EvalState,
    now: Instant,
) -> Vec<RuleAlerts> {
    let mut out = Vec::new();
    let base = |rule: &'static str, severity: AlertSeverity, summary: String| {
        Alert::new(
            device,
            Protocol::Snmp,
            AlertKind::Expectation,
            rule,
            severity,
            summary,
        )
        .with_label("device", device)
    };

    // --- device_unreachable -------------------------------------------------
    if obs.all_transport_failed {
        state.consecutive_transport_failures += 1;
    } else {
        state.consecutive_transport_failures = 0;
    }
    if cfg.unreachable.enabled {
        let mut alerts = Vec::new();
        if state.consecutive_transport_failures >= cfg.unreachable.cycles {
            alerts.push(base(
                UNREACHABLE_RULE,
                AlertSeverity::Critical,
                format!(
                    "{} unreachable: {} consecutive poll cycles failed",
                    device, state.consecutive_transport_failures
                ),
            ));
        }
        out.push(RuleAlerts {
            rule: UNREACHABLE_RULE,
            alerts,
        });
    }

    // An unreachable device produced no rows this cycle; interface/storage
    // rules would reconcile-away as "recovered", which is wrong — skip them
    // and keep their previous state until the device answers again.
    let device_answered = !obs.all_transport_failed;

    // --- device_rebooted ----------------------------------------------------
    if obs.reset_detected {
        state.reboot_seen_at = Some(now);
    }
    if cfg.reboot.enabled {
        let mut alerts = Vec::new();
        if let Some(seen) = state.reboot_seen_at {
            if now.duration_since(seen) <= Duration::from_secs(cfg.reboot.hold_secs) {
                alerts.push(base(
                    REBOOT_RULE,
                    AlertSeverity::Info,
                    format!("{device} rebooted (sysUpTime went backwards)"),
                ));
            } else {
                state.reboot_seen_at = None;
            }
        }
        out.push(RuleAlerts {
            rule: REBOOT_RULE,
            alerts,
        });
    }

    if device_answered {
        // --- interface_down -------------------------------------------------
        if cfg.interface_down.enabled {
            let mut alerts = Vec::new();
            for (index, ifo) in &obs.interfaces {
                if ifo.admin_up == Some(true) && ifo.oper_up == Some(false) {
                    let name = ifo.name.clone().unwrap_or_else(|| format!("if{index}"));
                    alerts.push(
                        base(
                            IF_DOWN_RULE,
                            AlertSeverity::Warning,
                            format!("{device}: interface {name} is down (admin-up)"),
                        )
                        .with_label("if_index", index.to_string())
                        .with_label("if_name", name),
                    );
                }
            }
            out.push(RuleAlerts {
                rule: IF_DOWN_RULE,
                alerts,
            });
        }

        // --- interface_errors -----------------------------------------------
        if cfg.interface_errors.enabled {
            let mut alerts = Vec::new();
            for (index, ifo) in &obs.interfaces {
                for ((direction, kind), rate) in &ifo.error_rates {
                    if *rate > cfg.interface_errors.per_sec {
                        let name = ifo.name.clone().unwrap_or_else(|| format!("if{index}"));
                        alerts.push(
                            base(
                                IF_ERRORS_RULE,
                                AlertSeverity::Warning,
                                format!(
                                    "{device}: {name} {direction} {kind} at {rate:.1}/s (threshold {}/s)",
                                    cfg.interface_errors.per_sec
                                ),
                            )
                            .with_label("if_index", index.to_string())
                            .with_label("if_name", name)
                            .with_label("direction", *direction)
                            .with_label("kind", *kind),
                        );
                    }
                }
            }
            out.push(RuleAlerts {
                rule: IF_ERRORS_RULE,
                alerts,
            });
        }

        // --- interface_utilization ------------------------------------------
        if cfg.utilization.enabled {
            let mut alerts = Vec::new();
            for (index, ifo) in &obs.interfaces {
                let Some(speed) = ifo.speed_bits.filter(|s| *s > 0.0) else {
                    continue;
                };
                for (direction, rate) in [("in", ifo.in_octet_rate), ("out", ifo.out_octet_rate)] {
                    let Some(rate) = rate else { continue };
                    let percent = rate * 8.0 / speed * 100.0;
                    if percent > cfg.utilization.percent {
                        let name = ifo.name.clone().unwrap_or_else(|| format!("if{index}"));
                        alerts.push(
                            base(
                                IF_UTILIZATION_RULE,
                                AlertSeverity::Warning,
                                format!(
                                    "{device}: {name} {direction} at {percent:.0}% of link speed"
                                ),
                            )
                            .with_label("if_index", index.to_string())
                            .with_label("if_name", name)
                            .with_label("direction", direction),
                        );
                    }
                }
            }
            out.push(RuleAlerts {
                rule: IF_UTILIZATION_RULE,
                alerts,
            });
        }

        // --- storage_usage --------------------------------------------------
        if cfg.storage.enabled {
            let mut alerts = Vec::new();
            for (index, st) in &obs.storage {
                let (Some(used), Some(size)) = (st.used, st.size) else {
                    continue;
                };
                if size <= 0.0 {
                    continue;
                }
                let percent = used / size * 100.0;
                if percent > cfg.storage.percent {
                    let descr = st
                        .descr
                        .clone()
                        .unwrap_or_else(|| format!("storage {index}"));
                    alerts.push(
                        base(
                            STORAGE_RULE,
                            AlertSeverity::Warning,
                            format!("{device}: {descr} at {percent:.0}% used"),
                        )
                        .with_label("storage_index", index.to_string()),
                    );
                }
            }
            out.push(RuleAlerts {
                rule: STORAGE_RULE,
                alerts,
            });
        }

        // --- processor_load -------------------------------------------------
        if cfg.processor.enabled {
            let mut alerts = Vec::new();
            for (index, load) in &obs.cpu_load {
                if *load > cfg.processor.percent {
                    alerts.push(
                        base(
                            CPU_RULE,
                            AlertSeverity::Warning,
                            format!("{device}: processor {index} at {load:.0}% load"),
                        )
                        .with_label("cpu_index", index.to_string()),
                    );
                }
            }
            out.push(RuleAlerts {
                rule: CPU_RULE,
                alerts,
            });
        }
    }

    out
}

// ===========================================================================
// Driver
// ===========================================================================

/// Per-device alert driver over the shared reporter.
pub struct AlertEvaluator {
    device: String,
    cfg: SnmpAlertsConfig,
    reporter: Arc<AlertReporter>,
    state: EvalState,
}

impl AlertEvaluator {
    /// The reporter should already carry the configured debounce
    /// (`AlertReporter::with_debounce`).
    pub fn new(device: String, cfg: SnmpAlertsConfig, reporter: Arc<AlertReporter>) -> Self {
        Self {
            device,
            cfg,
            reporter,
            state: EvalState::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Whether any interface rule wants the IF-MIB columns auto-walked.
    pub fn wants_interface_columns(&self) -> bool {
        self.cfg.enabled
            && (self.cfg.interface_down.enabled
                || self.cfg.interface_errors.enabled
                || self.cfg.utilization.enabled)
    }

    /// Evaluate one poll cycle and reconcile this device's alert set.
    pub async fn tick(&mut self, obs: &CycleObservation) {
        if !self.cfg.enabled {
            return;
        }
        let for_duration = (self.cfg.for_secs > 0).then(|| Duration::from_secs(self.cfg.for_secs));

        // Rules absent from the sweep (device unanswering → interface rules
        // keep state; disabled rules) are deliberately NOT reconciled.
        let sweeps = evaluate(
            &self.device,
            &self.cfg,
            obs,
            &mut self.state,
            Instant::now(),
        );
        for ra in sweeps {
            let mut firing_keys = Vec::with_capacity(ra.alerts.len());
            for alert in ra.alerts {
                firing_keys.push(alert.alert_key());
                if let Err(e) = self.reporter.observe(alert, for_duration).await {
                    warn!(error = %e, rule = %ra.rule, device = %self.device, "snmp: failed to publish alert");
                }
            }
            if let Err(e) = self
                .reporter
                .reconcile_labeled(ra.rule, "device", &self.device, &firing_keys)
                .await
            {
                warn!(error = %e, rule = %ra.rule, device = %self.device, "snmp: failed to reconcile alerts");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SnmpAlertsConfig {
        SnmpAlertsConfig::default()
    }

    fn find<'a>(sweeps: &'a [RuleAlerts], rule: &str) -> Option<&'a RuleAlerts> {
        sweeps.iter().find(|ra| ra.rule == rule)
    }

    #[test]
    fn unreachable_fires_after_n_cycles() {
        let mut state = EvalState::default();
        let obs = CycleObservation {
            all_transport_failed: true,
            ..Default::default()
        };
        let now = Instant::now();
        for i in 1..=3 {
            let sweeps = evaluate("r1", &cfg(), &obs, &mut state, now);
            let ra = find(&sweeps, UNREACHABLE_RULE).unwrap();
            if i < 3 {
                assert!(ra.alerts.is_empty(), "cycle {i} must not fire yet");
            } else {
                assert_eq!(ra.alerts.len(), 1);
                assert_eq!(ra.alerts[0].severity, AlertSeverity::Critical);
                assert_eq!(ra.alerts[0].labels["device"], "r1");
            }
        }
        // Recovery resets the counter and clears the rule sweep.
        let ok = CycleObservation::default();
        let sweeps = evaluate("r1", &cfg(), &ok, &mut state, now);
        assert!(find(&sweeps, UNREACHABLE_RULE).unwrap().alerts.is_empty());
        assert_eq!(state.consecutive_transport_failures, 0);
    }

    #[test]
    fn interface_down_only_when_admin_up() {
        let mut obs = CycleObservation::default();
        obs.interfaces.insert(
            1,
            IfObservation {
                name: Some("eth0".into()),
                admin_up: Some(true),
                oper_up: Some(false),
                ..Default::default()
            },
        );
        // Admin-down interface: intentionally off, no alert.
        obs.interfaces.insert(
            2,
            IfObservation {
                admin_up: Some(false),
                oper_up: Some(false),
                ..Default::default()
            },
        );
        let sweeps = evaluate(
            "r1",
            &cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        let ra = find(&sweeps, IF_DOWN_RULE).unwrap();
        assert_eq!(ra.alerts.len(), 1);
        assert_eq!(ra.alerts[0].labels["if_name"], "eth0");
    }

    #[test]
    fn error_rate_threshold() {
        let mut obs = CycleObservation::default();
        let mut ifo = IfObservation::default();
        ifo.error_rates.insert(("in", "errors"), 5.0);
        ifo.error_rates.insert(("out", "errors"), 0.2);
        obs.interfaces.insert(3, ifo);
        let sweeps = evaluate(
            "r1",
            &cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        let ra = find(&sweeps, IF_ERRORS_RULE).unwrap();
        assert_eq!(ra.alerts.len(), 1);
        assert_eq!(ra.alerts[0].labels["direction"], "in");
    }

    #[test]
    fn utilization_against_high_speed() {
        let mut obs = CycleObservation::default();
        obs.interfaces.insert(
            1,
            IfObservation {
                speed_bits: Some(100e6),
                // 95 Mb/s in bytes/s.
                in_octet_rate: Some(95e6 / 8.0),
                out_octet_rate: Some(10e6 / 8.0),
                ..Default::default()
            },
        );
        let sweeps = evaluate(
            "r1",
            &cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        let ra = find(&sweeps, IF_UTILIZATION_RULE).unwrap();
        assert_eq!(ra.alerts.len(), 1);
        assert_eq!(ra.alerts[0].labels["direction"], "in");
    }

    #[test]
    fn reboot_holds_then_clears() {
        let mut state = EvalState::default();
        let start = Instant::now();
        let obs = CycleObservation {
            reset_detected: true,
            ..Default::default()
        };
        let sweeps = evaluate("r1", &cfg(), &obs, &mut state, start);
        assert_eq!(find(&sweeps, REBOOT_RULE).unwrap().alerts.len(), 1);

        // Still inside the hold window on a later, clean cycle.
        let clean = CycleObservation::default();
        let sweeps = evaluate(
            "r1",
            &cfg(),
            &clean,
            &mut state,
            start + Duration::from_secs(60),
        );
        assert_eq!(find(&sweeps, REBOOT_RULE).unwrap().alerts.len(), 1);

        // Past the hold window: cleared.
        let sweeps = evaluate(
            "r1",
            &cfg(),
            &clean,
            &mut state,
            start + Duration::from_secs(301),
        );
        assert!(find(&sweeps, REBOOT_RULE).unwrap().alerts.is_empty());
    }

    #[test]
    fn storage_and_cpu_thresholds() {
        let mut obs = CycleObservation::default();
        obs.storage.insert(
            1,
            StorageObservation {
                descr: Some("/var".into()),
                used: Some(950.0),
                size: Some(1000.0),
            },
        );
        obs.cpu_load.insert(1, 97.0);
        obs.cpu_load.insert(2, 12.0);
        let sweeps = evaluate(
            "r1",
            &cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(find(&sweeps, STORAGE_RULE).unwrap().alerts.len(), 1);
        assert_eq!(find(&sweeps, CPU_RULE).unwrap().alerts.len(), 1);
    }

    #[test]
    fn unanswering_device_skips_interface_rules() {
        let mut state = EvalState::default();
        let obs = CycleObservation {
            all_transport_failed: true,
            ..Default::default()
        };
        let sweeps = evaluate("r1", &cfg(), &obs, &mut state, Instant::now());
        assert!(find(&sweeps, IF_DOWN_RULE).is_none());
        assert!(find(&sweeps, IF_ERRORS_RULE).is_none());
        assert!(find(&sweeps, STORAGE_RULE).is_none());
    }

    #[test]
    fn disabled_rule_still_reconciles_empty() {
        let mut config = cfg();
        config.interface_down.enabled = false;
        let mut obs = CycleObservation::default();
        obs.interfaces.insert(
            1,
            IfObservation {
                admin_up: Some(true),
                oper_up: Some(false),
                ..Default::default()
            },
        );
        let sweeps = evaluate(
            "r1",
            &config,
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert!(
            find(&sweeps, IF_DOWN_RULE).is_none(),
            "disabled rule must not sweep"
        );
    }

    #[test]
    fn observation_ingest_maps_columns() {
        let mut obs = CycleObservation::default();
        use async_snmp::Value;
        use bytes::Bytes;
        obs.ingest("1.3.6.1.2.1.2.2.1.7.3", &Value::Integer(1), None);
        obs.ingest("1.3.6.1.2.1.2.2.1.8.3", &Value::Integer(2), None);
        obs.ingest(
            "1.3.6.1.2.1.31.1.1.1.1.3",
            &Value::OctetString(Bytes::from_static(b"eth3")),
            None,
        );
        obs.ingest("1.3.6.1.2.1.2.2.1.5.3", &Value::Gauge32(10_000_000), None);
        obs.ingest("1.3.6.1.2.1.31.1.1.1.15.3", &Value::Gauge32(100), None);
        obs.ingest("1.3.6.1.2.1.2.2.1.14.3", &Value::Counter32(50), Some(2.5));
        obs.ingest("1.3.6.1.2.1.25.3.3.1.2.1", &Value::Integer(95), None);

        let ifo = &obs.interfaces[&3];
        assert_eq!(ifo.admin_up, Some(true));
        assert_eq!(ifo.oper_up, Some(false));
        assert_eq!(ifo.name.as_deref(), Some("eth3"));
        // ifHighSpeed (100 Mb/s) beats ifSpeed (10 Mb/s).
        assert_eq!(ifo.speed_bits, Some(100e6));
        assert_eq!(ifo.error_rates[&("in", "errors")], 2.5);
        assert_eq!(obs.cpu_load[&1], 95.0);
    }
}
