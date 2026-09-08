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
    // The registration guard above validates the *path*. This one validates
    // the *semantics* — that the value's variant is the kind the registry
    // declares (RFC 08 §2 v1.32, #1071). Without it five cumulative counters
    // here went out as `Gauge` for a release: the exporters read the variant
    // and nothing else, so `container_…_oom_kills_total` was scraped as
    // `# TYPE … gauge`, which no backend can `rate()`.
    if let Err(e) = zensight_common::registry::kind_matches("container", &metric, &value) {
        debug_assert!(false, "{e}");
    }
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
        for m in ["caddy/memory_bytes", "caddy/healthy", "containers/total"] {
            assert_eq!(checked_point("h", m, TelemetryValue::Gauge(0.0)).metric, m);
        }
        for m in ["caddy/oom_kills_total", "caddy/restart_count"] {
            assert_eq!(checked_point("h", m, TelemetryValue::Counter(0)).metric, m);
        }
    }

    /// The guard the registry could not run before #1071: a cumulative counter
    /// published as a `Gauge`. Both exporters derive the wire type from the
    /// variant and nothing else, so this is what a scrape sees — five of these
    /// shipped as `# TYPE … gauge`, which no backend can `rate()`.
    #[test]
    #[should_panic(expected = "declared kind = \"counter\" but published as gauge")]
    fn a_counter_published_as_a_gauge_panics_in_debug() {
        let _ = checked_point("h", "caddy/oom_kills_total", TelemetryValue::Gauge(3.0));
    }

    /// And the other way, so the guard is not one-sided.
    #[test]
    #[should_panic(expected = "declared kind = \"gauge\" but published as counter")]
    fn a_gauge_published_as_a_counter_panics_in_debug() {
        let _ = checked_point("h", "caddy/memory_bytes", TelemetryValue::Counter(3));
    }
}
