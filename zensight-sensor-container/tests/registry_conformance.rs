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

/// The slice declares no write procedure that reaches a container, and this test is the
/// thing that notices if one is ever added.
///
/// A read-only socket mount and cgroup files, and nothing that can change a container. Adding a `kind = "write"` procedure to `container.toml` would
/// change that silently — the code would still compile, the tests would still
/// pass, and the only visible difference would be a new key on the bus.
///
/// The allowlist is one entry long and is not an exception to that rule:
/// `thresholds/set` (#931) rewrites what this sensor **alerts on** and touches
/// nothing it observes. It is declared `write` because #957 classifies a
/// procedure that changes a host's behaviour as one whose outcome must reach
/// that host's audit trail — accountability for "who changed the rules", not
/// permission to act.
///
/// Parsed, not grepped: `!toml.contains("kind = \"write\"")` — which this was
/// — also matches the sentence in a comment explaining that there is no write
/// surface, which is how bmc's version failed on its own documentation.
#[test]
fn the_slice_declares_no_write_surface_beyond_its_own_rule_set() {
    const ALLOWED: &[&str] = &["thresholds/set"];
    let toml = zensight_common::registry::container::REGISTRY_TOML;
    let slice = zenkey::parse_slice(toml).expect("the shipped container slice parses");
    let mut writes: Vec<&str> = slice
        .procedures
        .iter()
        .filter(|p| {
            p.kind
                .as_ref()
                .and_then(|k| k.known())
                .is_some_and(|k| matches!(k, zenkey::slice::ProcedureKind::Write))
        })
        .map(|p| p.path.as_str())
        .filter(|path| !ALLOWED.contains(path))
        .collect();
    writes.sort_unstable();
    assert!(
        writes.is_empty(),
        "container declared write procedure(s) {writes:?}. Stopping a container is a different \
         threat model (see the crate docs and #819); that is a decision to make \
         explicitly, not one to land by editing a registry file."
    );
}
