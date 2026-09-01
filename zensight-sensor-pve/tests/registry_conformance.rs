//! Reverse registry conformance for pve (#654 pattern, RFC 08 §6.1).
//!
//! Forward (*published ⊆ registered*) is `telemetry_guard::checked_point`'s
//! debug-assert. This is the reverse: *registered ⊆ emittable* — a family the
//! registry advertises that no build can publish is a surface `introspect`
//! promises the fleet and nobody serves.
//!
//! It matters more here than in most sensors, because this slice's families
//! are the *audit findings*: a `pool/overcommit_ratio` that were registered
//! and never emitted would leave the one number nobody had, still missing,
//! while `introspect` claimed otherwise.

use zensight_common::registry::pve::Subject;
use zensight_common::registry_audit;

/// Families whose emission depends on something a build cannot guarantee.
///
/// `cluster/quorate` is the only one and it is **not** listed: a standalone
/// node genuinely has no quorum, but that is a property of the deployment,
/// not of the build, and the emitter exists unconditionally. The `pve` slice
/// carries no conditional families, and the empty list is the claim.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

/// One representative metric per family the sensor can emit. Kept as literal
/// strings deliberately: the point is to state, in one readable place, every
/// series a `pve` deployment produces — and a copy-paste from the registry
/// would prove only that the file equals itself.
#[test]
fn every_registered_family_has_an_emitter() {
    let emitted: Vec<String> = [
        "guest/140/cpu_ratio",
        "guest/140/mem_bytes",
        "guest/140/mem_max_bytes",
        "guest/140/disk_bytes",
        "guest/140/disk_max_bytes",
        "guest/140/provisioned_bytes",
        "guest/140/uptime_secs",
        "guest/140/running",
        "storage/local-lvm/total_bytes",
        "storage/local-lvm/used_bytes",
        "storage/local-lvm/avail_bytes",
        "storage/local-lvm/used_ratio",
        "storage/local-lvm/allocated_bytes",
        "storage/local-lvm/overcommit_ratio",
        "backup/140/size_bytes",
        "backup/140/size_change_pct",
        "backup/140/age_secs",
        "backup/140/duration_secs",
        "backup/140/ok",
        "backup/job/pve/ok",
        "backup/job/pve/duration_secs",
        "cluster/quorate",
        "cluster/nodes_online",
        "cluster/nodes_total",
        "cluster/guests_total",
        "cluster/guests_running",
        "cluster/replication_failed",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    registry_audit::assert_families_covered(
        "pve",
        emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}

/// The slice declares no write procedure, and this test is the thing that
/// notices if one is ever added by accident.
///
/// The sensor's whole security posture is that it *cannot* act: a monitor
/// that can stop a VM is a different threat model. Adding a `kind = "write"`
/// procedure to `pve.toml` would change that silently — the code would still
/// compile, the tests would still pass, and the only visible difference would
/// be a new key on the bus.
#[test]
fn the_slice_declares_no_write_surface() {
    let toml = zensight_common::registry::pve::REGISTRY_TOML;
    assert!(
        !toml.contains(r#"kind = "write""#),
        "pve declared a write procedure. That is a deliberate decision to make \
         explicitly (see the crate docs and #818), not one to land by editing a \
         registry file — a monitor that can stop a VM is a different threat model."
    );
}
