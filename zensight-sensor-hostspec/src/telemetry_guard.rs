//! Registry-checked telemetry-point construction (RFC 08 §5, issue #468).

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// Build one telemetry point, enforcing the subject registry.
///
/// Every metric name this sensor emits funnels through here (RFC 08 §5,
/// issue #468): in debug builds — which is every unit test — an unregistered
/// metric name panics. Adding a metric without registering it in
/// `zensight-common/registry/hostspec.toml` fails the existing tests. Unit
/// names never enter these subjects, so no slugging is needed (#843's
/// lesson lives one crate over).
pub(crate) fn checked_point(
    source: &str,
    metric: impl Into<String>,
    value: TelemetryValue,
) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_common::registry::is_registered_telemetry("hostspec", &metric),
        "unregistered hostspec telemetry subject {metric:?} — add it to \
         zensight-common/registry/hostspec.toml (RFC 08 §5, issue #468)"
    );
    TelemetryPoint::new(source, Protocol::Hostspec, metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry guard must actually bite (the conformance suite that
    /// cannot fail is vacuously true — systemd's guard says the same).
    #[test]
    #[should_panic(expected = "unregistered hostspec telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = checked_point("h", "totally/made/up", TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn the_failing_gauge_constructs() {
        let p = checked_point("h", "assertions/failing", TelemetryValue::Gauge(0.0));
        assert_eq!(p.metric, "assertions/failing");
    }
}
