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
    // Emitted only for a stream with an H.264 `EncoderStatsHandle` — the JPEG
    // preview paths are timed by `TimedElement` (so they have `encode_ms`) but
    // parallax keeps no histogram for them, and RTSP passthrough has no encoder
    // at all (#729). Runtime-conditional, like the two above.
    "cam0/stats/encode_p95_ms",
    "cam0/stats/encode_p99_ms",
    // The receiver-feedback aggregate (#715). Emitted only for a tier with at
    // least one live report — no viewer reporting means no consumer to count,
    // which is a *runtime* condition like `rc_drops` above and not a build
    // one. The timing families additionally require that a consumer actually
    // measured the thing: RFC 07 §1.3 makes "unstamped is not asked, never
    // zero" normative, so a tier whose consumers report no frame age publishes
    // no frame age rather than a confident 0.
    "cam0/rx/high/consumers",
    "cam0/rx/high/loss_pct_max",
    "cam0/rx/high/loss_pct_p50",
    "cam0/rx/high/frame_age_ms_max",
    "cam0/rx/high/frame_age_ms_p50",
    "cam0/rx/high/decode_queue_max",
    "cam0/rx/high/decode_queue_p50",
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

/// The registry declares the rate ceiling the sensor actually enforces (#715).
///
/// RFC 07 §1.1 says a report's rate *"belongs in the registry entry rather than
/// in prose"*. RFC 08 §2 scopes `rate` to `events` subjects, and `zenkey-build`
/// 0.7 does not read it on a procedure — so nothing upstream checks this, and
/// the field could drift from the code silently while `introspect` kept serving
/// it to the fleet verbatim.
///
/// This is that check, locally. If the constant moves, move the registry entry;
/// if the registry entry is wrong, the fleet has been told a number no build
/// enforces.
#[test]
fn the_registry_declares_the_rate_ceiling_the_sensor_enforces() {
    let toml = include_str!("../../zensight-common/registry/parallax.toml");
    let doc: toml::Value = toml::from_str(toml)
        .unwrap_or_else(|e| panic!("parallax.toml does not parse: {}", e.message()));
    let procedures = doc["procedure"].as_array().expect("[[procedure]] entries");
    let report = procedures
        .iter()
        .find(|p| p["path"].as_str() == Some("stream/report"))
        .expect("stream/report is registered");

    let declared = report["rate"]
        .as_str()
        .expect("stream/report declares a rate ceiling (RFC 07 §1.1)");
    let per_hour = 3600 / zensight_sensor_parallax::reports::REPORT_MIN_INTERVAL.as_secs();
    assert_eq!(
        declared,
        format!("burst({per_hour}/h)"),
        "the declared rate ceiling and reports::REPORT_MIN_INTERVAL disagree — \
         `introspect` serves this file verbatim, so the fleet would be told a \
         number no build enforces"
    );

    assert_eq!(
        report["kind"].as_str(),
        Some("write"),
        "a report is a write, not a read (RFC 07 §1.1)"
    );
    assert_eq!(
        report["idempotent"].as_bool(),
        Some(true),
        "a report is a snapshot with cumulative counters, so a retry repeats a \
         statement rather than adding to one"
    );
    assert!(
        report.get("fanout").is_none(),
        "stream/report must not declare fanout: the write default is forbidden \
         (RFC 08 §2 G2), which makes a fleet-wide report about a stream one host \
         publishes unrepresentable rather than merely discouraged"
    );
}
