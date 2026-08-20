//! Both directions of RFC 08 §5/§6.1 for the parallax subject registry.
//!
//! The forward direction (*published ⊆ registered*) is already enforced at
//! runtime by `telemetry_guard::checked_point`, which debug-panics on an
//! unregistered name. This file adds the reverse (*registered ⊆ emittable*,
//! #648/#654): a family the registry advertises that no build can publish is a
//! surface `introspect` promises the fleet and nobody delivers.
//!
//! Parallax builds its metric names inline with `format!` rather than through a
//! mapper returning a `Vec`, so — as with sysinfo's collector-built families —
//! coverage is asserted from a list of representatives, one per family, pinned
//! against `src/stats.rs`.

use zensight_common::registry::parallax::Subject;
use zensight_common::registry_audit;

/// One representative metric per family `src/stats.rs` publishes.
///
/// `streams/advertised` is the baseline presence gauge; the rest are per-stream
/// and emitted inside the `for (stream, stats) in open` loop.
const EMITTED: &[&str] = &[
    "streams/advertised",
    "cam0/stats/fps",
    "cam0/stats/kbps",
    "cam0/stats/drops",
    // Emitted only for a stream that ran a rate-controlled encoder — a preview
    // or an RTSP passthrough has none, and reports nothing rather than zero
    // (#510). Runtime-conditional like `encode_ms` below, not build-conditional.
    "cam0/stats/rc_drops",
    "cam0/stats/viewers",
    // Emitted only when `derive()` produced an encode time — i.e. when frames
    // were actually encoded this interval. That is a *runtime* condition, not a
    // build one, so it is covered rather than a ledger entry: this build can
    // publish it, which is the question the reverse check asks.
    "cam0/stats/encode_ms",
];

/// Registered parallax telemetry families this build can never emit, and why.
///
/// Empty, and the audit helper keeps it honest: an entry the build *does* emit
/// fails, and so does an entry the registry no longer declares.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

/// Forward: everything in the representative list is a registered subject.
/// Without this the reverse test below could pass on a list of typos.
#[test]
fn every_representative_is_registered() {
    for metric in EMITTED {
        assert!(
            zensight_common::registry::is_registered_telemetry("parallax", metric),
            "{metric:?} is not a registered parallax subject — add it to \
             zensight-common/registry/parallax.toml (RFC 08 §5)"
        );
    }
}

/// Reverse: every registered family has an emitter (#654).
#[test]
fn every_registered_family_has_an_emitter() {
    registry_audit::assert_families_covered(
        "parallax",
        EMITTED,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}
