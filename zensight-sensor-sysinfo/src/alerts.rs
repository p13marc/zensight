//! Threshold-based alerting for the sysinfo sensor.
//!
//! sysinfo collects best-in-class saturation/error data (PSI, OOM kills, near-full
//! disks, FD/inode exhaustion, thermal criticals, swap thrash) but historically
//! emitted *no* alerts — unlike snmp/logs/netlink/netring, which all drive a
//! [`AlertReporter`] → `state/<producer>/alert/<key>`. This module closes
//! that gap with config-driven threshold rules evaluated each poll tick.
//!
//! Design (mirrors the netlink sentinel):
//! - The decision logic is a **pure** function [`evaluate`] from already-collected
//!   metrics ([`AlertInputs`]) + thresholds ([`AlertsConfig`]) to a per-rule set of
//!   firing [`Alert`]s — unit-testable with no live session.
//! - [`AlertEvaluator`] owns the rate/delta state (previous counters) and drives
//!   the firing/resolved lifecycle: `observe` every current violation, then
//!   `reconcile` each rule so an alert auto-resolves once its condition clears.
//!
//! Alert keys are kept **stable per condition**: only bucketing labels (resource,
//! mount, fs_type, chip, label, threshold) feed the [`Alert::alert_key`]. The live
//! value lives in the `summary`, never in a key-affecting label, so a metric that
//! oscillates above the threshold updates one alert in place instead of churning a
//! new key every tick (no flapping).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

// Stable rule slugs (one logical rule per slug; reconcile clears recovered keys).
const OOM_RULE: &str = "oom_kills";
const PRESSURE_CPU_RULE: &str = "pressure_cpu";
const PRESSURE_MEMORY_RULE: &str = "pressure_memory";
const PRESSURE_IO_RULE: &str = "pressure_io";
const DISK_USAGE_RULE: &str = "disk_usage";
const DISK_INODE_RULE: &str = "disk_inodes";
const DISK_FILL_RULE: &str = "disk_fill_rate";
const FD_RULE: &str = "fd_exhaustion";
const THERMAL_RULE: &str = "thermal";
const SWAP_RULE: &str = "swap_thrash";

// ===========================================================================
// Configuration
// ===========================================================================

fn default_true() -> bool {
    true
}

/// Top-level alerting configuration (JSON5 `sysinfo.alerts`). Off-by-default
/// rules are those whose input is not collected by the default `collect` config
/// (notably `thermal`, which needs `collect.temperatures = true`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsConfig {
    /// Master switch for all sysinfo threshold alerting (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// "Must be violated continuously for N seconds" debounce before a firing
    /// alert is published (default: 0 = publish on first violation). Applies to
    /// every rule.
    #[serde(default)]
    pub for_secs: u64,

    #[serde(default)]
    pub oom: OomRule,
    #[serde(default)]
    pub pressure: PressureRule,
    #[serde(default)]
    pub disk: WatermarkRule,
    #[serde(default = "WatermarkRule::default_inode")]
    pub inode: WatermarkRule,
    #[serde(default)]
    pub disk_fill_rate: DiskFillRule,
    #[serde(default)]
    pub fd: FdRule,
    #[serde(default)]
    pub thermal: ThermalRule,
    #[serde(default)]
    pub swap: SwapRule,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: 0,
            oom: OomRule::default(),
            pressure: PressureRule::default(),
            disk: WatermarkRule::default(),
            inode: WatermarkRule::default_inode(),
            disk_fill_rate: DiskFillRule::default(),
            fd: FdRule::default(),
            thermal: ThermalRule::default(),
            swap: SwapRule::default(),
        }
    }
}

/// OOM-kill rule: fires Critical whenever new OOM kills occurred since the last
/// poll (`memory/oom_kills_total` delta > 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OomRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for OomRule {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// PSI `some/avg10` rule per resource: Warning at `*_warn`, escalating to
/// Critical at `*_critical`. Memory pressure is graded more aggressively than
/// cpu/io (stall there usually means swapping / imminent OOM).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "psi_cpu_warn")]
    pub cpu_warn: f64,
    #[serde(default = "psi_cpu_critical")]
    pub cpu_critical: f64,
    #[serde(default = "psi_memory_warn")]
    pub memory_warn: f64,
    #[serde(default = "psi_memory_critical")]
    pub memory_critical: f64,
    #[serde(default = "psi_io_warn")]
    pub io_warn: f64,
    #[serde(default = "psi_io_critical")]
    pub io_critical: f64,
}

fn psi_cpu_warn() -> f64 {
    40.0
}
fn psi_cpu_critical() -> f64 {
    70.0
}
fn psi_memory_warn() -> f64 {
    10.0
}
fn psi_memory_critical() -> f64 {
    30.0
}
fn psi_io_warn() -> f64 {
    40.0
}
fn psi_io_critical() -> f64 {
    70.0
}

impl Default for PressureRule {
    fn default() -> Self {
        Self {
            enabled: true,
            cpu_warn: psi_cpu_warn(),
            cpu_critical: psi_cpu_critical(),
            memory_warn: psi_memory_warn(),
            memory_critical: psi_memory_critical(),
            io_warn: psi_io_warn(),
            io_critical: psi_io_critical(),
        }
    }
}

/// A hi-watermark percentage rule (used for disk usage% and inode%): Warning at
/// `warn_percent`, Critical at `critical_percent`, with optional per-mount
/// overrides (#822 — one global pair cannot fit both a scratch disk that lives
/// at 85% by design and a root disk that must never get there).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatermarkRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "watermark_warn")]
    pub warn_percent: f64,
    #[serde(default = "watermark_critical")]
    pub critical_percent: f64,
    /// Per-mount threshold overrides, matched on mount path (exact or glob,
    /// `glob` crate semantics). **First match wins** — put specific entries
    /// before broad globs. A field left out of an override falls back to the
    /// rule-level value, per field.
    #[serde(default)]
    pub mounts: Vec<MountWatermarkOverride>,
}

/// One mount's threshold override. `path` is exact or a glob; an omitted
/// threshold falls back to the rule's global value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountWatermarkOverride {
    pub path: String,
    #[serde(default)]
    pub warn_percent: Option<f64>,
    #[serde(default)]
    pub critical_percent: Option<f64>,
}

fn watermark_warn() -> f64 {
    90.0
}
fn watermark_critical() -> f64 {
    95.0
}

impl Default for WatermarkRule {
    fn default() -> Self {
        Self {
            enabled: true,
            warn_percent: watermark_warn(),
            critical_percent: watermark_critical(),
            mounts: Vec::new(),
        }
    }
}

impl WatermarkRule {
    /// Inodes use the same defaults as disk space; a distinct constructor keeps
    /// the two rules independently configurable.
    fn default_inode() -> Self {
        Self::default()
    }

    /// The `(warn, critical)` thresholds for `mount`: the first matching
    /// override wins, each absent field falling back to the rule-level value.
    pub fn thresholds_for(&self, mount: &str) -> (f64, f64) {
        for o in &self.mounts {
            if mount_matches(&o.path, mount) {
                return (
                    o.warn_percent.unwrap_or(self.warn_percent),
                    o.critical_percent.unwrap_or(self.critical_percent),
                );
            }
        }
        (self.warn_percent, self.critical_percent)
    }
}

/// Match a mount path against an exact-or-glob pattern (`glob` crate
/// semantics, the `action.rs` precedent). An invalid pattern matches nothing —
/// config validation rejects it at startup, so this arm is unreachable from a
/// validated config.
fn mount_matches(pattern: &str, mount: &str) -> bool {
    match glob::Pattern::new(pattern) {
        Ok(p) => p.matches(mount),
        Err(_) => false,
    }
}

/// Projected time-to-full rule (#822): fit the recent `used_bytes` trend per
/// mount and alert on **hours until full**, not current occupancy. "Full in
/// under 4 hours at the current rate" is actionable at 40% and useless at 95%
/// — precisely backwards from how a watermark behaves.
///
/// Kept deliberately boring: a least-squares fit over `window_secs`, nothing
/// said before `min_window_secs`/`min_samples` of history, nothing said on a
/// flat or negative slope, and a history reset when a mount's usage drops
/// sharply (a reclaim invalidates the trend it was fit to). Absent or
/// insufficient history reads as *not asked*, never as "fine".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskFillRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Regression window, seconds.
    #[serde(default = "fill_window_secs")]
    pub window_secs: u64,
    /// Say nothing until the history spans at least this many seconds.
    #[serde(default = "fill_min_window_secs")]
    pub min_window_secs: u64,
    /// …and holds at least this many samples.
    #[serde(default = "fill_min_samples")]
    pub min_samples: u32,
    /// Warning when the projection is at or under this many hours.
    #[serde(default = "fill_warn_hours")]
    pub warn_hours_to_full: f64,
    /// Critical when the projection is at or under this many hours.
    #[serde(default = "fill_critical_hours")]
    pub critical_hours_to_full: f64,
    /// Per-mount horizon overrides — a scratch volume that legitimately fills
    /// and is swept wants a shorter horizon than a root disk. Same matching
    /// rules as [`MountWatermarkOverride`].
    #[serde(default)]
    pub mounts: Vec<MountFillOverride>,
}

/// One mount's projection-horizon override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountFillOverride {
    pub path: String,
    #[serde(default)]
    pub warn_hours_to_full: Option<f64>,
    #[serde(default)]
    pub critical_hours_to_full: Option<f64>,
}

fn fill_window_secs() -> u64 {
    1800
}
fn fill_min_window_secs() -> u64 {
    600
}
fn fill_min_samples() -> u32 {
    5
}
fn fill_warn_hours() -> f64 {
    24.0
}
fn fill_critical_hours() -> f64 {
    4.0
}

impl Default for DiskFillRule {
    fn default() -> Self {
        Self {
            enabled: true,
            window_secs: fill_window_secs(),
            min_window_secs: fill_min_window_secs(),
            min_samples: fill_min_samples(),
            warn_hours_to_full: fill_warn_hours(),
            critical_hours_to_full: fill_critical_hours(),
            mounts: Vec::new(),
        }
    }
}

impl DiskFillRule {
    /// The `(warn_hours, critical_hours)` horizons for `mount` — first
    /// matching override wins, per-field fallback, like
    /// [`WatermarkRule::thresholds_for`].
    pub fn horizons_for(&self, mount: &str) -> (f64, f64) {
        for o in &self.mounts {
            if mount_matches(&o.path, mount) {
                return (
                    o.warn_hours_to_full.unwrap_or(self.warn_hours_to_full),
                    o.critical_hours_to_full
                        .unwrap_or(self.critical_hours_to_full),
                );
            }
        }
        (self.warn_hours_to_full, self.critical_hours_to_full)
    }
}

/// File-descriptor exhaustion rule: Warning when `fs.file-nr` occupancy crosses
/// `warn_percent` of `fs.file-max`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FdRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "fd_warn")]
    pub warn_percent: f64,
}

fn fd_warn() -> f64 {
    80.0
}

impl Default for FdRule {
    fn default() -> Self {
        Self {
            enabled: true,
            warn_percent: fd_warn(),
        }
    }
}

/// Thermal rule: Critical when a sensor reaches `fraction` of its reported
/// critical trip point. **Off by default** because it needs
/// `collect.temperatures = true` (critical trip points aren't collected
/// otherwise).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThermalRule {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "thermal_fraction")]
    pub fraction: f64,
}

fn thermal_fraction() -> f64 {
    0.9
}

impl Default for ThermalRule {
    fn default() -> Self {
        Self {
            enabled: false,
            fraction: thermal_fraction(),
        }
    }
}

/// Swap-thrash rule: Warning when the combined `pswpin + pswpout` rate exceeds
/// `warn_pages_per_sec`. A page is typically 4 KiB, so the 1000 pages/s default
/// is roughly 4 MiB/s of sustained swap traffic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "swap_warn")]
    pub warn_pages_per_sec: f64,
}

fn swap_warn() -> f64 {
    1000.0
}

impl Default for SwapRule {
    fn default() -> Self {
        Self {
            enabled: true,
            warn_pages_per_sec: swap_warn(),
        }
    }
}

// ===========================================================================
// Inputs
// ===========================================================================

/// One filesystem's occupancy (used for both disk-space and inode watermarks).
#[derive(Debug, Clone, PartialEq)]
pub struct DiskUsageInput {
    pub mount: String,
    pub fs_type: String,
    pub used_percent: f64,
}

/// One filesystem's occupancy in **bytes** (feeds the fill-rate tracker; the
/// percent-based [`DiskUsageInput`] stays for the watermarks — projecting a
/// time-to-full needs the absolute headroom, which a percentage has thrown
/// away).
#[derive(Debug, Clone, PartialEq)]
pub struct DiskBytesInput {
    pub mount: String,
    pub fs_type: String,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// One mount's fill projection for this tick (`None` = not enough history, or
/// not growing — either way, nothing to say).
#[derive(Debug, Clone, PartialEq)]
pub struct DiskFillInput {
    pub mount: String,
    pub fs_type: String,
    pub hours_to_full: Option<f64>,
    pub fill_rate_bytes_per_sec: f64,
}

/// One thermal sensor reading plus its critical trip point (if known).
#[derive(Debug, Clone, PartialEq)]
pub struct ThermalInput {
    pub chip: String,
    pub label: String,
    pub temp_celsius: f64,
    pub critical_celsius: Option<f64>,
}

/// Raw per-tick inputs gathered by the collector, *before* rate/delta derivation.
/// Counters (`*_total`) are turned into deltas/rates by [`derive_inputs`].
#[derive(Debug, Clone, Default)]
pub struct RawInputs {
    pub oom_kill_total: Option<u64>,
    pub pswpin_total: Option<u64>,
    pub pswpout_total: Option<u64>,
    pub psi_cpu_avg10: Option<f64>,
    pub psi_memory_avg10: Option<f64>,
    pub psi_io_avg10: Option<f64>,
    pub disks: Vec<DiskUsageInput>,
    pub inodes: Vec<DiskUsageInput>,
    /// Byte-level occupancy per mount, for the fill-rate tracker.
    pub disk_bytes: Vec<DiskBytesInput>,
    pub fd_used_percent: Option<f64>,
    pub temps: Vec<ThermalInput>,
}

/// Derived per-tick inputs the pure [`evaluate`] consumes. Deltas/rates are
/// `None` on the first tick (no previous sample) so startup never false-fires.
#[derive(Debug, Clone, Default)]
pub struct AlertInputs {
    /// New OOM kills since the previous tick (`memory/oom_kills_total` delta).
    pub oom_kill_delta: Option<u64>,
    pub psi_cpu_avg10: Option<f64>,
    pub psi_memory_avg10: Option<f64>,
    pub psi_io_avg10: Option<f64>,
    pub disks: Vec<DiskUsageInput>,
    pub inodes: Vec<DiskUsageInput>,
    /// Per-mount fill projections from the [`FillRateTracker`].
    pub disk_fill: Vec<DiskFillInput>,
    pub fd_used_percent: Option<f64>,
    pub temps: Vec<ThermalInput>,
    /// Combined `pswpin + pswpout` pages per second since the previous tick.
    pub swap_pages_per_sec: Option<f64>,
}

/// Previous cumulative counters, kept across ticks to derive deltas/rates.
#[derive(Debug, Clone, Default)]
pub struct PrevCounters {
    pub oom: Option<u64>,
    pub pswpin: Option<u64>,
    pub pswpout: Option<u64>,
}

/// Turn raw counters into deltas/rates against `prev`, updating `prev` in place.
/// A counter that goes backwards (reboot / wrap) or is absent yields `None`
/// rather than a misleading spike.
pub fn derive_inputs(prev: &mut PrevCounters, raw: RawInputs, interval_secs: f64) -> AlertInputs {
    let oom_kill_delta = match (prev.oom, raw.oom_kill_total) {
        (Some(p), Some(c)) if c >= p => Some(c - p),
        _ => None,
    };
    let swap_pages_per_sec = if interval_secs > 0.0 {
        match (
            prev.pswpin,
            raw.pswpin_total,
            prev.pswpout,
            raw.pswpout_total,
        ) {
            (Some(pi), Some(ci), Some(po), Some(co)) if ci >= pi && co >= po => {
                Some(((ci - pi) + (co - po)) as f64 / interval_secs)
            }
            _ => None,
        }
    } else {
        None
    };

    // Carry forward the latest seen counters (don't clobber with a missing read).
    prev.oom = raw.oom_kill_total.or(prev.oom);
    prev.pswpin = raw.pswpin_total.or(prev.pswpin);
    prev.pswpout = raw.pswpout_total.or(prev.pswpout);

    AlertInputs {
        oom_kill_delta,
        psi_cpu_avg10: raw.psi_cpu_avg10,
        psi_memory_avg10: raw.psi_memory_avg10,
        psi_io_avg10: raw.psi_io_avg10,
        disks: raw.disks,
        inodes: raw.inodes,
        // Filled in by the driver from the [`FillRateTracker`], which owns
        // cross-tick history this by-value derivation cannot.
        disk_fill: Vec::new(),
        fd_used_percent: raw.fd_used_percent,
        temps: raw.temps,
        swap_pages_per_sec,
    }
}

// ===========================================================================
// Fill-rate tracking (#822)
// ===========================================================================

/// Per-mount `used_bytes` history and the projection over it. Owned by
/// [`AlertEvaluator`] next to [`PrevCounters`]; pure in the same way — time is
/// a synthetic clock advanced by the configured `interval_secs` per tick (the
/// swap-rate discipline), so every behavior is unit-testable without sleeping.
#[derive(Debug, Default)]
pub struct FillRateTracker {
    now_secs: f64,
    histories: std::collections::HashMap<String, std::collections::VecDeque<(f64, u64)>>,
}

/// Hard per-mount sample cap, besides the time-window pruning — a config with
/// a huge `window_secs` and a short poll interval must not grow unbounded.
const FILL_HISTORY_CAP: usize = 4096;

/// A drop of more than this fraction of the filesystem between two samples is
/// read as a reclaim (sweep, log rotation, vacuum): the trend the window was
/// fit to no longer describes the disk, so the history resets and the rule
/// stays silent until `min_window_secs` re-accumulates. Milder dips are left
/// to the regression to absorb.
const FILL_RECLAIM_FRACTION: f64 = 0.05;

impl FillRateTracker {
    /// Advance the clock, ingest this tick's byte samples, prune, and project.
    /// Mounts absent from `disks` are dropped from the history (an unmounted
    /// volume must not haunt the tracker).
    pub fn update(
        &mut self,
        cfg: &DiskFillRule,
        disks: &[DiskBytesInput],
        interval_secs: f64,
    ) -> Vec<DiskFillInput> {
        self.now_secs += interval_secs.max(0.0);
        let now = self.now_secs;

        self.histories
            .retain(|mount, _| disks.iter().any(|d| d.mount == *mount));

        let mut out = Vec::with_capacity(disks.len());
        for d in disks {
            let h = self.histories.entry(d.mount.clone()).or_default();
            if let Some(&(_, last_used)) = h.back()
                && last_used.saturating_sub(d.used_bytes) as f64
                    > d.total_bytes as f64 * FILL_RECLAIM_FRACTION
            {
                h.clear();
            }
            h.push_back((now, d.used_bytes));
            while let Some(&(t, _)) = h.front()
                && (now - t > cfg.window_secs as f64 || h.len() > FILL_HISTORY_CAP)
            {
                h.pop_front();
            }

            let (hours_to_full, rate) = match project_hours_to_full(
                h.make_contiguous(),
                d.total_bytes,
                cfg.min_window_secs as f64,
                cfg.min_samples as usize,
            ) {
                Some((hours, rate)) => (Some(hours), rate),
                None => (None, 0.0),
            };
            out.push(DiskFillInput {
                mount: d.mount.clone(),
                fs_type: d.fs_type.clone(),
                hours_to_full,
                fill_rate_bytes_per_sec: rate,
            });
        }
        out
    }
}

/// Least-squares fit over `(t_secs, used_bytes)` → `(hours_to_full,
/// slope_bytes_per_sec)`. `None` unless the history spans `min_window_secs`,
/// holds `min_samples`, and the slope is positive — insufficient history and a
/// flat/shrinking disk are both *not asked*, never "fine".
fn project_hours_to_full(
    samples: &[(f64, u64)],
    total_bytes: u64,
    min_window_secs: f64,
    min_samples: usize,
) -> Option<(f64, f64)> {
    if samples.len() < min_samples.max(2) {
        return None;
    }
    let span = samples.last()?.0 - samples.first()?.0;
    if span < min_window_secs {
        return None;
    }
    let n = samples.len() as f64;
    let (mut st, mut su) = (0.0, 0.0);
    for &(t, u) in samples {
        st += t;
        su += u as f64;
    }
    let (t_mean, u_mean) = (st / n, su / n);
    let (mut num, mut den) = (0.0, 0.0);
    for &(t, u) in samples {
        num += (t - t_mean) * (u as f64 - u_mean);
        den += (t - t_mean) * (t - t_mean);
    }
    if den <= 0.0 {
        return None;
    }
    let slope = num / den; // bytes per second
    if slope <= 0.0 {
        return None;
    }
    let latest_used = samples.last()?.1;
    let headroom = total_bytes.saturating_sub(latest_used) as f64;
    Some((headroom / slope / 3600.0, slope))
}

// ===========================================================================
// Pure evaluation
// ===========================================================================

/// The firing alerts for a single rule this tick. An entry is emitted for every
/// *enabled* rule even when `alerts` is empty, so the driver can `reconcile` and
/// auto-resolve a previously-firing alert whose condition has cleared.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleAlerts {
    pub rule: String,
    pub alerts: Vec<Alert>,
}

/// Grade a value against a `warn`/`critical` watermark (critical takes priority).
fn level(value: f64, warn: f64, critical: f64) -> Option<AlertSeverity> {
    if value >= critical {
        Some(AlertSeverity::Critical)
    } else if value >= warn {
        Some(AlertSeverity::Warning)
    } else {
        None
    }
}

/// Build a sysinfo saturation alert. All sysinfo threshold alerts use
/// [`AlertKind::SensorHealth`] (host resource health), as the issue specifies.
fn mk(host: &str, rule: &str, severity: AlertSeverity, summary: String) -> Alert {
    Alert::new(
        host,
        Protocol::Sysinfo,
        AlertKind::SensorHealth,
        rule,
        severity,
        summary,
    )
}

/// Evaluate every enabled rule against the derived inputs. Pure: no I/O, no
/// session — fully unit-testable on synthetic [`AlertInputs`].
pub fn evaluate(host: &str, cfg: &AlertsConfig, inputs: &AlertInputs) -> Vec<RuleAlerts> {
    let mut out = Vec::new();

    // OOM kills (Critical on any new kill).
    if cfg.oom.enabled {
        let mut alerts = Vec::new();
        if let Some(delta) = inputs.oom_kill_delta
            && delta > 0
        {
            let plural = if delta == 1 { "" } else { "s" };
            alerts.push(
                mk(
                    host,
                    OOM_RULE,
                    AlertSeverity::Critical,
                    format!("{delta} new OOM kill{plural} since last poll"),
                )
                .with_label("resource", "memory"),
            );
        }
        out.push(RuleAlerts {
            rule: OOM_RULE.into(),
            alerts,
        });
    }

    // PSI some/avg10 per resource (Warning → Critical).
    if cfg.pressure.enabled {
        for (res, rule, avg10, warn, crit) in [
            (
                "cpu",
                PRESSURE_CPU_RULE,
                inputs.psi_cpu_avg10,
                cfg.pressure.cpu_warn,
                cfg.pressure.cpu_critical,
            ),
            (
                "memory",
                PRESSURE_MEMORY_RULE,
                inputs.psi_memory_avg10,
                cfg.pressure.memory_warn,
                cfg.pressure.memory_critical,
            ),
            (
                "io",
                PRESSURE_IO_RULE,
                inputs.psi_io_avg10,
                cfg.pressure.io_warn,
                cfg.pressure.io_critical,
            ),
        ] {
            let mut alerts = Vec::new();
            if let Some(v) = avg10
                && let Some(sev) = level(v, warn, crit)
            {
                alerts.push(
                    mk(
                        host,
                        rule,
                        sev,
                        format!(
                            "{res} pressure some/avg10 {v:.1}% (warn {warn:.0}%, crit {crit:.0}%)"
                        ),
                    )
                    .with_label("resource", res)
                    .with_label("scope", "some")
                    .with_label("threshold", format!("{warn:.0}")),
                );
            }
            out.push(RuleAlerts {
                rule: rule.into(),
                alerts,
            });
        }
    }

    // Disk space + inode hi-watermarks (per mount).
    if cfg.disk.enabled {
        out.push(watermark_rule(
            host,
            DISK_USAGE_RULE,
            "space",
            &cfg.disk,
            &inputs.disks,
        ));
    }
    if cfg.inode.enabled {
        out.push(watermark_rule(
            host,
            DISK_INODE_RULE,
            "inodes",
            &cfg.inode,
            &inputs.inodes,
        ));
    }

    // Projected time-to-full (#822). Emitted whenever enabled — an empty
    // entry is what lets reconcile clear a projection that recovered.
    if cfg.disk_fill_rate.enabled {
        out.push(fill_rate_rule(host, &cfg.disk_fill_rate, &inputs.disk_fill));
    }

    // File-descriptor exhaustion (Warning).
    if cfg.fd.enabled {
        let mut alerts = Vec::new();
        if let Some(pct) = inputs.fd_used_percent
            && pct >= cfg.fd.warn_percent
        {
            alerts.push(
                mk(
                    host,
                    FD_RULE,
                    AlertSeverity::Warning,
                    format!(
                        "file-descriptor table {pct:.1}% full (threshold {:.0}%)",
                        cfg.fd.warn_percent
                    ),
                )
                .with_label("resource", "file_descriptors")
                .with_label("threshold", format!("{:.0}", cfg.fd.warn_percent)),
            );
        }
        out.push(RuleAlerts {
            rule: FD_RULE.into(),
            alerts,
        });
    }

    // Thermal critical (only when temperatures are collected; off by default).
    if cfg.thermal.enabled {
        let mut alerts = Vec::new();
        for t in &inputs.temps {
            if let Some(crit) = t.critical_celsius
                && crit > 0.0
                && t.temp_celsius >= crit * cfg.thermal.fraction
            {
                alerts.push(
                    mk(
                        host,
                        THERMAL_RULE,
                        AlertSeverity::Critical,
                        format!(
                            "{}/{} at {:.1}\u{b0}C (\u{2265} {:.0}% of critical {:.1}\u{b0}C)",
                            t.chip,
                            t.label,
                            t.temp_celsius,
                            cfg.thermal.fraction * 100.0,
                            crit
                        ),
                    )
                    .with_label("chip", t.chip.clone())
                    .with_label("label", t.label.clone())
                    .with_label("critical", format!("{crit:.1}")),
                );
            }
        }
        out.push(RuleAlerts {
            rule: THERMAL_RULE.into(),
            alerts,
        });
    }

    // Swap thrash (Warning).
    if cfg.swap.enabled {
        let mut alerts = Vec::new();
        if let Some(rate) = inputs.swap_pages_per_sec
            && rate >= cfg.swap.warn_pages_per_sec
        {
            alerts.push(
                mk(
                    host,
                    SWAP_RULE,
                    AlertSeverity::Warning,
                    format!(
                        "swap thrash {rate:.0} pages/s (threshold {:.0})",
                        cfg.swap.warn_pages_per_sec
                    ),
                )
                .with_label("resource", "swap")
                .with_label("threshold", format!("{:.0}", cfg.swap.warn_pages_per_sec)),
            );
        }
        out.push(RuleAlerts {
            rule: SWAP_RULE.into(),
            alerts,
        });
    }

    out
}

/// Build the per-mount alerts for a hi-watermark rule (disk space or inodes).
/// `what` is a human word for the summary (`space` / `inodes`).
fn watermark_rule(
    host: &str,
    rule: &str,
    what: &str,
    cfg: &WatermarkRule,
    disks: &[DiskUsageInput],
) -> RuleAlerts {
    let mut alerts = Vec::new();
    for d in disks {
        // Per-mount overrides (#822). The resolved warn value feeds the
        // `threshold` label, which is key-affecting — so a changed override
        // retires the old key via reconcile and raises a fresh one, the same
        // documented lifecycle as editing the global threshold.
        let (warn, critical) = cfg.thresholds_for(&d.mount);
        if let Some(sev) = level(d.used_percent, warn, critical) {
            alerts.push(
                mk(
                    host,
                    rule,
                    sev,
                    format!(
                        "{} {} {:.1}% full (threshold {:.0}%)",
                        d.mount, what, d.used_percent, warn
                    ),
                )
                .with_label("mount", d.mount.clone())
                .with_label("fs_type", d.fs_type.clone())
                .with_label("threshold", format!("{warn:.0}")),
            );
        }
    }
    RuleAlerts {
        rule: rule.into(),
        alerts,
    }
}

/// Grade a projected time-to-full against warn/critical **horizons** — lower
/// is worse, the mirror of [`level`].
fn fill_level(hours: f64, warn: f64, critical: f64) -> Option<AlertSeverity> {
    if hours <= critical {
        Some(AlertSeverity::Critical)
    } else if hours <= warn {
        Some(AlertSeverity::Warning)
    } else {
        None
    }
}

/// Build the per-mount alerts for the fill-rate rule (#822). Volatile values
/// (the projection, the rate) live in the summary only; the key-affecting
/// `threshold` label carries the resolved warn horizon, so a projection
/// drifting from 3.9h to 2.1h updates one alert in place. No extra hysteresis:
/// the regression window is itself the smoother, and `reconcile` clears a
/// projection that lengthens back past the horizon.
fn fill_rate_rule(host: &str, cfg: &DiskFillRule, disks: &[DiskFillInput]) -> RuleAlerts {
    let mut alerts = Vec::new();
    for d in disks {
        let Some(hours) = d.hours_to_full else {
            continue; // not asked — insufficient history, or not growing
        };
        let (warn, critical) = cfg.horizons_for(&d.mount);
        if let Some(sev) = fill_level(hours, warn, critical) {
            let rate_mib_h = d.fill_rate_bytes_per_sec * 3600.0 / (1024.0 * 1024.0);
            alerts.push(
                mk(
                    host,
                    DISK_FILL_RULE,
                    sev,
                    format!(
                        "{} projected full in {hours:.1}h at {rate_mib_h:.0} MiB/h \
                         (warn {warn:.0}h, crit {critical:.0}h)",
                        d.mount
                    ),
                )
                .with_label("mount", d.mount.clone())
                .with_label("fs_type", d.fs_type.clone())
                .with_label("threshold", format!("{warn:.0}")),
            );
        }
    }
    RuleAlerts {
        rule: DISK_FILL_RULE.into(),
        alerts,
    }
}

// ===========================================================================
// Live evaluator (drives the AlertReporter lifecycle)
// ===========================================================================

/// Owns rate/delta state and the [`AlertReporter`]; drives the firing/resolved
/// lifecycle each poll tick (mirrors the netlink sentinel's `report`).
pub struct AlertEvaluator {
    host: String,
    cfg: AlertsConfig,
    reporter: Arc<AlertReporter>,
    prev: PrevCounters,
    fill: FillRateTracker,
}

impl AlertEvaluator {
    /// Create an evaluator. The reporter should already carry the configured
    /// debounce (via [`AlertReporter::with_debounce`]).
    pub fn new(host: String, cfg: AlertsConfig, reporter: Arc<AlertReporter>) -> Self {
        Self {
            host,
            cfg,
            reporter,
            prev: PrevCounters::default(),
            fill: FillRateTracker::default(),
        }
    }

    /// Whether alerting is enabled at all.
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Evaluate the raw inputs for this tick and reconcile the alert set: every
    /// current violation is `observe`d, then each rule is `reconcile`d so cleared
    /// conditions auto-resolve.
    pub async fn tick(&mut self, raw: RawInputs, interval_secs: f64) {
        if !self.cfg.enabled {
            return;
        }
        // The tracker consumes the byte samples before `derive_inputs` takes
        // `raw` by value; its projections ride into the pure evaluation.
        let disk_fill = if self.cfg.disk_fill_rate.enabled {
            self.fill
                .update(&self.cfg.disk_fill_rate, &raw.disk_bytes, interval_secs)
        } else {
            Vec::new()
        };
        let mut inputs = derive_inputs(&mut self.prev, raw, interval_secs);
        inputs.disk_fill = disk_fill;
        let for_duration = if self.cfg.for_secs > 0 {
            Some(Duration::from_secs(self.cfg.for_secs))
        } else {
            None
        };
        for ra in evaluate(&self.host, &self.cfg, &inputs) {
            let mut firing_keys = Vec::with_capacity(ra.alerts.len());
            for alert in ra.alerts {
                firing_keys.push(alert.alert_key());
                if let Err(e) = self.reporter.observe(alert, for_duration).await {
                    warn!(error = %e, rule = %ra.rule, "sysinfo: failed to publish alert");
                }
            }
            if let Err(e) = self.reporter.reconcile(&ra.rule, &firing_keys).await {
                warn!(error = %e, rule = %ra.rule, "sysinfo: failed to reconcile alerts");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "host01";

    /// Evaluate and return the firing alerts for one rule (owned, so callers
    /// don't borrow a temporary).
    fn rule_alerts(cfg: &AlertsConfig, inputs: &AlertInputs, rule: &str) -> Vec<Alert> {
        evaluate(HOST, cfg, inputs)
            .into_iter()
            .find(|r| r.rule == rule)
            .unwrap_or_else(|| panic!("rule {rule} missing from output"))
            .alerts
    }

    fn has_rule(cfg: &AlertsConfig, inputs: &AlertInputs, rule: &str) -> bool {
        evaluate(HOST, cfg, inputs).iter().any(|r| r.rule == rule)
    }

    #[test]
    fn oom_fires_only_on_positive_delta() {
        let cfg = AlertsConfig::default();
        // No delta yet (first tick) → no alert, but the rule is still present.
        assert!(rule_alerts(&cfg, &AlertInputs::default(), OOM_RULE).is_empty());

        let inputs = AlertInputs {
            oom_kill_delta: Some(0),
            ..Default::default()
        };
        assert!(rule_alerts(&cfg, &inputs, OOM_RULE).is_empty());

        let inputs = AlertInputs {
            oom_kill_delta: Some(2),
            ..Default::default()
        };
        let a = rule_alerts(&cfg, &inputs, OOM_RULE);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].severity, AlertSeverity::Critical);
        assert_eq!(a[0].kind, AlertKind::SensorHealth);
    }

    #[test]
    fn pressure_grades_warning_then_critical() {
        let cfg = AlertsConfig::default(); // cpu warn 40, crit 70
        let under = AlertInputs {
            psi_cpu_avg10: Some(10.0),
            ..Default::default()
        };
        assert!(rule_alerts(&cfg, &under, PRESSURE_CPU_RULE).is_empty());

        let warn = AlertInputs {
            psi_cpu_avg10: Some(50.0),
            ..Default::default()
        };
        assert_eq!(
            rule_alerts(&cfg, &warn, PRESSURE_CPU_RULE)[0].severity,
            AlertSeverity::Warning
        );

        let crit = AlertInputs {
            psi_cpu_avg10: Some(85.0),
            ..Default::default()
        };
        assert_eq!(
            rule_alerts(&cfg, &crit, PRESSURE_CPU_RULE)[0].severity,
            AlertSeverity::Critical
        );
    }

    #[test]
    fn disk_and_inode_watermarks_per_mount() {
        let cfg = AlertsConfig::default(); // warn 90, crit 95
        let inputs = AlertInputs {
            disks: vec![
                DiskUsageInput {
                    mount: "/".into(),
                    fs_type: "ext4".into(),
                    used_percent: 50.0, // ok
                },
                DiskUsageInput {
                    mount: "/data".into(),
                    fs_type: "xfs".into(),
                    used_percent: 92.0, // warn
                },
                DiskUsageInput {
                    mount: "/boot".into(),
                    fs_type: "vfat".into(),
                    used_percent: 99.0, // critical
                },
            ],
            ..Default::default()
        };
        let a = rule_alerts(&cfg, &inputs, DISK_USAGE_RULE);
        assert_eq!(a.len(), 2);
        let data = a.iter().find(|x| x.summary.starts_with("/data")).unwrap();
        assert_eq!(data.severity, AlertSeverity::Warning);
        assert!(data.labels.get("mount").is_some_and(|m| m == "/data"));
        let boot = a.iter().find(|x| x.summary.starts_with("/boot")).unwrap();
        assert_eq!(boot.severity, AlertSeverity::Critical);
    }

    // ---- per-mount overrides (#822) ----

    fn over(path: &str, warn: Option<f64>, crit: Option<f64>) -> MountWatermarkOverride {
        MountWatermarkOverride {
            path: path.into(),
            warn_percent: warn,
            critical_percent: crit,
        }
    }

    #[test]
    fn mount_override_exact_glob_and_first_match() {
        let rule = WatermarkRule {
            mounts: vec![
                over("/", Some(75.0), Some(85.0)),
                over("/srv/dev", Some(92.0), Some(97.0)),
                over("/srv/*", Some(50.0), Some(60.0)),
            ],
            ..Default::default() // 90/95 globals
        };
        // Exact `/` matches only `/` — glob semantics, not a prefix.
        assert_eq!(rule.thresholds_for("/"), (75.0, 85.0));
        // First match wins: the specific entry shadows the glob behind it.
        assert_eq!(rule.thresholds_for("/srv/dev"), (92.0, 97.0));
        assert_eq!(rule.thresholds_for("/srv/backups"), (50.0, 60.0));
        // No match falls back to the globals.
        assert_eq!(rule.thresholds_for("/var"), (90.0, 95.0));
    }

    #[test]
    fn mount_override_partial_fields_fall_back_per_field() {
        let rule = WatermarkRule {
            mounts: vec![over("/", Some(75.0), None)],
            ..Default::default()
        };
        assert_eq!(rule.thresholds_for("/"), (75.0, 95.0));
    }

    #[test]
    fn invalid_glob_matches_nothing() {
        // Rejected by config validation at startup; the matcher itself must
        // still fail closed rather than panic.
        assert!(!mount_matches("[", "/"));
    }

    #[test]
    fn disk_watermark_honors_per_mount_override_and_inode_stays_global() {
        let mut cfg = AlertsConfig::default();
        cfg.disk.mounts = vec![over("/", Some(75.0), Some(85.0))];
        let d = |mount: &str, pct: f64| DiskUsageInput {
            mount: mount.into(),
            fs_type: "ext4".into(),
            used_percent: pct,
        };
        let inputs = AlertInputs {
            disks: vec![d("/", 80.0), d("/data", 80.0)],
            inodes: vec![d("/", 80.0)],
            ..Default::default()
        };
        // `/` fires Warning under its override; `/data` at the same 80% stays
        // silent under the global 90.
        let a = rule_alerts(&cfg, &inputs, DISK_USAGE_RULE);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].labels["mount"], "/");
        assert_eq!(a[0].severity, AlertSeverity::Warning);
        assert_eq!(a[0].labels["threshold"], "75");
        // The inode rule carries its own (empty) override list: nothing fires.
        assert!(rule_alerts(&cfg, &inputs, DISK_INODE_RULE).is_empty());
    }

    #[test]
    fn a_changed_override_retires_the_old_alert_key() {
        // The `threshold` label is key-affecting by design: editing a mount's
        // threshold retires the old key (via reconcile) and raises a fresh
        // one, the same lifecycle as editing the global value.
        let d = DiskUsageInput {
            mount: "/".into(),
            fs_type: "ext4".into(),
            used_percent: 96.0,
        };
        let mut cfg = AlertsConfig::default();
        let inputs = AlertInputs {
            disks: vec![d],
            ..Default::default()
        };
        let before = rule_alerts(&cfg, &inputs, DISK_USAGE_RULE)[0].alert_key();
        cfg.disk.mounts = vec![over("/", Some(75.0), Some(85.0))];
        let after = rule_alerts(&cfg, &inputs, DISK_USAGE_RULE)[0].alert_key();
        assert_ne!(before, after);
    }

    // ---- fill-rate projection (#822) ----

    fn bytes_input(mount: &str, used: u64, total: u64) -> DiskBytesInput {
        DiskBytesInput {
            mount: mount.into(),
            fs_type: "ext4".into(),
            used_bytes: used,
            total_bytes: total,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn fill_projection_needs_enough_history() {
        // 4 samples over 3 windows of 60s: fails min_samples=5 and (span
        // 180s) min_window_secs=600 — both read as "not asked".
        let samples: Vec<(f64, u64)> = (0..4).map(|i| (i as f64 * 60.0, GIB * i)).collect();
        assert_eq!(project_hours_to_full(&samples, 100 * GIB, 600.0, 5), None);
    }

    #[test]
    fn fill_projection_flat_or_shrinking_is_none() {
        let flat: Vec<(f64, u64)> = (0..10).map(|i| (i as f64 * 120.0, 50 * GIB)).collect();
        assert_eq!(project_hours_to_full(&flat, 100 * GIB, 600.0, 5), None);
        let shrinking: Vec<(f64, u64)> = (0..10)
            .map(|i| (i as f64 * 120.0, (60 - i) * GIB))
            .collect();
        assert_eq!(project_hours_to_full(&shrinking, 100 * GIB, 600.0, 5), None);
    }

    #[test]
    fn fill_projection_linear_growth_projects_the_right_hours() {
        // 1 GiB/minute on a 100 GiB disk, 50 GiB used at the last sample:
        // 50 GiB headroom / (1 GiB/60s) = 3000s ≈ 0.833h.
        let samples: Vec<(f64, u64)> = (0..11).map(|i| (i as f64 * 60.0, (40 + i) * GIB)).collect();
        let (hours, rate) = project_hours_to_full(&samples, 100 * GIB, 600.0, 5).expect("projects");
        assert!((hours - 50.0 * 60.0 / 3600.0).abs() < 0.01, "{hours}");
        assert!((rate - GIB as f64 / 60.0).abs() < 1.0, "{rate}");
    }

    #[test]
    fn fill_tracker_resets_on_reclaim_and_prunes_vanished_mounts() {
        let cfg = DiskFillRule {
            min_window_secs: 120,
            min_samples: 3,
            ..Default::default()
        };
        let mut tracker = FillRateTracker::default();
        // Steady growth: 1 GiB per 60s tick until a projection appears.
        for i in 0..5u64 {
            tracker.update(&cfg, &[bytes_input("/", (40 + i) * GIB, 100 * GIB)], 60.0);
        }
        let out = tracker.update(&cfg, &[bytes_input("/", 45 * GIB, 100 * GIB)], 60.0);
        assert!(out[0].hours_to_full.is_some(), "trend established");

        // The nightly sweep: 20% of the disk reclaimed at once. The trend is
        // invalidated — silence until the minimum window re-accumulates.
        let out = tracker.update(&cfg, &[bytes_input("/", 25 * GIB, 100 * GIB)], 60.0);
        assert_eq!(out[0].hours_to_full, None, "reclaim resets the history");
        let out = tracker.update(&cfg, &[bytes_input("/", 26 * GIB, 100 * GIB)], 60.0);
        assert_eq!(out[0].hours_to_full, None, "still under the min window");

        // A mount that vanishes takes its history with it.
        tracker.update(&cfg, &[bytes_input("/other", GIB, 100 * GIB)], 60.0);
        assert!(!tracker.histories.contains_key("/"));
    }

    #[test]
    fn fill_rule_emits_empty_for_reconcile_and_honors_horizons() {
        let cfg = AlertsConfig::default(); // warn 24h, crit 4h
        // No projections: the rule entry must still be present (empty) so a
        // recovered projection reconciles away.
        let inputs = AlertInputs::default();
        assert!(rule_alerts(&cfg, &inputs, DISK_FILL_RULE).is_empty());

        let fill = |mount: &str, hours: Option<f64>| DiskFillInput {
            mount: mount.into(),
            fs_type: "ext4".into(),
            hours_to_full: hours,
            fill_rate_bytes_per_sec: 1024.0 * 1024.0,
        };
        let inputs = AlertInputs {
            disk_fill: vec![
                fill("/", Some(3.0)),      // critical (≤ 4h)
                fill("/data", Some(12.0)), // warning (≤ 24h)
                fill("/srv", Some(48.0)),  // beyond the horizon
                fill("/tmp", None),        // not asked
            ],
            ..Default::default()
        };
        let a = rule_alerts(&cfg, &inputs, DISK_FILL_RULE);
        assert_eq!(a.len(), 2, "{a:?}");
        let root = a.iter().find(|x| x.labels["mount"] == "/").unwrap();
        assert_eq!(root.severity, AlertSeverity::Critical);
        let data = a.iter().find(|x| x.labels["mount"] == "/data").unwrap();
        assert_eq!(data.severity, AlertSeverity::Warning);
    }

    #[test]
    fn fill_alert_key_is_stable_as_the_projection_moves() {
        let cfg = AlertsConfig::default();
        let fill = |hours: f64| AlertInputs {
            disk_fill: vec![DiskFillInput {
                mount: "/".into(),
                fs_type: "ext4".into(),
                hours_to_full: Some(hours),
                fill_rate_bytes_per_sec: 500.0,
            }],
            ..Default::default()
        };
        // 3.9h vs 2.1h: same key — the live value lives in the summary, so
        // the projection drifting toward full updates one alert in place.
        let a = rule_alerts(&cfg, &fill(3.9), DISK_FILL_RULE)[0].alert_key();
        let b = rule_alerts(&cfg, &fill(2.1), DISK_FILL_RULE)[0].alert_key();
        assert_eq!(a, b);
    }

    #[test]
    fn fill_horizons_take_per_mount_overrides() {
        let rule = DiskFillRule {
            mounts: vec![MountFillOverride {
                path: "/srv/*".into(),
                warn_hours_to_full: Some(6.0),
                critical_hours_to_full: None,
            }],
            ..Default::default()
        };
        assert_eq!(rule.horizons_for("/srv/dev"), (6.0, 4.0));
        assert_eq!(rule.horizons_for("/"), (24.0, 4.0));
    }

    #[test]
    fn fd_warns_over_threshold() {
        let cfg = AlertsConfig::default();
        let ok = AlertInputs {
            fd_used_percent: Some(50.0),
            ..Default::default()
        };
        assert!(rule_alerts(&cfg, &ok, FD_RULE).is_empty());
        let hot = AlertInputs {
            fd_used_percent: Some(85.0),
            ..Default::default()
        };
        assert_eq!(
            rule_alerts(&cfg, &hot, FD_RULE)[0].severity,
            AlertSeverity::Warning
        );
    }

    #[test]
    fn thermal_off_by_default_on_by_opt_in() {
        let inputs = AlertInputs {
            temps: vec![ThermalInput {
                chip: "coretemp".into(),
                label: "Core 0".into(),
                temp_celsius: 95.0,
                critical_celsius: Some(100.0),
            }],
            ..Default::default()
        };
        // Default: thermal disabled → the rule isn't even emitted.
        let cfg = AlertsConfig::default();
        assert!(!has_rule(&cfg, &inputs, THERMAL_RULE));

        // Opt in: 95 >= 0.9 * 100 → Critical.
        let mut cfg = AlertsConfig::default();
        cfg.thermal.enabled = true;
        let a = rule_alerts(&cfg, &inputs, THERMAL_RULE);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].severity, AlertSeverity::Critical);

        // Below the 0.9 fraction → no alert.
        let cool = AlertInputs {
            temps: vec![ThermalInput {
                chip: "coretemp".into(),
                label: "Core 0".into(),
                temp_celsius: 80.0,
                critical_celsius: Some(100.0),
            }],
            ..Default::default()
        };
        assert!(rule_alerts(&cfg, &cool, THERMAL_RULE).is_empty());
    }

    #[test]
    fn swap_thrash_warns_over_rate() {
        let cfg = AlertsConfig::default(); // 1000 pages/s
        let calm = AlertInputs {
            swap_pages_per_sec: Some(100.0),
            ..Default::default()
        };
        assert!(rule_alerts(&cfg, &calm, SWAP_RULE).is_empty());
        let thrash = AlertInputs {
            swap_pages_per_sec: Some(5000.0),
            ..Default::default()
        };
        assert_eq!(
            rule_alerts(&cfg, &thrash, SWAP_RULE)[0].severity,
            AlertSeverity::Warning
        );
    }

    #[test]
    fn alert_key_is_stable_across_ticks_no_flap() {
        // The same condition with a *different* live value must keep the same
        // alert_key so the reporter updates in place instead of churning keys.
        let cfg = AlertsConfig::default();
        let t1 = AlertInputs {
            disks: vec![DiskUsageInput {
                mount: "/data".into(),
                fs_type: "xfs".into(),
                used_percent: 91.0,
            }],
            ..Default::default()
        };
        let t2 = AlertInputs {
            disks: vec![DiskUsageInput {
                mount: "/data".into(),
                fs_type: "xfs".into(),
                used_percent: 93.5, // different value, same condition
            }],
            ..Default::default()
        };
        let k1 = rule_alerts(&cfg, &t1, DISK_USAGE_RULE)[0].alert_key();
        let k2 = rule_alerts(&cfg, &t2, DISK_USAGE_RULE)[0].alert_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn disabled_individual_rule_is_omitted() {
        // A disabled individual rule must not appear even when its input is hot.
        let cfg = AlertsConfig {
            oom: OomRule { enabled: false },
            ..AlertsConfig::default()
        };
        let inputs = AlertInputs {
            oom_kill_delta: Some(3),
            ..Default::default()
        };
        assert!(!has_rule(&cfg, &inputs, OOM_RULE));
    }

    #[test]
    fn derive_inputs_no_false_fire_on_first_tick() {
        let mut prev = PrevCounters::default();
        let raw = RawInputs {
            oom_kill_total: Some(5),
            pswpin_total: Some(100),
            pswpout_total: Some(200),
            ..Default::default()
        };
        let d = derive_inputs(&mut prev, raw, 5.0);
        // First observation: nothing to delta against.
        assert_eq!(d.oom_kill_delta, None);
        assert_eq!(d.swap_pages_per_sec, None);

        // Second tick: 7-5 = 2 new kills; (150-100)+(260-200)=110 pages over 5s = 22/s.
        let raw = RawInputs {
            oom_kill_total: Some(7),
            pswpin_total: Some(150),
            pswpout_total: Some(260),
            ..Default::default()
        };
        let d = derive_inputs(&mut prev, raw, 5.0);
        assert_eq!(d.oom_kill_delta, Some(2));
        assert_eq!(d.swap_pages_per_sec, Some(22.0));
    }

    #[test]
    fn derive_inputs_counter_reset_is_safe() {
        let mut prev = PrevCounters {
            oom: Some(100),
            pswpin: Some(100),
            pswpout: Some(100),
        };
        // Counters went backwards (reboot) → no spurious delta/rate.
        let raw = RawInputs {
            oom_kill_total: Some(1),
            pswpin_total: Some(1),
            pswpout_total: Some(1),
            ..Default::default()
        };
        let d = derive_inputs(&mut prev, raw, 5.0);
        assert_eq!(d.oom_kill_delta, None);
        assert_eq!(d.swap_pages_per_sec, None);
    }
}
