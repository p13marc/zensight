//! A misspelled config key is a startup refusal, not a silent default (#1150).
//!
//! `zensight-sensor-sysinfo/src/config.rs` records what the old behaviour cost:
//! `temperatures` and `power` "stayed dark for so long" because a key in the
//! wrong block parses clean and takes the Rust default. On a production host
//! that is indistinguishable from a key nobody wrote.
//!
//! The mechanism is [`SensorConfig::parse_strict`] — `serde_ignored`, lifted
//! out of `zensight-sensor-logs` (#547) where it was the tree's only strict
//! loader. It is deliberately **not** `serde(deny_unknown_fields)`: that errors
//! at the first struct to see a stray key, so the message names the field but
//! not where it sits, and it aborts before a collector could run, which rules
//! out both the full dotted path and the mixed-version escape hatch.

use serde::Deserialize;
use zensight_common::config::{LoggingConfig, ZenohConfig};
use zensight_sensor_core::SensorConfig;

/// A config shaped like a real sensor's: a `zenoh` block, a `logging` block and
/// one producer block with a nested struct.
#[derive(Debug, Clone, Default, Deserialize)]
struct TestConfig {
    #[serde(default)]
    zenoh: ZenohConfig,
    #[serde(default)]
    logging: LoggingConfig,
    #[serde(default)]
    probe: ProbeBlock,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProbeBlock {
    #[serde(default)]
    poll_interval_secs: u64,
    #[serde(default)]
    targets: Vec<Target>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Target {
    /// Declared so `nmae` has something to be a typo *of*; the value itself is
    /// beside the point here.
    #[serde(default)]
    #[allow(dead_code, reason = "the field exists to be misspelled at")]
    name: String,
}

impl SensorConfig for TestConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }
    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }
    fn producer(&self) -> &str {
        "test"
    }
}

#[test]
fn a_misspelled_key_is_refused_and_the_error_names_it() {
    // The issue's acceptance criterion, spelled exactly: `poll_interval_sec`
    // for `poll_interval_secs`. One character, and before #1150 it meant the
    // sensor polled at the Rust default forever while the operator read their
    // own config and saw the interval they had set.
    let err = TestConfig::parse_strict(
        r#"{ zenoh: { mode: "peer" }, probe: { poll_interval_sec: 30 } }"#,
    )
    .expect_err("a key no struct declares must not load");
    let msg = err.to_string();
    assert!(msg.contains("probe.poll_interval_sec"), "got: {msg}");
    assert!(
        msg.contains("allow_unknown_fields"),
        "the error must say what to do about it: {msg}"
    );
}

#[test]
fn the_error_carries_the_full_path_not_just_the_field() {
    // This is the whole reason for `serde_ignored` over `deny_unknown_fields`.
    // "unknown field `name`" is a fair description of the problem and no help
    // at all in a config with four blocks and a list of targets.
    let err =
        TestConfig::parse_strict(r#"{ probe: { targets: [ { name: "a" }, { nmae: "b" } ] } }"#)
            .expect_err("a nested typo must not load");
    assert!(
        err.to_string().contains("probe.targets.1.nmae"),
        "got: {err}"
    );
}

#[test]
fn every_unknown_key_is_named_at_once() {
    // Not the first one and then a second run to find the next: an operator
    // fixing a config wants the list.
    let err =
        TestConfig::parse_strict(r#"{ probe: { a: 1, b: 2 }, c: 3 }"#).expect_err("must not load");
    let msg = err.to_string();
    for key in ["probe.a", "probe.b", "c"] {
        assert!(msg.contains(key), "{key} missing from: {msg}");
    }
}

#[test]
fn the_zenoh_block_stays_forward_compatible() {
    // The standing exemption. `zenoh` is the one block a newer participant must
    // be able to hand to an older one mid-rollout — refusing an unknown
    // transport knob would turn a staged upgrade into an outage.
    TestConfig::parse_strict(r#"{ zenoh: { mode: "peer", some_future_transport_knob: true } }"#)
        .expect("an unknown zenoh key is tolerated on purpose");
}

#[test]
fn the_escape_hatch_downgrades_the_refusal_to_a_warning() {
    // For a mixed-version fleet sharing one file. It is read off the raw tree,
    // so a config struct does not have to declare it to honour it.
    let cfg = TestConfig::parse_strict(
        r#"{ allow_unknown_fields: true, future_knob: 42, probe: { alos: 1 } }"#,
    )
    .expect("the escape hatch must allow extras");
    assert_eq!(cfg.producer(), "test");
}

#[test]
fn the_escape_hatch_itself_is_not_an_unknown_key() {
    // It is declared by no config struct in the tree but logs', so without the
    // exemption setting it would be the thing that fails.
    TestConfig::parse_strict(r#"{ allow_unknown_fields: false }"#)
        .expect("the hatch's own key never trips the check");
}

#[test]
fn a_correct_config_still_loads() {
    let cfg = TestConfig::parse_strict(
        r#"{ zenoh: { mode: "peer" },
             probe: { poll_interval_secs: 30, targets: [ { name: "a" } ] } }"#,
    )
    .expect("the strictness must not cost a valid config");
    assert_eq!(cfg.probe.poll_interval_secs, 30);
    assert_eq!(cfg.probe.targets.len(), 1);
}
