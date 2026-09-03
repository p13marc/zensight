//! Registry-checked telemetry-point construction (RFC 08 §5, issue #468).

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// Build one telemetry point, enforcing the subject registry.
///
/// Every metric name this sensor emits funnels through here: in debug builds
/// — which is every unit test — an unregistered name panics. The dynamic
/// chunks are a chassis id and a component id, both of them values a BMC
/// chose, so both are slugged at the call site (#843's boundary) before they
/// reach a key.
pub(crate) fn checked_point(
    source: &str,
    metric: impl Into<String>,
    value: TelemetryValue,
) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_common::registry::is_registered_telemetry("bmc", &metric),
        "unregistered bmc telemetry subject {metric:?} — add it to \
         zensight-common/registry/bmc.toml (RFC 08 §5, issue #468)"
    );
    // `source` is the REPORTING HOST, never the chassis (#883). A managed
    // chassis is a facet of the vantage point that polls it; the chassis rides
    // in the key and in the labels.
    TelemetryPoint::new(source, Protocol::Bmc, metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "unregistered bmc telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = checked_point("h", "totally/made/up", TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn the_registered_families_construct() {
        for m in [
            "1/psu/0/input_watts",
            "1/psu/0/present",
            "1/fan/3/rpm",
            "1/thermal/cpu1/celsius",
            "1/thermal/cpu1/upper_critical_c",
            "1/reachable",
        ] {
            let p = checked_point("mgmt01", m, TelemetryValue::Gauge(0.0));
            assert_eq!(p.metric, m);
        }
    }
}
