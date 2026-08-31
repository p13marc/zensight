//! Registry-checked telemetry-point construction (RFC 08 §5, issue #468).

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// Build one telemetry point, enforcing the subject registry.
///
/// The dynamic chunk is the operator's target NAME, slugged at the call site
/// through `zenkey::Chunk::slug` before it can reach a key (#843's lesson).
pub(crate) fn checked_point(
    source: &str,
    metric: impl Into<String>,
    value: TelemetryValue,
) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_common::registry::is_registered_telemetry("probe", &metric),
        "unregistered probe telemetry subject {metric:?} — add it to \
         zensight-common/registry/probe.toml (RFC 08 §5, issue #468)"
    );
    TelemetryPoint::new(source, Protocol::Probe, metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "unregistered probe telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = checked_point("h", "totally/made/up", TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn the_registered_families_construct() {
        for m in [
            "forge/up",
            "forge/duration_ms",
            "forge/timeout",
            "forge/tls_days_to_expiry",
            "targets/failing",
        ] {
            assert_eq!(checked_point("h", m, TelemetryValue::Gauge(0.0)).metric, m);
        }
    }
}
