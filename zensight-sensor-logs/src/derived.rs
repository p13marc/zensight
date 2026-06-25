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

use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::parser::{Severity, SyslogMessage};
use crate::receiver::JournaldStatsSnapshot;

/// Overflow bucket name for units beyond the cardinality cap.
const OTHER_UNIT: &str = "other";

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
}

/// Accumulates log-stream rollups; shared (`Arc`) between the publish loop
/// (which calls [`observe`](Self::observe)) and the emit tick.
pub struct LogAggregator {
    top_units: usize,
    inner: Mutex<Inner>,
}

impl LogAggregator {
    pub fn new(top_units: usize) -> Self {
        Self {
            top_units: top_units.max(1),
            inner: Mutex::new(Inner::default()),
        }
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
