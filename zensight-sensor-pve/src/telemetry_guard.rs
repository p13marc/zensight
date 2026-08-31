//! Registry-checked telemetry-point construction (RFC 08 §5, issue #468).

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// Build one telemetry point, enforcing the subject registry.
///
/// Every metric name this sensor emits funnels through here: in debug builds
/// — which is every unit test — an unregistered metric name panics. The
/// dynamic chunks here are a vmid and a storage name; the vmid is a number
/// and safe, but a storage name is operator-chosen, so it is slugged at the
/// call site (`#843`'s lesson, two crates over) before it reaches a key.
pub(crate) fn checked_point(
    source: &str,
    metric: impl Into<String>,
    value: TelemetryValue,
) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_common::registry::is_registered_telemetry("pve", &metric),
        "unregistered pve telemetry subject {metric:?} — add it to \
         zensight-common/registry/pve.toml (RFC 08 §5, issue #468)"
    );
    TelemetryPoint::new(source, Protocol::Pve, metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "unregistered pve telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = checked_point("h", "totally/made/up", TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn the_registered_families_construct() {
        for m in [
            "guest/140/cpu_ratio",
            "guest/140/running",
            "storage/local-lvm/overcommit_ratio",
            "backup/140/size_change_pct",
            "cluster/quorate",
        ] {
            let p = checked_point("pve", m, TelemetryValue::Gauge(0.0));
            assert_eq!(p.metric, m);
        }
    }
}
