//! The compiled subject registry (RFC 08): per-producer `Subject`/`ProcedureId`
//! enums, `AnySubject` dispatch, `REGISTRIES`, `registry_toml()`, and
//! `is_registered_telemetry()`.
//!
//! Generated at build time by `zenkey-build` from the registry TOMLs in
//! `zensight-common/registry/*.toml` — edit those files (and the append-only
//! `deprecated.lock` ledger), never this module's output.

// zenkey-build renders a service with no `common`-mapped subjects as a
// `match s { _ => None }` arm (the `@desired` slice is the first such), which
// clippy flags inside the GENERATED code. Module-level allow, because the
// fix belongs upstream in the generator, not in a file nobody edits.
#![allow(clippy::match_single_binding)]

include!(concat!(env!("OUT_DIR"), "/zenkey_registry.rs"));

/// Whether a `TelemetryValue` variant agrees with the subject's declared
/// `kind` (RFC 08 §2, v1.32) — the check `checked_point` runs (#1071).
///
/// **What this catches, and why nothing else could.** The registry declared a
/// subject's path, class, type and cardinality — not whether the number is a
/// counter or a gauge. So `checked_point`'s guard validated *registration* and
/// not *semantics*, and `zensight-sensor-container` published five cumulative
/// counters as `TelemetryValue::Gauge`. Both exporters derive the wire type
/// from the variant and nothing else, so `container_…_oom_kills_total` was
/// scraped as `# TYPE … gauge` and exported to OTLP as a Gauge — which no
/// backend can `rate()` or delta-aggregate. Every sibling sensor happened to
/// get it right; nothing in the tree could have said so.
///
/// `Ok(())` when the subject declares no kind: most do not yet, and an
/// undeclared subject is unjudged, never "wrong". The same asymmetry the
/// `kind-mismatch` doctor check uses on a live bus.
///
/// `Bool` accepts a `Gauge` as well as a `Boolean`: a 0/1 step series is
/// legitimately published either way today, and both render as a gauge.
pub fn kind_matches(
    producer: &str,
    metric: &str,
    value: &crate::TelemetryValue,
) -> Result<(), String> {
    use crate::TelemetryValue as V;
    use ::zenkey::slice::{SliceToken, SubjectKind as K};

    let tail: Vec<&str> = metric.split('/').collect();
    let Some(subject) = parse_subject(producer, ::zenkey::grammar::Class::Telemetry, &tail) else {
        return Ok(()); // registration is `is_registered_telemetry`'s job
    };
    let Some(declared) = subject.kind() else {
        return Ok(()); // undeclared is unjudged, not wrong
    };
    let ok = matches!(
        (declared, value),
        (K::Counter, V::Counter(_))
            | (K::Gauge, V::Gauge(_))
            | (K::Text, V::Text(_))
            | (K::Bool, V::Boolean(_) | V::Gauge(_))
    );
    if ok {
        return Ok(());
    }
    let got = match value {
        V::Counter(_) => "counter",
        V::Gauge(_) => "gauge",
        V::Boolean(_) => "bool",
        V::Text(_) => "text",
        V::Binary(_) => "binary",
    };
    Err(format!(
        "{producer} telemetry {metric:?} is declared kind = {:?} but published as {got}. \
         The exporters read the TelemetryValue variant and nothing else, so this is the \
         wire type a scrape sees — fix the publish site, or change the declaration in \
         zensight-common/registry/{producer}.toml and regenerate registry.lock \
         (a changed kind is incompatible: retire and add a sibling)",
        declared.token()
    ))
}
