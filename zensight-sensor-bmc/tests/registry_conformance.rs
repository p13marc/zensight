//! Forward (published ⊆ registered) is `telemetry_guard::checked_point`'s
//! debug-assert. This is the reverse — registered ⊆ emittable — plus the one
//! claim that matters more than either: **this sensor cannot act**.

use zensight_common::registry::bmc::Subject;
use zensight_common::registry_audit;

/// Families this build can register but never emit. **The empty list is the
/// claim**: every subject in `bmc.toml` is reachable from the code below.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

#[test]
fn every_registered_family_has_an_emitter() {
    // Literal strings deliberately: a copy-paste from the registry would
    // prove only that the file equals itself.
    let emitted: Vec<String> = [
        "1/psu/0/input_watts",
        "1/psu/0/output_watts",
        "1/psu/0/capacity_watts",
        "1/psu/0/present",
        "1/fan/2/rpm",
        "1/thermal/cpu1/celsius",
        "1/thermal/cpu1/upper_critical_c",
        "1/thermal/cpu1/upper_warning_c",
        "rack-a-1/reachable",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    registry_audit::assert_families_covered(
        "bmc",
        emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}

/// The slice declares no write procedure, and this test is the thing that
/// notices if one is ever added.
///
/// The sensor's whole posture is that it **cannot act**: a monitor that can
/// power-cycle a server is a different threat model from one that reads its
/// fan speed. Adding a `kind = "write"` procedure to `bmc.toml` would change
/// that silently — the code would still compile, the tests would still pass,
/// and the only visible difference would be a new key on the bus.
///
/// Crossing that line is a decision to make in its own issue, with the #283
/// gate pattern (default-off master switch, allowlist, a refusal that names
/// the switch that refused). #956 is that decision being taken for PDU
/// outlets; it is not this file's to make.
#[test]
fn the_slice_declares_no_write_surface() {
    // Parsed, not grepped. `pve`, `probe` and `container` test this with
    // `!toml.contains("kind = \"write\"")`, which also matches the sentence in
    // a comment explaining that there is no write surface — it failed on the
    // first run here for exactly that reason. Asking the slice is both
    // stronger and immune to its own documentation.
    let toml = zensight_common::registry::bmc::REGISTRY_TOML;
    let slice = zenkey::parse_slice(toml).expect("the shipped bmc slice parses");
    let writes: Vec<&str> = slice
        .procedures
        .iter()
        .filter(|p| {
            p.kind
                .as_ref()
                .and_then(|k| k.known())
                .is_some_and(|k| matches!(k, zenkey::slice::ProcedureKind::Write))
        })
        .map(|p| p.path.as_str())
        .collect();
    assert!(
        writes.is_empty(),
        "bmc declared write procedure(s) {writes:?}. That is a deliberate decision to make \
         explicitly (see the crate docs and #953), not one to land by editing a registry \
         file — a monitor that can reset a chassis is a different threat model."
    );
}

/// …and neither does the code. The registry is what `introspect` hands the
/// fleet, but a `Chassis.Reset` could be issued without ever appearing there.
#[test]
fn the_sensor_issues_no_redfish_action() {
    let src = concat!(
        include_str!("../src/redfish.rs"),
        include_str!("../src/poller.rs"),
        include_str!("../src/main.rs"),
    );
    for forbidden in [
        "Actions/",
        "Chassis.Reset",
        "ComputerSystem.Reset",
        ".post(",
        ".patch(",
    ] {
        assert!(
            !src.contains(forbidden),
            "the sensor source contains {forbidden:?} — this sensor issues GETs and nothing \
             else, and power control is a separate decision (#953, and #956 for the shape it \
             would take)"
        );
    }
}
