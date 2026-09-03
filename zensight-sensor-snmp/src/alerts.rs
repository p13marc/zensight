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

use std::collections::{BTreeMap, HashMap};
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
// UPS / PDU (#955, SYS-SUP-002 and the read half of -003).
const UPS_ON_BATTERY_RULE: &str = "ups_on_battery";
const UPS_BATTERY_LOW_RULE: &str = "ups_battery_low";
const UPS_RUNTIME_LOW_RULE: &str = "ups_runtime_low";
const UPS_LOAD_HIGH_RULE: &str = "ups_load_high";
const PDU_OUTLET_OFF_RULE: &str = "pdu_outlet_off";
const PDU_OVERLOAD_RULE: &str = "pdu_overload";

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

    // ── UPS / PDU (#955) ────────────────────────────────────────────────
    //
    // These read what the `ups` and `pdu-*` profiles walk. A device with
    // neither profile produces no observation, so the rules fire nothing and
    // reconcile empty — they cost a switch nothing beyond the empty sweep.
    #[serde(default)]
    pub ups_on_battery: SimpleRule,
    #[serde(default)]
    pub ups_battery_low: SimpleRule,
    #[serde(default)]
    pub ups_runtime_low: MinutesRule,
    #[serde(default)]
    pub ups_load_high: OptionalPercentRule,
    #[serde(default)]
    pub pdu_outlet_off: OutletRule,
    #[serde(default)]
    pub pdu_overload: OptionalPercentRule,
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
            ups_on_battery: SimpleRule::default(),
            ups_battery_low: SimpleRule::default(),
            ups_runtime_low: MinutesRule::default(),
            ups_load_high: OptionalPercentRule::default(),
            pdu_outlet_off: OutletRule::default(),
            pdu_overload: OptionalPercentRule::default(),
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

/// Estimated runtime below a configured floor (#955).
///
/// **There is no default number**, and that is the rule, not an omission. A
/// five-minute line-interactive UPS under a switch and a sixty-minute one under
/// a rack have different answers, and a number invented here would page the
/// whole fleet the first time it ran. Unset means the rule never fires; it
/// migrates to #931's `ThresholdsConfig` when that lands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinutesRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub minutes: Option<f64>,
}

impl Default for MinutesRule {
    fn default() -> Self {
        Self {
            enabled: true,
            minutes: None,
        }
    }
}

/// A percentage watermark with **no default** — see [`MinutesRule`] for why.
///
/// Distinct from [`PercentRule`], whose 90 % is a defensible universal for a
/// filesystem. "Loaded" is a property of how a site sized its power, not of
/// power.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionalPercentRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub percent: Option<f64>,
}

impl Default for OptionalPercentRule {
    fn default() -> Self {
        Self {
            enabled: true,
            percent: None,
        }
    }
}

/// Outlets that must read *on* (#955).
///
/// Empty is the default and fires nothing: which outlets matter is site
/// knowledge, and a PDU with an outlet deliberately dark is as normal as one
/// with all of them lit. Ids are the table index as the device reports it —
/// `"3"` on an APC or a Raritan, `"1.3"` on an Eaton, whose outlet tables are
/// indexed by `unit.outlet`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutletRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub expect_on: Vec<String>,
}

impl Default for OutletRule {
    fn default() -> Self {
        Self {
            enabled: true,
            expect_on: Vec::new(),
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
    /// RFC 1628 UPS scalars, when the device answered the UPS tree (#955).
    pub ups: UpsObservation,
    /// Per-outlet PDU state, keyed by the table index **as the device reports
    /// it** — `"3"` on an APC or Raritan, `"1.3"` on an Eaton, whose outlet
    /// tables are indexed by `unit.outlet`. A `String` rather than a `u32` so
    /// no vendor has to be special-cased above this line.
    pub outlets: BTreeMap<String, OutletObservation>,
    /// Whole-PDU load, however the vendor expresses it.
    pub pdu: PduObservation,
}

/// RFC 1628 scalars the UPS rules read. Every field is `Option`: a UPS that
/// does not implement `upsEstimatedMinutesRemaining` fires nothing rather than
/// reading as zero minutes left.
#[derive(Debug, Default)]
pub struct UpsObservation {
    /// `upsBatteryStatus`: unknown(1) normal(2) low(3) depleted(4).
    pub battery_status: Option<i64>,
    /// `upsOutputSource`: other(1) none(2) normal(3) bypass(4) battery(5)
    /// booster(6) reducer(7).
    pub output_source: Option<i64>,
    pub minutes_remaining: Option<f64>,
    pub charge_percent: Option<f64>,
    /// `upsOutputPercentLoad` per output line.
    pub percent_load: HashMap<u32, f64>,
}

#[derive(Debug, Default)]
pub struct OutletObservation {
    pub name: Option<String>,
    /// `None` when the device reports a transitional state (Eaton's
    /// pendingOn/pendingOff, Raritan's cycling) — mid-transition is not the
    /// same fact as off, and firing on it would page every reboot.
    pub on: Option<bool>,
}

#[derive(Debug, Default)]
pub struct PduObservation {
    /// A vendor that reports load as a percentage (Eaton `inputCurrentPercentLoad`).
    pub percent_load: Option<f64>,
    /// A vendor that reports it as a verdict (APC `rPDU2DeviceStatusLoadState`
    /// in nearOverload(3)/overload(4)). The device already decided, against
    /// the rating we do not know; overriding that with a percentage we made up
    /// would be worse information, not more.
    pub overloaded: Option<bool>,
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

// ── RFC 1628 UPS-MIB scalars (#955). Exact OIDs, `.0` and all. ────────────
const UPS_BATTERY_STATUS: &str = "1.3.6.1.2.1.33.1.2.1.0";
const UPS_MINUTES_REMAINING: &str = "1.3.6.1.2.1.33.1.2.3.0";
const UPS_CHARGE_REMAINING: &str = "1.3.6.1.2.1.33.1.2.4.0";
const UPS_OUTPUT_SOURCE: &str = "1.3.6.1.2.1.33.1.4.1.0";
/// `upsOutputPercentLoad`, a column of `upsOutputTable`.
const UPS_OUTPUT_PERCENT_LOAD: &str = "1.3.6.1.2.1.33.1.4.4.1.5";

// ── PDU outlet columns, per vendor (#955) ─────────────────────────────────
//
// There is no standard PDU MIB — RFC 1628 stops at the outlet — so the vendor
// tree is the only place this data lives. Each entry is a full column OID
// verified against the vendor MIB (APC PowerNet-MIB v4.5.8) or against the
// OID set NUT drives real hardware with (Eaton Marlin, Raritan PX):
// `(column oid, on-value, off-value)`.
//
// The suffix after the column is the outlet id verbatim, however many
// integers it is — which is what makes the Eaton `unit.outlet` pair work
// without a second code path.
const PDU_OUTLET_NAME_COLUMNS: [&str; 3] = [
    "1.3.6.1.4.1.318.1.1.26.9.2.3.1.3", // APC rPDU2OutletSwitchedStatusName
    "1.3.6.1.4.1.534.6.6.7.6.1.1.6",    // Eaton outletDesignator
    "1.3.6.1.4.1.13742.1.2.2.1.2",      // Raritan outletLabel
];
const PDU_OUTLET_STATE_COLUMNS: [(&str, i64, i64); 3] = [
    // APC rPDU2OutletSwitchedStatusState: off(1) on(2)
    ("1.3.6.1.4.1.318.1.1.26.9.2.3.1.5", 2, 1),
    // Eaton outletControlStatus: off(0) on(1) pendingOff(2) pendingOn(3)
    ("1.3.6.1.4.1.534.6.6.7.6.6.1.2", 1, 0),
    // Raritan outletOperationalState: off(0) on(1) cycling(2)
    ("1.3.6.1.4.1.13742.1.2.2.1.3", 1, 0),
];
/// Eaton `inputCurrentPercentLoad`.
const PDU_PERCENT_LOAD_COLUMN: &str = "1.3.6.1.4.1.534.6.6.7.3.3.1.11";
/// APC `rPDU2DeviceStatusLoadState`: lowLoad(1) normal(2) nearOverload(3)
/// overload(4) notsupported(5).
const PDU_LOAD_STATE_COLUMN: &str = "1.3.6.1.4.1.318.1.1.26.4.3.1.4";

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
        } else {
            self.ingest_power(oid, value);
        }
    }

    /// UPS (RFC 1628) and PDU (per vendor) columns — #955.
    ///
    /// Split out because it is a different tree, not because it is optional:
    /// a device with neither profile simply never matches any of these.
    fn ingest_power(&mut self, oid: &str, value: &async_snmp::Value) {
        use async_snmp::Value;

        match oid {
            UPS_BATTERY_STATUS => self.ups.battery_status = as_int(value),
            UPS_OUTPUT_SOURCE => self.ups.output_source = as_int(value),
            UPS_MINUTES_REMAINING => {
                self.ups.minutes_remaining = as_int(value).map(|n| n as f64);
            }
            UPS_CHARGE_REMAINING => {
                self.ups.charge_percent = as_int(value).map(|n| n as f64);
            }
            _ => {
                if let Some(index) = index_after(oid, UPS_OUTPUT_PERCENT_LOAD)
                    && let Ok(line) = index.parse::<u32>()
                    && let Some(n) = as_int(value)
                {
                    self.ups.percent_load.insert(line, n as f64);
                    return;
                }
                for column in PDU_OUTLET_NAME_COLUMNS {
                    if let Some(index) = index_after(oid, column)
                        && let Value::OctetString(bytes) = value
                    {
                        self.outlets.entry(index).or_default().name =
                            String::from_utf8(bytes.to_vec()).ok();
                        return;
                    }
                }
                for (column, on, off) in PDU_OUTLET_STATE_COLUMNS {
                    if let Some(index) = index_after(oid, column)
                        && let Some(n) = as_int(value)
                    {
                        // Anything that is neither the on nor the off value is
                        // a transition (pendingOn, cycling) and stays `None`:
                        // mid-reboot is not the same fact as off.
                        let state = if n == on {
                            Some(true)
                        } else if n == off {
                            Some(false)
                        } else {
                            None
                        };
                        self.outlets.entry(index).or_default().on = state;
                        return;
                    }
                }
                if index_after(oid, PDU_PERCENT_LOAD_COLUMN).is_some()
                    && let Some(n) = as_int(value)
                {
                    // Several inlets: the busiest one is the PDU's load.
                    let pct = n as f64;
                    self.pdu.percent_load =
                        Some(self.pdu.percent_load.map_or(pct, |cur| cur.max(pct)));
                } else if index_after(oid, PDU_LOAD_STATE_COLUMN).is_some()
                    && let Some(n) = as_int(value)
                {
                    // notsupported(5) is not a verdict — leave it unset rather
                    // than reading "not supported" as "not overloaded".
                    let verdict = match n {
                        1 | 2 => Some(false),
                        3 | 4 => Some(true),
                        _ => None,
                    };
                    if let Some(v) = verdict {
                        self.pdu.overloaded = Some(self.pdu.overloaded.unwrap_or(false) || v);
                    }
                }
            }
        }
    }
}

/// Any integral SNMP value as an `i64`. The UPS and PDU trees mix `Integer`,
/// `Gauge32` and `Unsigned32` across vendors for the same quantity.
fn as_int(value: &async_snmp::Value) -> Option<i64> {
    use async_snmp::Value;
    match value {
        Value::Integer(n) => Some(i64::from(*n)),
        Value::Gauge32(n) | Value::Counter32(n) | Value::UInteger32(n) => Some(i64::from(*n)),
        Value::Counter64(n) => i64::try_from(*n).ok(),
        _ => None,
    }
}

/// The index suffix of `oid` under column `prefix`, verbatim.
///
/// Verbatim, and returned as a `String`, because an outlet index is one
/// integer on an APC and two (`unit.outlet`) on an Eaton. Parsing it into a
/// number would need a per-vendor code path for a value nothing does
/// arithmetic on.
fn index_after(oid: &str, prefix: &str) -> Option<String> {
    let rest = oid.strip_prefix(prefix)?.strip_prefix('.')?;
    (!rest.is_empty() && rest.split('.').all(|c| c.parse::<u32>().is_ok()))
        .then(|| rest.to_string())
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

        // ── UPS / PDU (#955) ────────────────────────────────────────────
        //
        // Every one of these reads an `Option`. A device that does not serve
        // the OID pushes an empty `RuleAlerts` — so the rule still reconciles
        // (a recovered condition resolves) and still fires nothing. "Not
        // measured" is never a zero: a UPS with no
        // `upsEstimatedMinutesRemaining` is not a UPS with none left.

        // --- ups_on_battery -------------------------------------------------
        if cfg.ups_on_battery.enabled {
            let mut alerts = Vec::new();
            // normal(3) is the only "on mains" value. bypass(4) is not: the
            // UPS is passing mains through with no protection, which is a
            // thing to know about, so it fires like the rest.
            if let Some(source) = obs.ups.output_source
                && source != 3
            {
                let (severity, what) = match source {
                    5 => (AlertSeverity::Critical, "running on battery"),
                    2 => (AlertSeverity::Critical, "output off"),
                    4 => (AlertSeverity::Warning, "on bypass (load unprotected)"),
                    6 => (AlertSeverity::Warning, "boosting (mains low)"),
                    7 => (AlertSeverity::Warning, "reducing (mains high)"),
                    _ => (AlertSeverity::Warning, "output source not normal"),
                };
                alerts.push(
                    base(
                        UPS_ON_BATTERY_RULE,
                        severity,
                        format!("{device}: UPS {what} (upsOutputSource = {source})"),
                    )
                    .with_label("output_source", source.to_string()),
                );
            }
            out.push(RuleAlerts {
                rule: UPS_ON_BATTERY_RULE,
                alerts,
            });
        }

        // --- ups_battery_low ------------------------------------------------
        if cfg.ups_battery_low.enabled {
            let mut alerts = Vec::new();
            // low(3) and depleted(4). unknown(1) is deliberately not a fault:
            // it is the UPS saying it does not know, and paging on that is
            // paging on a missing measurement.
            if let Some(status) = obs.ups.battery_status
                && (status == 3 || status == 4)
            {
                let charge = obs
                    .ups
                    .charge_percent
                    .map(|c| format!(", {c:.0}% charge"))
                    .unwrap_or_default();
                alerts.push(
                    base(
                        UPS_BATTERY_LOW_RULE,
                        AlertSeverity::Critical,
                        format!(
                            "{device}: UPS battery {}{charge}",
                            if status == 4 { "depleted" } else { "low" }
                        ),
                    )
                    .with_label("battery_status", status.to_string()),
                );
            }
            out.push(RuleAlerts {
                rule: UPS_BATTERY_LOW_RULE,
                alerts,
            });
        }

        // --- ups_runtime_low ------------------------------------------------
        if cfg.ups_runtime_low.enabled {
            let mut alerts = Vec::new();
            if let (Some(floor), Some(left)) =
                (cfg.ups_runtime_low.minutes, obs.ups.minutes_remaining)
                && left < floor
            {
                alerts.push(
                    base(
                        UPS_RUNTIME_LOW_RULE,
                        AlertSeverity::Critical,
                        format!(
                            "{device}: UPS estimates {left:.0} min of runtime left \
                             (floor {floor:.0})"
                        ),
                    )
                    .with_label("minutes_remaining", format!("{left:.0}")),
                );
            }
            out.push(RuleAlerts {
                rule: UPS_RUNTIME_LOW_RULE,
                alerts,
            });
        }

        // --- ups_load_high --------------------------------------------------
        if cfg.ups_load_high.enabled {
            let mut alerts = Vec::new();
            if let Some(limit) = cfg.ups_load_high.percent {
                for (line, load) in &obs.ups.percent_load {
                    if *load > limit {
                        alerts.push(
                            base(
                                UPS_LOAD_HIGH_RULE,
                                AlertSeverity::Warning,
                                format!("{device}: UPS output line {line} at {load:.0}% load"),
                            )
                            .with_label("output_line", line.to_string()),
                        );
                    }
                }
            }
            out.push(RuleAlerts {
                rule: UPS_LOAD_HIGH_RULE,
                alerts,
            });
        }

        // --- pdu_outlet_off -------------------------------------------------
        if cfg.pdu_outlet_off.enabled {
            let mut alerts = Vec::new();
            for want in &cfg.pdu_outlet_off.expect_on {
                // An outlet the device did not report is not an outlet that is
                // off. A typo'd id, or a PDU that dropped a module, must not
                // read as an outage.
                let Some(outlet) = obs.outlets.get(want) else {
                    continue;
                };
                if outlet.on == Some(false) {
                    let name = outlet
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("outlet {want}"));
                    alerts.push(
                        base(
                            PDU_OUTLET_OFF_RULE,
                            AlertSeverity::Critical,
                            format!("{device}: outlet {want} ({name}) is off but expected on"),
                        )
                        .with_label("outlet", want.clone())
                        .with_label("outlet_name", name),
                    );
                }
            }
            out.push(RuleAlerts {
                rule: PDU_OUTLET_OFF_RULE,
                alerts,
            });
        }

        // --- pdu_overload ---------------------------------------------------
        if cfg.pdu_overload.enabled {
            let mut alerts = Vec::new();
            // Two vendors, two ways of saying it, one rule. APC's own verdict
            // wins where it exists: it is measured against a rating this
            // sensor does not know.
            if obs.pdu.overloaded == Some(true) {
                alerts.push(base(
                    PDU_OVERLOAD_RULE,
                    AlertSeverity::Warning,
                    format!("{device}: PDU reports its load at or near overload"),
                ));
            } else if let (Some(limit), Some(load)) =
                (cfg.pdu_overload.percent, obs.pdu.percent_load)
                && load > limit
            {
                alerts.push(
                    base(
                        PDU_OVERLOAD_RULE,
                        AlertSeverity::Warning,
                        format!("{device}: PDU input at {load:.0}% load (limit {limit:.0})"),
                    )
                    .with_label("percent_load", format!("{load:.0}")),
                );
            }
            out.push(RuleAlerts {
                rule: PDU_OVERLOAD_RULE,
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

    fn ups_cfg() -> SnmpAlertsConfig {
        SnmpAlertsConfig {
            ups_runtime_low: MinutesRule {
                enabled: true,
                minutes: Some(10.0),
            },
            ups_load_high: OptionalPercentRule {
                enabled: true,
                percent: Some(80.0),
            },
            pdu_overload: OptionalPercentRule {
                enabled: true,
                percent: Some(80.0),
            },
            pdu_outlet_off: OutletRule {
                enabled: true,
                expect_on: vec!["3".to_string(), "1.4".to_string()],
            },
            ..SnmpAlertsConfig::default()
        }
    }

    fn fired(alerts: &[RuleAlerts], rule: &str) -> usize {
        alerts
            .iter()
            .find(|r| r.rule == rule)
            .map(|r| r.alerts.len())
            .unwrap_or_else(|| panic!("{rule} did not reconcile — it must push even when empty"))
    }

    /// On mains, charged, unloaded: every UPS rule reconciles and none fires.
    #[test]
    fn a_healthy_ups_fires_nothing_and_still_reconciles() {
        let mut obs = CycleObservation::default();
        obs.ups.output_source = Some(3); // normal
        obs.ups.battery_status = Some(2); // normal
        obs.ups.minutes_remaining = Some(45.0);
        obs.ups.charge_percent = Some(100.0);
        obs.ups.percent_load.insert(1, 22.0);

        let out = evaluate(
            "ups01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        for rule in [
            UPS_ON_BATTERY_RULE,
            UPS_BATTERY_LOW_RULE,
            UPS_RUNTIME_LOW_RULE,
            UPS_LOAD_HIGH_RULE,
        ] {
            assert_eq!(fired(&out, rule), 0, "{rule} fired on a healthy UPS");
        }
    }

    /// The mains dropped. `bypass` is a different severity from `battery`, and
    /// both are worth knowing — a UPS on bypass is passing mains straight
    /// through with the load unprotected.
    #[test]
    fn output_source_off_mains_fires_with_the_severity_of_what_happened() {
        for (source, severity) in [
            (5, AlertSeverity::Critical), // battery
            (2, AlertSeverity::Critical), // none
            (4, AlertSeverity::Warning),  // bypass
            (6, AlertSeverity::Warning),  // booster
        ] {
            let mut obs = CycleObservation::default();
            obs.ups.output_source = Some(source);
            let out = evaluate(
                "ups01",
                &ups_cfg(),
                &obs,
                &mut EvalState::default(),
                Instant::now(),
            );
            let a = &out
                .iter()
                .find(|r| r.rule == UPS_ON_BATTERY_RULE)
                .unwrap()
                .alerts;
            assert_eq!(a.len(), 1, "source {source} should fire");
            assert_eq!(a[0].severity, severity, "source {source}");
            assert_eq!(
                a[0].labels.get("output_source").map(String::as_str),
                Some(source.to_string().as_str())
            );
        }
    }

    /// low(3) and depleted(4) fire. **unknown(1) does not**: that is the UPS
    /// saying it does not know, and paging on a missing measurement is the
    /// thing this codebase refuses to do.
    #[test]
    fn battery_low_fires_but_unknown_is_not_a_fault() {
        for (status, expect) in [(2, 0), (3, 1), (4, 1), (1, 0)] {
            let mut obs = CycleObservation::default();
            obs.ups.battery_status = Some(status);
            let out = evaluate(
                "ups01",
                &ups_cfg(),
                &obs,
                &mut EvalState::default(),
                Instant::now(),
            );
            assert_eq!(fired(&out, UPS_BATTERY_LOW_RULE), expect, "status {status}");
        }
    }

    /// The runtime floor is site knowledge. Under it fires; **unconfigured
    /// never fires**, and neither does a UPS that does not serve the OID.
    #[test]
    fn runtime_low_needs_both_a_floor_and_a_reading() {
        let mut obs = CycleObservation::default();
        obs.ups.minutes_remaining = Some(4.0);
        let out = evaluate(
            "ups01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, UPS_RUNTIME_LOW_RULE), 1);

        // No configured floor: the default. Inventing one would page every
        // small UPS on the fleet the first time it ran.
        let out = evaluate(
            "ups01",
            &SnmpAlertsConfig::default(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, UPS_RUNTIME_LOW_RULE), 0);

        // A UPS that does not implement the OID is not a UPS with no runtime.
        let out = evaluate(
            "ups01",
            &ups_cfg(),
            &CycleObservation::default(),
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, UPS_RUNTIME_LOW_RULE), 0);
    }

    /// An outlet that should be on and is not. An outlet the device never
    /// reported — a typo'd id, a dropped module — is **not** an outage.
    #[test]
    fn outlet_off_fires_only_for_an_outlet_the_device_reported() {
        let mut obs = CycleObservation::default();
        obs.outlets.insert(
            "3".to_string(),
            OutletObservation {
                name: Some("db-primary".to_string()),
                on: Some(false),
            },
        );
        // "1.4" is in expect_on but absent from the sweep.
        let out = evaluate(
            "pdu01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        let a = &out
            .iter()
            .find(|r| r.rule == PDU_OUTLET_OFF_RULE)
            .unwrap()
            .alerts;
        assert_eq!(a.len(), 1);
        assert!(a[0].summary.contains("db-primary"), "{}", a[0].summary);
        assert_eq!(a[0].labels.get("outlet").map(String::as_str), Some("3"));

        // Mid-transition (Eaton pendingOn, Raritan cycling) is not off.
        obs.outlets.get_mut("3").unwrap().on = None;
        let out = evaluate(
            "pdu01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, PDU_OUTLET_OFF_RULE), 0);
    }

    /// Two vendors say "overloaded" two ways. The device's own verdict wins
    /// where it exists — it is measured against a rating we do not know.
    #[test]
    fn overload_reads_a_percentage_or_the_device_own_verdict() {
        let mut obs = CycleObservation::default();
        obs.pdu.percent_load = Some(91.0);
        let out = evaluate(
            "pdu01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, PDU_OVERLOAD_RULE), 1);

        let mut obs = CycleObservation::default();
        obs.pdu.overloaded = Some(true);
        let out = evaluate(
            "pdu01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, PDU_OVERLOAD_RULE), 1);

        // Under the limit, and the device says it is fine.
        let mut obs = CycleObservation::default();
        obs.pdu.percent_load = Some(41.0);
        obs.pdu.overloaded = Some(false);
        let out = evaluate(
            "pdu01",
            &ups_cfg(),
            &obs,
            &mut EvalState::default(),
            Instant::now(),
        );
        assert_eq!(fired(&out, PDU_OVERLOAD_RULE), 0);
    }

    /// The vendor OID tables, exercised through `ingest` with the real column
    /// OIDs — the numbers verified against PowerNet-MIB v4.5.8 and against the
    /// OID sets NUT drives Eaton and Raritan hardware with.
    #[test]
    fn power_columns_map_to_one_vendor_neutral_observation() {
        use async_snmp::Value;
        let mut obs = CycleObservation::default();

        // RFC 1628 scalars.
        obs.ingest("1.3.6.1.2.1.33.1.2.1.0", &Value::Integer(3), None);
        obs.ingest("1.3.6.1.2.1.33.1.4.1.0", &Value::Integer(5), None);
        obs.ingest("1.3.6.1.2.1.33.1.2.3.0", &Value::Integer(7), None);
        obs.ingest("1.3.6.1.2.1.33.1.4.4.1.5.1", &Value::Integer(64), None);
        assert_eq!(obs.ups.battery_status, Some(3));
        assert_eq!(obs.ups.output_source, Some(5));
        assert_eq!(obs.ups.minutes_remaining, Some(7.0));
        assert_eq!(obs.ups.percent_load.get(&1), Some(&64.0));

        // APC: single index, on(2)/off(1).
        obs.ingest(
            "1.3.6.1.4.1.318.1.1.26.9.2.3.1.3.4",
            &Value::OctetString(b"web-01".to_vec().into()),
            None,
        );
        obs.ingest(
            "1.3.6.1.4.1.318.1.1.26.9.2.3.1.5.4",
            &Value::Integer(1),
            None,
        );
        assert_eq!(obs.outlets["4"].on, Some(false));
        assert_eq!(obs.outlets["4"].name.as_deref(), Some("web-01"));

        // Eaton: TWO indices (unit.outlet), on(1)/off(0). The composite id is
        // what makes this work without a per-vendor code path.
        obs.ingest(
            "1.3.6.1.4.1.534.6.6.7.6.6.1.2.1.9",
            &Value::Integer(1),
            None,
        );
        assert_eq!(obs.outlets["1.9"].on, Some(true));
        obs.ingest(
            "1.3.6.1.4.1.534.6.6.7.6.6.1.2.1.10",
            &Value::Integer(3),
            None,
        );
        assert_eq!(obs.outlets["1.10"].on, None, "pendingOn is not off");

        // Raritan: single index, on(1)/off(0), cycling(2) is neither.
        obs.ingest("1.3.6.1.4.1.13742.1.2.2.1.3.6", &Value::Integer(2), None);
        assert_eq!(obs.outlets["6"].on, None, "cycling is not off");

        // Whole-PDU load, both dialects.
        obs.ingest(
            "1.3.6.1.4.1.534.6.6.7.3.3.1.11.1.1.1",
            &Value::Gauge32(77),
            None,
        );
        assert_eq!(obs.pdu.percent_load, Some(77.0));
        obs.ingest("1.3.6.1.4.1.318.1.1.26.4.3.1.4.1", &Value::Integer(4), None);
        assert_eq!(obs.pdu.overloaded, Some(true));
    }

    /// APC's `notsupported(5)` is not a verdict. Reading it as "not
    /// overloaded" would be inventing an answer the device declined to give.
    #[test]
    fn a_load_state_of_notsupported_leaves_the_verdict_unset() {
        use async_snmp::Value;
        let mut obs = CycleObservation::default();
        obs.ingest("1.3.6.1.4.1.318.1.1.26.4.3.1.4.1", &Value::Integer(5), None);
        assert_eq!(obs.pdu.overloaded, None);
    }

    /// A device answering neither tree gets every power rule reconciled and
    /// none fired — the shape that lets these rules default to enabled without
    /// costing a switch anything.
    #[test]
    fn a_device_with_no_power_tree_reconciles_every_power_rule_empty() {
        let out = evaluate(
            "sw01",
            &ups_cfg(),
            &CycleObservation::default(),
            &mut EvalState::default(),
            Instant::now(),
        );
        for rule in [
            UPS_ON_BATTERY_RULE,
            UPS_BATTERY_LOW_RULE,
            UPS_RUNTIME_LOW_RULE,
            UPS_LOAD_HIGH_RULE,
            PDU_OUTLET_OFF_RULE,
            PDU_OVERLOAD_RULE,
        ] {
            assert_eq!(fired(&out, rule), 0, "{rule}");
        }
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
