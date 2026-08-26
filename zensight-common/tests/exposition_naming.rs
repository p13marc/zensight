//! Conformance for the registry-driven metric naming (#764, #770).
//!
//! The family rule is *"drop `{var}` chunks from the registered pattern, join
//! the literals"*. That rule is only safe while it stays **injective**: if two
//! patterns collapse to one name, two unrelated metrics silently merge into one
//! family — and it surfaces as a wrong dashboard, not a failed build.
//!
//! Run over every `class = "telemetry"` subject in `zensight-common/registry/`,
//! the rule today produces exactly one collision and four rest-var families.
//! These tests pin that, so a future registry edit that breaks it fails here.

use std::collections::BTreeMap;

use zensight_common::exposition::family_chunks;
use zensight_common::registry::REGISTRIES;
use zensight_common::registry_audit::registered_telemetry_patterns;

/// Collisions that are correct, each with the reason it is correct.
///
/// A collision is legitimate when the two patterns are an **aggregate and its
/// per-entity refinement**: they belong in one family, told apart by a label.
/// Anything else merging two metrics is a bug.
const INTENDED_COLLISIONS: &[(&str, &str, &str)] = &[
    (
        "sysinfo",
        "cpu_usage",
        "`cpu/usage` (whole-machine) and `cpu/{core}/usage` (per-core) are one \
         family distinguished by the `cpu` label — exactly semconv's \
         system.cpu.utilization{cpu} shape.",
    ),
    (
        "sysinfo",
        "cpu_times",
        "`cpu/times/{component}` (aggregate) and `{cpu}/times/{component}` \
         (per-cpu) are one family distinguished by the `cpu` label. The second \
         reaches this name through NAME_ALIASES; without the alias it would be \
         orphaned as a bare `times`.",
    ),
    (
        "sysinfo",
        "cpu_schedstat_run_delay_ns_total",
        "Same aggregate/per-cpu pair as cpu_times, via the same alias.",
    ),
];

/// Producers whose telemetry tail is device-defined, registered as a rest-var
/// catch-all. Their family name comes from the rest variable's *value*, so it
/// cannot be computed from the pattern alone.
const REST_VAR_PRODUCERS: &[&str] = &["snmp", "modbus", "gnmi", "netflow"];

/// Every `(producer, pattern)` in the registry's telemetry class.
fn all_telemetry_patterns() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (producer, _) in REGISTRIES {
        for pattern in registered_telemetry_patterns(producer) {
            out.push((producer.to_string(), pattern));
        }
    }
    out
}

/// Is this pattern a rest-var catch-all (`.../{x...}`)?
fn is_catchall(pattern: &str) -> bool {
    pattern.contains("...")
}

/// No two telemetry patterns may collapse to the same family name, except the
/// aggregate/refinement pairs listed above.
///
/// This is the guard that keeps the naming rule honest as the registry grows.
#[test]
fn family_names_are_injective() {
    let mut by_name: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();

    for (producer, pattern) in all_telemetry_patterns() {
        if is_catchall(&pattern) {
            continue; // named from the value, not the pattern
        }
        let name = family_chunks(&producer, &pattern, &[]).join("_");
        by_name
            .entry((producer.clone(), name))
            .or_default()
            .push(pattern);
    }

    let mut unexpected = Vec::new();
    for ((producer, name), patterns) in &by_name {
        if patterns.len() < 2 {
            continue;
        }
        let allowed = INTENDED_COLLISIONS
            .iter()
            .any(|(p, n, _)| p == producer && n == name);
        if !allowed {
            unexpected.push(format!("  {producer}: {name:?} <- {patterns:?}"));
        }
    }

    assert!(
        unexpected.is_empty(),
        "these telemetry patterns collapse to one family name, silently merging \
         unrelated metrics:\n{}\n\nIf a pair is genuinely an aggregate and its \
         per-entity refinement, add it to INTENDED_COLLISIONS with the reason. \
         Otherwise the registry pattern or the alias table needs fixing.",
        unexpected.join("\n")
    );
}

/// Every intended collision must actually still collide.
///
/// Without this, a registry rename could quietly retire an allow-list entry and
/// leave a stale exemption that would hide a real collision later.
#[test]
fn intended_collisions_are_still_collisions() {
    for (producer, name, reason) in INTENDED_COLLISIONS {
        let hits: Vec<String> = registered_telemetry_patterns(producer)
            .into_iter()
            .filter(|p| !is_catchall(p))
            .filter(|p| family_chunks(producer, p, &[]).join("_") == *name)
            .collect();
        assert!(
            hits.len() >= 2,
            "INTENDED_COLLISIONS lists ({producer}, {name:?}) but only {hits:?} \
             maps there now — the exemption is stale and should be removed.\n\
             Reason on file: {reason}"
        );
    }
}

/// Every telemetry pattern must produce a non-empty family name.
///
/// The four rest-var catch-alls are the exception and are named from their
/// value at runtime; anything *else* falling through to an empty name means a
/// new leading-variable pattern that needs an alias.
#[test]
fn every_telemetry_pattern_has_a_name() {
    let mut empty: Vec<String> = Vec::new();

    for (producer, pattern) in all_telemetry_patterns() {
        let chunks = family_chunks(&producer, &pattern, &[]);
        if chunks.is_empty() || chunks.iter().all(|c| c.is_empty()) {
            if is_catchall(&pattern) && REST_VAR_PRODUCERS.contains(&producer.as_str()) {
                continue;
            }
            empty.push(format!("  {producer}: {pattern:?}"));
        }
    }

    assert!(
        empty.is_empty(),
        "these telemetry patterns yield an empty family name:\n{}\n\nA pattern \
         whose leading chunk is a variable loses its discriminator — add it to \
         NAME_ALIASES so it rejoins its aggregate sibling's family.",
        empty.join("\n")
    );
}

/// The four known rest-var catch-alls, pinned.
///
/// If a fifth appears, the exporters' naming needs to know about it — the
/// family rule cannot name it from the pattern.
#[test]
fn the_rest_var_producers_are_the_four_we_know() {
    let mut found: Vec<String> = all_telemetry_patterns()
        .into_iter()
        .filter(|(_, p)| is_catchall(p))
        .map(|(prod, _)| prod)
        .collect();
    found.sort();
    found.dedup();

    let mut expected: Vec<String> = REST_VAR_PRODUCERS.iter().map(|s| s.to_string()).collect();
    expected.sort();

    assert_eq!(
        found, expected,
        "the set of producers registered with a rest-var telemetry catch-all \
         changed. Their family name comes from the rest variable's VALUE, so \
         `exposition::REST_VAR_PRODUCERS` must be updated to match."
    );
}

/// A rest-var producer's name comes from the value, and its leading variable
/// becomes a label rather than a name chunk.
#[test]
fn a_rest_var_producer_is_named_from_its_value() {
    let vars = vec![
        ("device", "sw1".to_string()),
        ("metric", "if/1/in_octets".to_string()),
    ];
    let chunks = family_chunks("snmp", "{device}/{metric...}", &vars);
    assert_eq!(
        chunks,
        vec!["if", "1", "in_octets"],
        "the rest variable's value is the name; `device` is a label"
    );
}

/// The plain rule, spelled out on a representative pattern.
#[test]
fn the_family_rule_drops_variable_chunks() {
    assert_eq!(
        family_chunks("sysinfo", "disk/{device}/io/read_bytes", &[]),
        vec!["disk", "io", "read_bytes"]
    );
    assert_eq!(
        family_chunks("netlink", "iface/{iface}/rx_bytes", &[]),
        vec!["iface", "rx_bytes"]
    );
}

/// A leading-variable pattern rejoins its aggregate sibling through the alias
/// table instead of being orphaned as a bare `times`.
#[test]
fn a_leading_variable_pattern_rejoins_its_family() {
    assert_eq!(
        family_chunks("sysinfo", "{cpu}/times/{component}", &[]),
        vec!["cpu", "times"],
    );
    assert_eq!(
        family_chunks("sysinfo", "cpu/times/{component}", &[]),
        vec!["cpu", "times"],
    );
}
