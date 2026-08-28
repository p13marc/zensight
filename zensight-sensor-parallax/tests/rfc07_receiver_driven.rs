//! RFC 07 §1.2, enforced by construction: **the report path holds no encoder
//! controls** (#715).
//!
//! # Why this is a grep and not a unit test
//!
//! §1.2 is normative and negative: a producer MUST NOT re-tune a shared tier
//! from one consumer's report. A negative like that has no natural unit test —
//! you cannot assert that a function nobody wrote was not called. What you
//! *can* assert is that the module holding receiver feedback has no way to
//! reach the knobs, which is a property of its imports.
//!
//! `src/reports.rs` deliberately takes an `Arc<ReceiverReports>` and nothing
//! else: no `SessionHandle`, no `SessionMsg`, no `PipelineControls`. That is
//! the difference between a comment and an invariant. A comment saying "do not
//! re-tune from a report" is obeyed until the next person wires up something
//! helpful; a module that cannot reach an encoder is obeyed by the compiler.
//!
//! This repo already enforces architecture by grep in CI — the design-system
//! colour guard, the `session.put` ban, the `declare_queryable` ban — so the
//! shape is idiomatic here rather than novel. It lives as a test as well as a
//! CI step so a branch that never runs the parallax suite still fails.
//!
//! The failure this catches, concretely: two viewers share a tier, one reports
//! loss, the bitrate drops, and the *healthy* viewer's picture degrades for a
//! reason it cannot see, caused by a peer it does not know exists. The
//! sanctioned adaptation is the consumer changing which tier key it subscribes
//! to; the escape hatch for a viewer that needs its own rate is a tier of its
//! own, never a mutation of a shared one.

/// Types that reach an encoder. None of them may appear in `src/reports.rs`.
const FORBIDDEN: &[&str] = &[
    "PipelineControls",
    "EncoderControl",
    "ScaleControl",
    "ThrottleControl",
    "SessionHandle",
    "SessionMsg",
    "KeyframeHandle",
    "request_keyframe",
    "set_bitrate",
];

#[test]
fn the_report_path_holds_no_encoder_controls() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reports.rs"),
    )
    .expect("src/reports.rs");

    // The module doc explains the rule and necessarily names the types, so
    // judge the code rather than the prose.
    let code: String = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("//!") || t.starts_with("///") || t.starts_with("//"))
        })
        .collect::<Vec<_>>()
        .join("\n");

    for forbidden in FORBIDDEN {
        assert!(
            !code.contains(forbidden),
            "src/reports.rs names `{forbidden}`. RFC 07 §1.2 forbids re-tuning a \
             shared tier from a consumer's report, and this module's whole design \
             is that it CANNOT — it takes an Arc<ReceiverReports> and nothing \
             else. If a control path genuinely belongs here, §1.2 requires a \
             stated arbitration rule that is not 'the most recent report', and \
             this test should be replaced by one that pins it."
        );
    }
}

/// The store is constructed from config and holds only data — tier names and a
/// window — so even its *fields* offer no route to a pipeline.
#[test]
fn the_report_store_holds_only_data() {
    let config = zensight_sensor_parallax::config::ParallaxConfig::default();
    let reports = zensight_sensor_parallax::reports::ReceiverReports::from_config(&config);
    // If this ever needs a session, a handle or a control, the signature above
    // stops compiling — which is the point.
    let _ = reports.aggregate(std::time::Instant::now());
}
