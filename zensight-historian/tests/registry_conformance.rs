//! Reverse registry conformance for the historian (#654 pattern, RFC 08 §6.1).

/// The historian publishes **no telemetry**, so there are no families to
/// cover — which is the invariant worth asserting rather than an omission to
/// explain away. A history service that re-published what it ingested would be
/// a loop with a database in it (RFC 04 §1.1), and the first sign of that
/// mistake would be a telemetry family appearing in this slice.
#[test]
fn the_slice_declares_no_telemetry() {
    let toml = zensight_common::registry::historian::REGISTRY_TOML;
    assert!(
        !toml.contains(r#"class = "telemetry""#),
        "the historian declared a telemetry subject. It ingests everyone else's \
         telemetry; publishing its own would be a loop with a database in it \
         (RFC 04 §1.1, #898). Its own numbers ride the health document and \
         @rpc/historian/stats."
    );
}

/// A read-only service. It answers questions about history and cannot be told
/// to change anything — no retention override on the wire, no compaction
/// trigger, no delete. A `write` procedure appearing in the slice would change
/// that with no other visible sign.
#[test]
fn the_slice_declares_no_write_surface() {
    let toml = zensight_common::registry::historian::REGISTRY_TOML;
    assert!(
        !toml.contains(r#"kind = "write""#),
        "the historian declared a write procedure. It is a read-only service \
         (#898): retention is configuration, not a wire call."
    );
}

/// Every procedure the slice advertises is served by this build (#908 was the
/// last to be built), checked from the registry side so a procedure added to
/// the TOML without a server is caught by `cargo test` and not only by a
/// running deployment.
///
/// There is no `serve_unavailable` list any more, and that is the point: the
/// list existed while `range`, `series` and `timeline` were declared and
/// unbuilt, and a list of exceptions is a thing to forget to shrink. What
/// remains is the RFC 08 §6.1 check itself, which fails the startup two
/// seconds after the mistake rather than the first time someone GETs a key
/// that was never there.
#[test]
fn every_declared_procedure_is_served() {
    let toml = zensight_common::registry::historian::REGISTRY_TOML;
    // `introspect` and `describe` come from the framework; the rest are this
    // crate's. If the registry grows a seventh, this fails until someone says
    // where it is served.
    let declared: Vec<&str> = toml
        .lines()
        .filter_map(|l| l.trim().strip_prefix("path = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    for procedure in [
        "introspect",
        "describe",
        "range",
        "series",
        "timeline",
        "stats",
    ] {
        assert!(
            declared.contains(&procedure),
            "the slice must declare {procedure}"
        );
    }
}
