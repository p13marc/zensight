//! Registry-checked telemetry-point construction (RFC 08 §5, issue #468).

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};
/// Build one telemetry point, enforcing the subject registry.
///
/// Every metric name this sensor emits funnels through here (RFC 08 §5, issue
/// #468): in debug builds — which is every unit test — an unregistered metric
/// name panics. Adding a metric without registering it in
/// `zensight-keyspace/registry/systemd.toml` fails the existing tests.
pub(crate) fn checked_point(
    source: &str,
    metric: impl Into<String>,
    value: TelemetryValue,
) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_keyspace::registry::is_registered_telemetry("systemd", &metric),
        "unregistered systemd telemetry subject {metric:?} — add it to \
         zensight-keyspace/registry/systemd.toml (RFC 08 §5, issue #468)"
    );
    TelemetryPoint::new(source, Protocol::Systemd, metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry guard must actually bite. A conformance suite that cannot fail
    /// is the same mistake as the `{metric...}` catch-all it replaced: vacuously
    /// true. This is the test that proves the mapper tests mean something.
    #[test]
    #[should_panic(expected = "unregistered systemd telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = checked_point("h", "totally/made/up/subject", TelemetryValue::Gauge(1.0));
    }

    /// ...and a real subject constructs.
    #[test]
    fn a_registered_metric_constructs() {
        let p = checked_point("h", "unit/nginx.service/active", TelemetryValue::Gauge(1.0));
        assert_eq!(p.metric, "unit/nginx.service/active");
    }
}
