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
