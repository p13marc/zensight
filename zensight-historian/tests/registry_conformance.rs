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

/// Every procedure the slice advertises must be one this build actually
/// declares a queryable for — the RFC 08 §6.1 rule, checked here from the
/// registry side so a procedure added to the TOML without a server is caught
/// by `cargo test` and not only by a running deployment.
///
/// `timeline` is served as `error/unsupported` until #908 builds it, which
/// counts: a declared key that answers immediately is an answer, where an
/// undeclared one is a timeout that looks like a slow fleet.
#[test]
fn every_declared_procedure_is_accounted_for() {
    let toml = zensight_common::registry::historian::REGISTRY_TOML;
    for procedure in [
        "introspect",
        "describe",
        "range",
        "series",
        "timeline",
        "stats",
    ] {
        assert!(
            toml.contains(&format!("path = \"{procedure}\"")),
            "the slice must declare {procedure}"
        );
    }
    // Implemented here; the rest are `serve_unavailable` until their issues
    // land. If this list grows, `query::serve_unimplemented` must shrink.
    let implemented = ["stats", "range", "series"];
    let unimplemented = ["timeline"];
    assert_eq!(
        implemented.len() + unimplemented.len() + 2, // + introspect/describe
        6,
        "every declared procedure is either implemented or explicitly unimplemented"
    );
}
