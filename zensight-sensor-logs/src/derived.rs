//! Derived rollup telemetry (#63): cheap, dashboard-friendly aggregates over
//! the log stream — per-severity counts, per-unit (top-N) rates, error/warning
//! rollups, units-in-failure, and journald throughput — emitted on a fixed tick
//! alongside the per-message points.
//!
//! Source-neutral (network syslog + journald). The aggregator observes each
//! message that passes filtering, then [`emit`](LogAggregator::emit) snapshots
//! the accumulators into [`TelemetryPoint`]s. Cardinality is bounded: at most
//! `top_units` distinct units are tracked as their own series; the rest fold
//! into an `other` bucket (never an unbounded label space).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::parser::{Severity, SyslogMessage};
use crate::receiver::JournaldStatsSnapshot;

/// Overflow bucket name for units beyond the cardinality cap.
const OTHER_UNIT: &str = "other";

/// Alert rule slug for per-unit error-budget / SLO burn (#105). One namespace
/// for all units (reconciled together); the offending unit is an alert label,
/// so dedup is effectively by `(rule, unit)`.
pub const BUDGET_RULE: &str = "log-error-budget";

/// Multiplier over the burn threshold at which a budget alert escalates from
/// `Warning` to `Critical` (the unit is *deeply* over budget, not just nudging
/// past it).
const CRITICAL_BURN_FACTOR: f64 = 2.0;

/// Per-unit cumulative counters.
#[derive(Debug, Default, Clone, Copy)]
struct UnitCounts {
    messages: u64,
    errors: u64,
}

#[derive(Debug, Default)]
struct Inner {
    /// Cumulative count per severity code (0=emergency .. 7=debug).
    by_severity: [u64; 8],
    /// Cumulative error (severity ≤ Error) and warning counts.
    errors_total: u64,
    warnings_total: u64,
    /// Per-unit cumulative counters (bounded to `top_units` + `other`).
    units: HashMap<String, UnitCounts>,
    /// Distinct units with an error/critical entry in the current window
    /// (reset each `emit`) — the units-in-failure gauge.
    failed_units_window: HashSet<String>,
    /// Cumulative per-unit snapshot at the last `tick_budgets` call, so the SLO
    /// layer (#105) can derive *per-window* deltas from the cumulative counters.
    prev_units: HashMap<String, UnitCounts>,
    /// Consecutive over-budget windows per unit (the multi-window burn guard).
    /// Absent / 0 means the unit is currently within budget.
    burn_streak: HashMap<String, u32>,
}

/// Resolved, validated per-unit error-budget / SLO thresholds (#105).
///
/// ## SLO math
/// For a unit over one derived window we have `messages` and `errors` (deltas of
/// the cumulative counters). The window error ratio is `errors / messages`. The
/// SLO target is `target_ratio` (the tolerated error fraction, e.g. 0.05 = 5%).
/// A window *burns budget* when the ratio exceeds `target_ratio * burn_rate`
/// (i.e. it's spending budget faster than `burn_rate`× the allowance) **and**
/// the window saw at least `min_messages` (so a unit that logged a single line
/// can't trip a 100% ratio). To avoid flapping on a one-off spike, an alert only
/// fires after `burn_windows` *consecutive* burning windows (a simple
/// multi-window burn-rate guard); the streak resets the first window the unit is
/// back within budget, which auto-resolves the alert.
#[derive(Debug, Clone, Copy)]
pub struct BudgetParams {
    /// Master switch for *alerting*. When false, `error_ratio` / `burn_rate`
    /// gauges are still emitted but no alert is ever raised.
    pub enabled: bool,
    /// Tolerated error fraction (the SLO), 0.0..=1.0.
    pub target_ratio: f64,
    /// Burn threshold multiplier: fire when ratio > `target_ratio * burn_rate`.
    pub burn_rate: f64,
    /// Consecutive over-budget windows required before firing.
    pub burn_windows: u32,
    /// Minimum messages in a window before the ratio is trusted.
    pub min_messages: u64,
}

impl Default for BudgetParams {
    fn default() -> Self {
        Self {
            enabled: false,
            target_ratio: 0.05,
            burn_rate: 2.0,
            burn_windows: 3,
            min_messages: 20,
        }
    }
}

/// Per-window SLO evaluation for one unit (pure result).
#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowEval {
    /// Window error ratio (`errors / messages`), clamped to 0.0..=1.0.
    ratio: f64,
    /// Whether this window burns budget (over threshold *and* enough volume).
    over_budget: bool,
}

/// Pure SLO math: window error ratio + whether this window burns budget.
///
/// `over_budget` requires both enough volume (`min_messages`) and the window
/// error ratio exceeding the burn threshold `target_ratio * burn_rate`.
fn evaluate_window(messages: u64, errors: u64, p: &BudgetParams) -> WindowEval {
    let ratio = if messages > 0 {
        (errors as f64 / messages as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let threshold = p.target_ratio * p.burn_rate;
    let over_budget = messages >= p.min_messages && ratio > threshold;
    WindowEval { ratio, over_budget }
}

/// Pure streak update. Returns `(new_streak, firing)`: a unit fires once it has
/// `burn_windows` consecutive over-budget windows, and the streak resets to 0
/// (no flap) the first window it's back within budget.
fn update_streak(prev_streak: u32, over_budget: bool, burn_windows: u32) -> (u32, bool) {
    if over_budget {
        let s = prev_streak.saturating_add(1);
        (s, s >= burn_windows.max(1))
    } else {
        (0, false)
    }
}

/// Telemetry + alert decisions produced by one [`LogAggregator::tick_budgets`]
/// call (#105). All fields are additive to the existing `logs/*` stream.
pub struct BudgetTick {
    /// `logs/by_unit/<unit>/error_ratio` and `.../burn_rate` gauges.
    pub points: Vec<TelemetryPoint>,
    /// Units in sustained burn this tick → `AlertReporter::observe`.
    pub firing: Vec<Alert>,
    /// `alert_key`s of all currently-burning units → `AlertReporter::reconcile`
    /// (anything previously firing but absent here is auto-resolved).
    pub firing_keys: Vec<String>,
}

/// Accumulates log-stream rollups; shared (`Arc`) between the publish loop
/// (which calls [`observe`](Self::observe)) and the emit tick.
pub struct LogAggregator {
    top_units: usize,
    budget: BudgetParams,
    inner: Mutex<Inner>,
}

impl LogAggregator {
    pub fn new(top_units: usize) -> Self {
        Self {
            top_units: top_units.max(1),
            budget: BudgetParams::default(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Attach per-unit error-budget / SLO thresholds (#105). Without this the
    /// budget layer stays disabled (gauges only when ticked, never alerts).
    pub fn with_budget(mut self, budget: BudgetParams) -> Self {
        self.budget = budget;
        self
    }

    /// Fold one message into the rollups. Cheap: a lock + a few counter bumps.
    pub fn observe(&self, msg: &SyslogMessage) {
        let sev = msg.severity as usize;
        let is_error = (msg.severity as u8) <= (Severity::Error as u8);
        let is_warning = msg.severity == Severity::Warning;
        let unit = unit_of(msg);

        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if sev < inner.by_severity.len() {
            inner.by_severity[sev] += 1;
        }
        if is_error {
            inner.errors_total += 1;
        }
        if is_warning {
            inner.warnings_total += 1;
        }

        // Per-unit bucket, bounded: a new unit folds into `other` once the cap
        // is reached, so the series set never grows unbounded.
        let cap = self.top_units;
        let key = match unit {
            Some(u) if inner.units.contains_key(u) => u.to_string(),
            Some(u) if inner.units.len() < cap => u.to_string(),
            Some(_) => OTHER_UNIT.to_string(),
            None => OTHER_UNIT.to_string(),
        };
        let entry = inner.units.entry(key.clone()).or_default();
        entry.messages += 1;
        if is_error {
            entry.errors += 1;
            if key != OTHER_UNIT {
                inner.failed_units_window.insert(key);
            }
        }
    }

    /// Snapshot the accumulators into telemetry points published under
    /// `zensight/syslog/<source>/logs/...`. Cumulative counters (per-severity,
    /// totals, per-unit) let the GUI/Prometheus derive rates; the
    /// units-in-failure gauge is windowed and reset here. `stats` adds journald
    /// throughput when the journald source is active.
    pub fn emit(&self, source: &str, stats: Option<JournaldStatsSnapshot>) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let counter = |metric: String, v: u64| {
            TelemetryPoint::new(source, Protocol::Syslog, metric, TelemetryValue::Counter(v))
        };
        let gauge = |metric: String, v: f64| {
            TelemetryPoint::new(source, Protocol::Syslog, metric, TelemetryValue::Gauge(v))
        };

        let Ok(mut inner) = self.inner.lock() else {
            return points;
        };

        for (code, count) in inner.by_severity.iter().enumerate() {
            if let Some(name) = severity_name(code as u8) {
                points.push(counter(format!("logs/by_severity/{name}_total"), *count));
            }
        }
        points.push(counter("logs/errors_total".into(), inner.errors_total));
        points.push(counter("logs/warnings_total".into(), inner.warnings_total));

        for (unit, c) in &inner.units {
            let slug = sanitize_unit(unit);
            points.push(counter(
                format!("logs/by_unit/{slug}/messages_total"),
                c.messages,
            ));
            if c.errors > 0 {
                points.push(counter(
                    format!("logs/by_unit/{slug}/errors_total"),
                    c.errors,
                ));
            }
        }

        // Units-in-failure is a windowed gauge: distinct units that logged an
        // error/critical since the last emit. Reset for the next window.
        points.push(gauge(
            "logs/units_in_failure".into(),
            inner.failed_units_window.len() as f64,
        ));
        inner.failed_units_window.clear();

        if let Some(s) = stats {
            points.push(counter("logs/journald/read_total".into(), s.read));
            points.push(counter("logs/journald/published_total".into(), s.published));
            points.push(counter("logs/journald/dropped_total".into(), s.dropped));
            points.push(counter(
                "logs/journald/sampled_out_total".into(),
                s.sampled_out,
            ));
        }

        points
    }

    /// Advance the per-unit error-budget window (#105) and return the SLO layer's
    /// telemetry + alert decisions. Call once per derived tick (alongside
    /// [`emit`](Self::emit)).
    ///
    /// Emits, for the *same bounded unit set* `emit` tracks (top-N + `other`):
    /// - `logs/by_unit/<unit>/error_ratio` — window `errors/messages` (0..1),
    /// - `logs/by_unit/<unit>/burn_rate` — `error_ratio / target_ratio` (×budget).
    ///
    /// When budget alerting is enabled, units in sustained burn (see
    /// [`BudgetParams`]) are returned as firing [`Alert`]s plus the full firing
    /// key set for reconcile; quiet/healthy units never fire.
    pub fn tick_budgets(&self, source: &str) -> BudgetTick {
        let gauge = |metric: String, v: f64| {
            TelemetryPoint::new(source, Protocol::Syslog, metric, TelemetryValue::Gauge(v))
        };
        let p = self.budget;
        let mut out = BudgetTick {
            points: Vec::new(),
            firing: Vec::new(),
            firing_keys: Vec::new(),
        };

        let Ok(mut inner) = self.inner.lock() else {
            return out;
        };

        // Snapshot current cumulative counters so we can drop the borrow on
        // `inner.units` before mutating the budget bookkeeping fields.
        let units_snapshot: Vec<(String, UnitCounts)> =
            inner.units.iter().map(|(k, v)| (k.clone(), *v)).collect();

        for (unit, cur) in &units_snapshot {
            let prev = inner.prev_units.get(unit).copied().unwrap_or_default();
            // Per-window deltas of the cumulative counters.
            let dm = cur.messages.saturating_sub(prev.messages);
            let de = cur.errors.saturating_sub(prev.errors);
            let eval = evaluate_window(dm, de, &p);

            let slug = sanitize_unit(unit);
            out.points.push(gauge(
                format!("logs/by_unit/{slug}/error_ratio"),
                eval.ratio,
            ));
            if p.target_ratio > 0.0 {
                out.points.push(gauge(
                    format!("logs/by_unit/{slug}/burn_rate"),
                    eval.ratio / p.target_ratio,
                ));
            }

            if p.enabled {
                let streak = inner.burn_streak.get(unit).copied().unwrap_or(0);
                let (new_streak, firing) = update_streak(streak, eval.over_budget, p.burn_windows);
                if new_streak == 0 {
                    inner.burn_streak.remove(unit);
                } else {
                    inner.burn_streak.insert(unit.clone(), new_streak);
                }
                if firing {
                    let alert = budget_alert(source, unit, eval.ratio, dm, de, &p);
                    out.firing_keys.push(alert.alert_key());
                    out.firing.push(alert);
                }
            }
        }

        // Keep the streak map bounded to the live unit set (units can fold into
        // `other` over time) and roll the window forward.
        let live: HashSet<&str> = units_snapshot.iter().map(|(k, _)| k.as_str()).collect();
        inner.burn_streak.retain(|u, _| live.contains(u.as_str()));
        inner.prev_units = inner.units.clone();

        out
    }
}

/// Build the firing alert for a unit in sustained budget burn (#105).
///
/// Pure (no reporter / no clock beyond the alert timestamp) so it is testable.
fn budget_alert(source: &str, unit: &str, ratio: f64, dm: u64, de: u64, p: &BudgetParams) -> Alert {
    let pct = ratio * 100.0;
    let target_pct = p.target_ratio * 100.0;
    // Deeply over budget → Critical; just past the threshold → Warning.
    let severity = if ratio >= (p.target_ratio * p.burn_rate * CRITICAL_BURN_FACTOR).min(1.0) {
        AlertSeverity::Critical
    } else {
        AlertSeverity::Warning
    };
    Alert::new(
        source.to_string(),
        Protocol::Syslog,
        AlertKind::Anomaly,
        BUDGET_RULE,
        severity,
        format!(
            "{unit}: error budget burn — {pct:.1}% errors this window \
             ({de}/{dm}), SLO target {target_pct:.1}%"
        ),
    )
    .with_label("unit", unit.to_string())
    .with_label("error_ratio", format!("{ratio:.4}"))
    .with_label("target_ratio", format!("{:.4}", p.target_ratio))
}

/// Extract the systemd unit from a journald-sourced message (network syslog has
/// none → `None`, which buckets into `other`).
fn unit_of(msg: &SyslogMessage) -> Option<&str> {
    msg.structured_data
        .get("journald")
        .and_then(|f| f.get("unit"))
        .map(String::as_str)
        .filter(|u| !u.is_empty())
}

/// Lowercase severity slug for the metric name (`emergency`..`debug`).
fn severity_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("emergency"),
        1 => Some("alert"),
        2 => Some("critical"),
        3 => Some("error"),
        4 => Some("warning"),
        5 => Some("notice"),
        6 => Some("informational"),
        7 => Some("debug"),
        _ => None,
    }
}

/// Make a unit name safe as a key-expression segment: no `/`, no whitespace.
/// (`nginx.service` stays readable; `user@1000.service/foo` loses the slash.)
fn sanitize_unit(unit: &str) -> String {
    unit.chars()
        .map(|c| {
            if c == '/' || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(sev: Severity, unit: Option<&str>) -> SyslogMessage {
        let mut m = crate::parser::parse("<14>hello").unwrap();
        m.severity = sev;
        if let Some(u) = unit {
            let mut f = HashMap::new();
            f.insert("unit".to_string(), u.to_string());
            m.structured_data.insert("journald".to_string(), f);
        }
        m
    }

    fn find<'a>(points: &'a [TelemetryPoint], metric: &str) -> Option<&'a TelemetryPoint> {
        points.iter().find(|p| p.metric == metric)
    }

    #[test]
    fn rolls_up_severity_and_error_totals() {
        let agg = LogAggregator::new(10);
        agg.observe(&msg(Severity::Error, Some("nginx.service")));
        agg.observe(&msg(Severity::Error, Some("nginx.service")));
        agg.observe(&msg(Severity::Warning, Some("nginx.service")));
        agg.observe(&msg(Severity::Informational, Some("cron.service")));

        let pts = agg.emit("host01", None);
        assert_eq!(
            find(&pts, "logs/errors_total").unwrap().value,
            TelemetryValue::Counter(2)
        );
        assert_eq!(
            find(&pts, "logs/warnings_total").unwrap().value,
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            find(&pts, "logs/by_severity/error_total").unwrap().value,
            TelemetryValue::Counter(2)
        );
        assert_eq!(
            find(&pts, "logs/by_unit/nginx.service/errors_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(2)
        );
        // Two distinct units logged this window; one (nginx) had errors.
        assert_eq!(
            find(&pts, "logs/units_in_failure").unwrap().value,
            TelemetryValue::Gauge(1.0)
        );
    }

    #[test]
    fn units_in_failure_is_windowed() {
        let agg = LogAggregator::new(10);
        agg.observe(&msg(Severity::Critical, Some("a.service")));
        let first = agg.emit("h", None);
        assert_eq!(
            find(&first, "logs/units_in_failure").unwrap().value,
            TelemetryValue::Gauge(1.0)
        );
        // Next window with no new errors → gauge resets to 0.
        let second = agg.emit("h", None);
        assert_eq!(
            find(&second, "logs/units_in_failure").unwrap().value,
            TelemetryValue::Gauge(0.0)
        );
    }

    #[test]
    fn per_unit_cardinality_capped_to_other() {
        let agg = LogAggregator::new(2);
        for i in 0..5 {
            agg.observe(&msg(Severity::Notice, Some(&format!("svc{i}.service"))));
        }
        let pts = agg.emit("h", None);
        let unit_series = pts
            .iter()
            .filter(|p| {
                p.metric.starts_with("logs/by_unit/") && p.metric.ends_with("/messages_total")
            })
            .count();
        // 2 tracked units + the `other` bucket = 3 series max.
        assert_eq!(unit_series, 3);
        assert!(find(&pts, "logs/by_unit/other/messages_total").is_some());
    }

    fn budget(target: f64, burn_rate: f64, windows: u32, min: u64) -> BudgetParams {
        BudgetParams {
            enabled: true,
            target_ratio: target,
            burn_rate,
            burn_windows: windows,
            min_messages: min,
        }
    }

    /// Observe `errors` error lines + `info` info lines for `unit`.
    fn observe_window(agg: &LogAggregator, unit: &str, errors: usize, info: usize) {
        for _ in 0..errors {
            agg.observe(&msg(Severity::Error, Some(unit)));
        }
        for _ in 0..info {
            agg.observe(&msg(Severity::Informational, Some(unit)));
        }
    }

    #[test]
    fn evaluate_window_ratio_volume_and_threshold() {
        let p = budget(0.1, 2.0, 3, 10); // burn threshold = 0.2
        let e = evaluate_window(100, 30, &p);
        assert!((e.ratio - 0.3).abs() < 1e-9);
        assert!(e.over_budget); // 0.3 > 0.2 and 100 >= 10
        // Below the burn threshold: within budget.
        assert!(!evaluate_window(100, 5, &p).over_budget); // 0.05 < 0.2
        // 100% error ratio but below the volume floor → not trusted.
        assert!(!evaluate_window(5, 5, &p).over_budget);
        // No traffic this window → ratio 0.
        assert_eq!(evaluate_window(0, 0, &p).ratio, 0.0);
    }

    #[test]
    fn update_streak_fires_after_n_and_resets_on_recovery() {
        assert_eq!(update_streak(0, true, 3), (1, false));
        assert_eq!(update_streak(1, true, 3), (2, false));
        assert_eq!(update_streak(2, true, 3), (3, true)); // Nth consecutive → fire
        assert_eq!(update_streak(3, true, 3), (4, true)); // stays firing
        assert_eq!(update_streak(4, false, 3), (0, false)); // recovered → reset
    }

    #[test]
    fn tick_emits_error_ratio_gauge_without_alerting_by_default() {
        let agg = LogAggregator::new(10); // budget disabled by default
        observe_window(&agg, "svc.service", 10, 10); // 50% error ratio
        let tick = agg.tick_budgets("h");
        assert_eq!(
            find(&tick.points, "logs/by_unit/svc.service/error_ratio")
                .unwrap()
                .value,
            TelemetryValue::Gauge(0.5)
        );
        // Disabled → never alerts even at 50% errors.
        assert!(tick.firing.is_empty());
        assert!(tick.firing_keys.is_empty());
    }

    #[test]
    fn budget_fires_on_sustained_burn_and_resolves_on_recovery() {
        // target 10%, burn 2x → threshold 20%; needs 2 consecutive windows.
        let agg = LogAggregator::new(10).with_budget(budget(0.1, 2.0, 2, 10));

        // Window 1: 50% errors over 100 msgs → over budget, streak 1 (< 2).
        observe_window(&agg, "bad.service", 50, 50);
        let t1 = agg.tick_budgets("h");
        assert!(t1.firing.is_empty());

        // Window 2: still burning → streak 2 → fires.
        observe_window(&agg, "bad.service", 50, 50);
        let t2 = agg.tick_budgets("h");
        assert_eq!(t2.firing.len(), 1);
        assert_eq!(t2.firing[0].rule, BUDGET_RULE);
        assert_eq!(
            t2.firing[0].labels.get("unit").map(String::as_str),
            Some("bad.service")
        );
        assert_eq!(t2.firing_keys.len(), 1);

        // Window 3: clean (no new errors) → recovers, nothing firing → reconcile
        // resolves the prior alert.
        observe_window(&agg, "bad.service", 0, 100);
        let t3 = agg.tick_budgets("h");
        assert!(t3.firing.is_empty());
        assert!(t3.firing_keys.is_empty());
    }

    #[test]
    fn quiet_unit_never_fires_even_at_100pct() {
        // Aggressive thresholds (1 window, target 1%) but a high volume floor.
        let agg = LogAggregator::new(10).with_budget(budget(0.01, 1.0, 1, 20));
        // Only 5 lines, all errors → 100% ratio but below the 20-msg floor.
        observe_window(&agg, "quiet.service", 5, 0);
        for _ in 0..3 {
            let tick = agg.tick_budgets("h");
            assert!(tick.firing.is_empty(), "quiet unit must not alert");
        }
    }

    #[test]
    fn budget_alert_severity_escalates_when_deeply_over() {
        let p = budget(0.05, 2.0, 1, 10); // threshold 10%, critical at >= 20%
        let warn = budget_alert("h", "u", 0.15, 100, 15, &p);
        assert_eq!(warn.severity, AlertSeverity::Warning);
        let crit = budget_alert("h", "u", 0.40, 100, 40, &p);
        assert_eq!(crit.severity, AlertSeverity::Critical);
    }

    #[test]
    fn journald_throughput_included_when_stats_present() {
        let agg = LogAggregator::new(10);
        let stats = JournaldStatsSnapshot {
            read: 100,
            published: 90,
            dropped: 10,
            ..Default::default()
        };
        let pts = agg.emit("h", Some(stats));
        assert_eq!(
            find(&pts, "logs/journald/dropped_total").unwrap().value,
            TelemetryValue::Counter(10)
        );
        // Absent when no journald stats.
        assert!(find(&agg.emit("h", None), "logs/journald/read_total").is_none());
    }
}
