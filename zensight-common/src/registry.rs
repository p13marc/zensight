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
    // A histogram (RFC 08 §2, v1.36) is judged twice: the variant, and its
    // bounds against the declared `buckets` — equal bit for bit, or two
    // producers of one subject are not comparable, which is the whole reason
    // the declaration is required. A malformed value is refused here too.
    if let (K::Histogram, V::Histogram(h)) = (declared, value) {
        let declared_bounds = subject.buckets().unwrap_or_default();
        if !h.same_bounds(declared_bounds) {
            return Err(format!(
                "{producer} telemetry {metric:?} is a histogram over {:?}, but the registry \
                 declares buckets = {declared_bounds:?} (RFC 08 §2): publish over the declared \
                 bounds, or change the declaration in zensight-common/registry/{producer}.toml",
                h.buckets
            ));
        }
        if !h.is_consistent() {
            return Err(format!(
                "{producer} telemetry {metric:?} is a malformed histogram: {} count(s) for {} \
                 bound(s), count {} against a sum of counts of {}",
                h.counts.len(),
                h.buckets.len(),
                h.count,
                h.counts.iter().sum::<u64>()
            ));
        }
        return Ok(());
    }
    let got = match value {
        V::Counter(_) => "counter",
        V::Gauge(_) => "gauge",
        V::Boolean(_) => "bool",
        V::Text(_) => "text",
        V::Binary(_) => "binary",
        V::Histogram(_) => "histogram",
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

#[cfg(test)]
mod histogram_kind_tests {
    use super::{kind_matches, probe};
    use crate::{HistogramValue, TelemetryValue};

    fn declared() -> &'static [f64] {
        probe::Subject::duration_seconds("web")
            .buckets()
            .expect("the probe declares its buckets")
    }

    /// #1151: a histogram subject accepts a histogram over exactly its
    /// declared bounds, and refuses one over any other bounds, a malformed
    /// one, and a scalar.
    #[test]
    fn a_histogram_subject_holds_the_value_to_its_declared_bounds() {
        let metric = "web/duration_seconds";
        let mut ok = HistogramValue::new(declared());
        ok.observe(0.02);
        assert_eq!(
            kind_matches("probe", metric, &TelemetryValue::Histogram(ok.clone())),
            Ok(())
        );

        let other = HistogramValue::new(&[0.1, 1.0]);
        let e = kind_matches("probe", metric, &TelemetryValue::Histogram(other)).unwrap_err();
        assert!(e.contains("declares buckets"), "{e}");

        let mut bad = ok;
        bad.count += 1;
        let e = kind_matches("probe", metric, &TelemetryValue::Histogram(bad)).unwrap_err();
        assert!(e.contains("malformed histogram"), "{e}");

        let e = kind_matches("probe", metric, &TelemetryValue::Gauge(0.02)).unwrap_err();
        assert!(
            e.contains("declared kind = \"histogram\" but published as gauge"),
            "{e}"
        );
    }

    /// And a histogram published under a gauge subject is the variant
    /// mismatch it always was.
    #[test]
    fn a_histogram_on_a_gauge_subject_is_refused() {
        let h = HistogramValue::new(&[1.0]);
        let e = kind_matches("probe", "web/rtt_p95_ms", &TelemetryValue::Histogram(h));
        if let Some(k) = probe::Subject::rtt_p95_ms("web").kind() {
            assert_eq!(k, zenkey::SubjectKind::Gauge);
            assert!(e.unwrap_err().contains("published as histogram"));
        }
    }
}
