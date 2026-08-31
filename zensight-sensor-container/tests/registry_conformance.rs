//! Reverse registry conformance for container (#654 pattern, RFC 08 §6.1).
//!
//! Forward (*published ⊆ registered*) is `telemetry_guard::checked_point`'s
//! debug-assert. This is the reverse: *registered ⊆ emittable*.

use zensight_common::registry::container::Subject;
use zensight_common::registry_audit;

/// `{name}/image_behind_upstream` is emitted only when the explicitly-
/// egressing upstream collector is on — the one part of this sensor that
/// leaves the host, and off by default. It is a *gauge with no reading*
/// otherwise, which is precisely the case the RFC 08 §6.1 ledger exists for:
/// a procedure can answer "unsupported", a gauge cannot, and a sentinel value
/// would corrupt every consumer downstream.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[(
    "{name}/image_behind_upstream",
    "needs container.upstream.enabled — the only egressing collector, off by default (#819)",
)];

#[test]
fn every_registered_family_has_an_emitter() {
    let emitted: Vec<String> = [
        "caddy/memory_bytes",
        "caddy/memory_max_bytes",
        "caddy/memory_ratio",
        "caddy/memory_peak_bytes",
        "caddy/cpu_usage_usec_total",
        "caddy/cpu_throttled_usec_total",
        "caddy/oom_kills_total",
        "caddy/memory_max_events_total",
        "caddy/cpu_pressure_avg10",
        "caddy/memory_pressure_avg10",
        "caddy/io_pressure_avg10",
        "caddy/pids",
        "caddy/running",
        "caddy/restart_count",
        "caddy/exit_code",
        "caddy/uptime_secs",
        "caddy/healthy",
        "containers/total",
        "containers/running",
        "containers/unhealthy",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    registry_audit::assert_families_covered(
        "container",
        emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}

/// The sensor's security posture, asserted rather than described: a read-only
/// socket mount and cgroup files, and nothing that can change a container.
/// Adding a write procedure to the slice would compile, pass every other test,
/// and change only one thing — a new key on the bus.
#[test]
fn the_slice_declares_no_write_surface() {
    let toml = zensight_common::registry::container::REGISTRY_TOML;
    assert!(
        !toml.contains(r#"kind = "write""#),
        "container declared a write procedure. Stopping a container is a different \
         threat model (see the crate docs and #819); that is a decision to make \
         explicitly, not one to land by editing a registry file."
    );
}
