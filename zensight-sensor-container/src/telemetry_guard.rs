//! Registry-checked telemetry-point construction (RFC 08 §5, issue #468).

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// Build one telemetry point, enforcing the subject registry.
///
/// The dynamic chunk here is a container NAME — operator-chosen and therefore
/// a foreign value, slugged at the call site through `zenkey::Chunk::slug`
/// before it can reach a key (#843's lesson, three crates over).
pub(crate) fn checked_point(
    source: &str,
    metric: impl Into<String>,
    value: TelemetryValue,
) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_common::registry::is_registered_telemetry("container", &metric),
        "unregistered container telemetry subject {metric:?} — add it to \
         zensight-common/registry/container.toml (RFC 08 §5, issue #468)"
    );
    TelemetryPoint::new(source, Protocol::Container, metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "unregistered container telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = checked_point("h", "totally/made/up", TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn the_registered_families_construct() {
        for m in [
            "caddy/memory_bytes",
            "caddy/oom_kills_total",
            "caddy/healthy",
            "containers/total",
        ] {
            assert_eq!(checked_point("h", m, TelemetryValue::Gauge(0.0)).metric, m);
        }
    }
}
