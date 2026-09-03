//! Reverse registry conformance for probe (#654 pattern, RFC 08 §6.1).

use zensight_common::registry::probe::Subject;
use zensight_common::registry_audit;

/// No conditional families. Every subject below is emitted by *some* target
/// kind, and a deployment that configures no HTTP target simply has no HTTP
/// series — which is a property of the config, not of the build. The RFC 08
/// §6.1 ledger exists for gauges a BUILD can never produce; that is not this.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

#[test]
fn every_registered_family_has_an_emitter() {
    let emitted: Vec<String> = [
        "forge/up",
        "forge/duration_ms",
        "forge/timeout",
        "forge/http_status",
        "forge/http_ttfb_ms",
        "forge/tls_days_to_expiry",
        "forge/tls_chain_valid",
        "forge/dns_answers",
        // The burst kind (#958). Listed here as the *build* emits them; a
        // deployment with no burst target simply has no burst series, which is
        // a property of the config and not of the build — the distinction this
        // file's header draws.
        "forge/loss_pct",
        "forge/rtt_min_ms",
        "forge/rtt_avg_ms",
        "forge/rtt_max_ms",
        "forge/rtt_p95_ms",
        "forge/jitter_ms",
        // The ntp kind (#959).
        "forge/ntp_offset_ms",
        "forge/ntp_delay_ms",
        "forge/ntp_stratum",
        "forge/ntp_synchronised",
        "targets/total",
        "targets/failing",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    registry_audit::assert_families_covered(
        "probe",
        emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}

/// A prober is a client. It opens connections an operator configured and does
/// nothing else, and a `write` procedure that reaches a *target* appearing in
/// the slice would change that with no other visible sign.
///
/// The allowlist is one entry long and it is not an exception to that rule:
/// `thresholds/set` (#931) rewrites what this sensor **alerts on**, and
/// touches nothing it probes. It is declared `write` because #957 classifies a
/// procedure that changes a host's behaviour as one whose outcome must reach
/// that host's audit trail — accountability for "who changed the rules", not
/// permission to act on a target. Anything else is a decision to take in its
/// own issue, with the #283 gate pattern, and not one to land by editing a
/// registry file.
///
/// Parsed, not grepped: `!toml.contains("kind = \"write\"")` — which this was
/// — also matches the sentence in a comment explaining that there is no write
/// surface. Asking the slice is stronger and immune to its own documentation.
#[test]
fn the_slice_declares_no_write_surface_beyond_its_own_rule_set() {
    const ALLOWED: &[&str] = &["thresholds/set"];
    let toml = zensight_common::registry::probe::REGISTRY_TOML;
    let slice = zenkey::parse_slice(toml).expect("the shipped probe slice parses");
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
        .filter(|path| !ALLOWED.contains(path))
        .collect();
    assert!(
        writes.is_empty(),
        "probe declared write procedure(s) {writes:?}. This sensor is a client only (#820):          it opens connections an operator configured and does nothing else."
    );
}
