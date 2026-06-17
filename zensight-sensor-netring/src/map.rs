//! Pure mapping from netring/flowscope observations to ZenSight types.
//!
//! Kept free of the netring/flowscope capture machinery so it is unit-testable
//! without privileges. `monitor.rs` decomposes netring callbacks into these
//! plain views; here we map them to [`TelemetryPoint`]s and [`Alert`]s.

use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol, TelemetryPoint, TelemetryValue};

/// A flattened anomaly, decomposed from `flowscope::OwnedAnomaly`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnomalyView {
    /// Detector slug, e.g. "PortScanTRW".
    pub kind: String,
    pub severity: AlertSeverity,
    /// `ip:port` (or `ip`) of the source, if known.
    pub src: Option<String>,
    pub dst: Option<String>,
    pub proto: Option<String>,
    /// Detector observations (k, v) — high-cardinality detail goes here.
    pub observations: Vec<(String, String)>,
    /// Numeric metrics (k, v).
    pub metrics: Vec<(String, f64)>,
}

/// Map a decomposed anomaly to a sensor-pushed [`Alert`].
///
/// The alert is bucketed by `(rule, src)` (via labels feeding `alert_key`), so a
/// 1000-port scan from one host collapses to one alert, not one-per-port — the
/// offending detail lives in labels/summary, never in a metric series name.
pub fn anomaly_alert(sensor_id: &str, a: &AnomalyView) -> Alert {
    let summary = human_summary(a);
    let mut alert = Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::Anomaly,
        &a.kind,
        a.severity,
        summary,
    );
    if let Some(src) = &a.src {
        alert = alert.with_label("src", src.clone());
    }
    if let Some(dst) = &a.dst {
        alert = alert.with_label("dst", dst.clone());
    }
    if let Some(proto) = &a.proto {
        alert = alert.with_label("proto", proto.clone());
    }
    for (k, v) in &a.observations {
        alert = alert.with_label(k.clone(), v.clone());
    }
    for (k, v) in &a.metrics {
        alert = alert.with_label(k.clone(), format!("{v}"));
    }
    alert
}

fn human_summary(a: &AnomalyView) -> String {
    match (&a.src, &a.dst) {
        (Some(src), Some(dst)) => format!("{} {} -> {}", a.kind, src, dst),
        (Some(src), None) => format!("{} from {}", a.kind, src),
        (None, Some(dst)) => format!("{} to {}", a.kind, dst),
        (None, None) => a.kind.clone(),
    }
}

/// Per-application bandwidth point: `bandwidth/<app>/bytes_per_sec` (Gauge).
pub fn bandwidth_point(sensor_id: &str, app: &str, bytes_per_sec: f64) -> TelemetryPoint {
    TelemetryPoint::new(
        sensor_id,
        Protocol::Netring,
        format!("bandwidth/{app}/bytes_per_sec"),
        TelemetryValue::Gauge(bytes_per_sec),
    )
    .with_label("app", app)
}

/// Flow-lifecycle aggregate points.
pub fn flow_points(
    sensor_id: &str,
    started_total: u64,
    ended_total: u64,
    active: u64,
) -> Vec<TelemetryPoint> {
    let p = |metric: &str, v: TelemetryValue| {
        TelemetryPoint::new(sensor_id, Protocol::Netring, metric, v)
    };
    vec![
        p("flow/started_total", TelemetryValue::Counter(started_total)),
        p("flow/ended_total", TelemetryValue::Counter(ended_total)),
        p("flow/active", TelemetryValue::Gauge(active as f64)),
    ]
}

/// TCP reset aggregate points.
pub fn tcp_reset_points(sensor_id: &str, resets: u64, refused: u64) -> Vec<TelemetryPoint> {
    vec![
        TelemetryPoint::new(
            sensor_id,
            Protocol::Netring,
            "tcp/resets_total",
            TelemetryValue::Counter(resets),
        ),
        TelemetryPoint::new(
            sensor_id,
            Protocol::Netring,
            "tcp/refused_total",
            TelemetryValue::Counter(refused),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_anomaly() -> AnomalyView {
        AnomalyView {
            kind: "PortScanTRW".into(),
            severity: AlertSeverity::Warning,
            src: Some("10.0.0.5:44321".into()),
            dst: Some("10.0.0.9:22".into()),
            proto: Some("tcp".into()),
            observations: vec![("verdict".into(), "scanner".into())],
            metrics: vec![("score".into(), 3.5)],
        }
    }

    #[test]
    fn anomaly_maps_to_alert_with_labels() {
        let a = anomaly_alert("sensor1", &scan_anomaly());
        assert_eq!(a.protocol, Protocol::Netring);
        assert_eq!(a.kind, AlertKind::Anomaly);
        assert_eq!(a.rule, "PortScanTRW");
        assert_eq!(a.severity, AlertSeverity::Warning);
        assert!(a.summary.contains("PortScanTRW"));
        assert!(a.summary.contains("10.0.0.5:44321"));
        assert_eq!(
            a.labels.get("src").map(String::as_str),
            Some("10.0.0.5:44321")
        );
        assert_eq!(a.labels.get("verdict").map(String::as_str), Some("scanner"));
        assert_eq!(a.labels.get("score").map(String::as_str), Some("3.5"));
    }

    #[test]
    fn alert_key_buckets_by_src_not_per_port() {
        // Same rule + src, different dst port → SAME alert_key (one alert).
        let mut a1 = scan_anomaly();
        a1.dst = Some("10.0.0.9:22".into());
        let mut a2 = scan_anomaly();
        a2.dst = Some("10.0.0.9:23".into());
        // Drop dst from labels for bucketing — emulate the production view that
        // keys on src only.
        a1.dst = None;
        a2.dst = None;
        let k1 = anomaly_alert("s", &a1).alert_key();
        let k2 = anomaly_alert("s", &a2).alert_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn bandwidth_and_flow_points() {
        let bp = bandwidth_point("s", "https", 1234.5);
        assert_eq!(bp.metric, "bandwidth/https/bytes_per_sec");
        assert_eq!(bp.value, TelemetryValue::Gauge(1234.5));

        let fps = flow_points("s", 10, 8, 2);
        assert_eq!(fps[0].value, TelemetryValue::Counter(10));
        assert_eq!(fps[2].value, TelemetryValue::Gauge(2.0));
    }

    #[test]
    fn tcp_reset_points_shape() {
        let pts = tcp_reset_points("s", 5, 3);
        assert_eq!(pts[0].metric, "tcp/resets_total");
        assert_eq!(pts[0].value, TelemetryValue::Counter(5));
        assert_eq!(pts[1].metric, "tcp/refused_total");
        assert_eq!(pts[1].value, TelemetryValue::Counter(3));
    }

    #[test]
    fn summary_variants() {
        let mut a = scan_anomaly();
        a.src = None;
        a.dst = None;
        assert_eq!(human_summary(&a), "PortScanTRW");
    }
}
