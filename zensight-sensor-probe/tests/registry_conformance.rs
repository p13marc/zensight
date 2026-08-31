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
/// nothing else — no listener, no write surface — and a `write` procedure
/// appearing in the slice would change that with no other visible sign.
#[test]
fn the_slice_declares_no_write_surface() {
    let toml = zensight_common::registry::probe::REGISTRY_TOML;
    assert!(
        !toml.contains(r#"kind = "write""#),
        "probe declared a write procedure. This sensor is a client only (#820)."
    );
}
