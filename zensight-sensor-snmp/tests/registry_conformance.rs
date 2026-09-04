//! What the `snmp` registry slice may and may not declare.
//!
//! `snmp` is a **catch-all producer**: `{device}/{metric...}` means its metric
//! tree is whatever the polled device exposes, so the family-coverage check
//! every other sensor runs here is vacuous for it and
//! `registry_audit::assert_families_covered` refuses to run at all (see its
//! panic message). The forward direction — published ⊆ registered — is
//! `metric_guard`'s job at run time.
//!
//! What is left, and what this file is for, is the **write surface**.

/// The write procedures this slice declares, and no others.
///
/// A plain `!toml.contains("kind = \"write\"")` — the shape `pve`, `probe` and
/// `container` carry — cannot work here, because the artifact channel is a
/// legitimate write surface every artifact-capable sensor has. So the claim is
/// the stronger one an allowlist makes: *these two, and nothing else.*
///
/// The list is short on purpose. `snmp` speaks to devices an operator does not
/// otherwise reach, and an SNMP SET is a different threat model from a poll —
/// which is exactly why the gated PDU outlet cycle (#956) is a separate issue
/// with a separate decision, and why landing it means **editing this list**
/// rather than watching a test keep passing.
const ALLOWED_WRITE_PROCEDURES: &[&str] = &[
    "artifact/request",
    "artifact/cancel",
    // #956, and adding it here was the deliberate act the doc above describes.
    // Default OFF, an empty allowlist that rejects everything, a SEPARATE
    // write credential refused at startup if missing, and a PDU profile whose
    // control OIDs were verified against the vendor MIB. Both outcomes reach
    // the host's audit trail (#957) — which for this procedure is the only
    // record that a request to cut power was ever made.
    "action/set",
    // #931. Unlike `action/set` this reaches no device: it rewrites the rule
    // set this sensor evaluates against its OWN telemetry. `kind = "write"`
    // buys the #957 audit record for "who changed the rules", which is the
    // right accountability for a procedure that changes what a host alerts on.
    "thresholds/set",
    // #936. Reaches no device *as an action*, but it does change which devices
    // this sensor talks to at all — so it is the entry on this list that most
    // deserves the scrutiny the doc above asks for. Three things bound it:
    //
    //   1. The request type cannot carry a credential. `SnmpTargets` names a
    //      credential SET; the community string and the v3 passphrases stay in
    //      this host's own `snmp.credentials`, and a name the host does not
    //      have is refused rather than falling back to a default. So the
    //      procedure can say "poll that device with the credentials you
    //      already hold" and can never say "poll it with these".
    //   2. `fanout = "forbidden"`. A threshold is the same rule wherever it
    //      lands; a device set is not — pushing one fleet-wide would have
    //      every host poll every device. Fleet-wide target changes go through
    //      `@desired`, which is per-host by construction.
    //   3. It grants nothing an operator did not already have: every device it
    //      can add is one they could have written into the config file and
    //      restarted for. What it removes is the restart.
    //
    // The #957 audit record answers "who changed what this sensor polls",
    // which is the accountability that matters here.
    "targets/set",
];

#[test]
fn the_slice_declares_no_write_surface_beyond_the_artifact_channel() {
    let toml = zensight_common::registry::snmp::REGISTRY_TOML;
    let slice = zenkey::parse_slice(toml).expect("the shipped snmp slice parses");

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

    let mut unexpected: Vec<&str> = writes
        .iter()
        .copied()
        .filter(|p| !ALLOWED_WRITE_PROCEDURES.contains(p))
        .collect();
    unexpected.sort_unstable();
    assert!(
        unexpected.is_empty(),
        "snmp declared write procedure(s) {unexpected:?} that this test does not know about. \
         An SNMP SET is a different threat model from a poll: it is a decision to make \
         explicitly (see #956 and the crate docs), not one to land by editing a registry file. \
         If it is deliberate, add it to ALLOWED_WRITE_PROCEDURES with the reason."
    );

    let mut missing: Vec<&str> = ALLOWED_WRITE_PROCEDURES
        .iter()
        .copied()
        .filter(|p| !writes.contains(p))
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "the allowlist names write procedure(s) {missing:?} the slice no longer declares — an \
         allowlist that excuses nothing is one nobody will notice growing"
    );
}

/// Every shipped profile's `oid_names` resolve to a *registered* family.
///
/// The catch-all makes `metric_guard` pass for any grammar-valid name, so
/// nothing else would notice a profile publishing under a family the registry
/// has never heard of — which is precisely how a metric ends up with no
/// `# TYPE` and no help text in the exporter (#955).
///
/// Note what this does **not** use: `Subject::parse_metric`. That resolves
/// through the `{device}/{metric...}` catch-all, so it answers `Some` for
/// literally any name and a test built on it would be theatre — see
/// `an_unregistered_family_really_does_not_resolve` below, which is the
/// tripwire for that. The comparison here is against the *declared* subject
/// paths with the rest-var one excluded.
#[test]
fn every_shipped_profile_name_is_a_registered_family() {
    let declared = declared_metric_patterns();
    let set = zensight_sensor_snmp::profile::ProfileSet::builtin();

    let mut unregistered = Vec::new();
    for name in set.all_oid_names().values() {
        // `{index}` is the profile's own placeholder for a table row; the
        // registry spells the same hole `{line}`, `{outlet}` or `{index}`, so
        // compare with a concrete row substituted in on both sides.
        let concrete = name.replace("{index}", "1");
        if !declared.iter().any(|p| pattern_matches(p, &concrete)) {
            unregistered.push(name.clone());
        }
    }
    unregistered.sort();
    unregistered.dedup();
    assert!(
        unregistered.is_empty(),
        "shipped profiles publish {unregistered:?}, which match no registered family. \
         Add them to zensight-common/registry/snmp.toml (and run `zenctl registry lock`), or \
         the exporter emits them with no type and no help."
    );
}

/// Declared telemetry subject paths, base-relative to `{device}/`, with the
/// rest-var catch-all left out — it is what makes every other check on this
/// producer vacuous.
fn declared_metric_patterns() -> Vec<String> {
    let toml = zensight_common::registry::snmp::REGISTRY_TOML;
    let slice = zenkey::parse_slice(toml).expect("the shipped snmp slice parses");
    slice
        .subjects
        .iter()
        .filter(|s| s.class.known() == Some(&zenkey::Class::Telemetry))
        .filter_map(|s| s.path.strip_prefix("{device}/"))
        .filter(|p| !p.contains("..."))
        .map(str::to_string)
        .collect()
}

/// A registry pattern against a concrete metric name: chunk by chunk, with a
/// `{var}` chunk matching any one chunk.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let n: Vec<&str> = name.split('/').collect();
    p.len() == n.len()
        && p.iter()
            .zip(&n)
            .all(|(pc, nc)| pc.starts_with('{') || pc == nc)
}

/// The check above is only worth having if it can fail, and the shape that
/// would silently defeat it is real: `Subject::parse_metric` resolves through
/// the rest-var catch-all, so it answers `Some` for anything at all.
#[test]
fn an_unregistered_family_really_does_not_resolve() {
    let declared = declared_metric_patterns();
    assert!(
        declared
            .iter()
            .any(|p| pattern_matches(p, "ups/battery/status")),
        "a registered family must match"
    );
    assert!(
        !declared
            .iter()
            .any(|p| pattern_matches(p, "ups/battery/no_such_thing")),
        "an unregistered family must not"
    );
    // …and the reason this test exists, pinned:
    assert!(
        zensight_common::registry::snmp::Subject::parse_metric("ups/battery/no_such_thing")
            .is_some(),
        "if parse_metric ever stops resolving everything, the coverage test above can be \
         simplified back to using it"
    );
}
