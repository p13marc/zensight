//! #825 item 3, pinned: a trap STORM cannot become an alert storm.
//!
//! The trap pipeline's alert half is bounded by construction — every
//! identical (rule, device, if_index) trap builds a byte-identical
//! `alert_key`, and `AlertReporter::observe` is idempotent while that key is
//! already firing. So N identical `linkDown`s produce N durable
//! `EventRecord`s (that is the record of what happened — events are the
//! trap's home, RFC 12 §4) but exactly ONE alert publication. This test
//! replays the exact alert shape `TrapReceiver::apply_alert_rules` builds,
//! 50 times, and counts what reaches the bus.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::{Alert, AlertKind, AlertSeverity, Format, Protocol};
use zensight_sensor_core::{AlertReporter, Publisher};

fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}

/// The exact construction `apply_alert_rules` uses for a fire rule
/// (trap.rs — device + optional if_index labels, Expectation kind).
fn trap_alert(device: &str) -> Alert {
    Alert::new(
        device,
        Protocol::Snmp,
        AlertKind::Expectation,
        "trap_link_down",
        AlertSeverity::Warning,
        format!("{device}: trap_link_down fired"),
    )
    .with_label("device", device)
    .with_label("if_index", "3")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trap_storm_is_one_alert() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/snmp/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let reporter = AlertReporter::new(
        Publisher::new(session.clone(), "snmp", Format::Json),
        Protocol::Snmp,
        Format::Json,
    );

    // The storm: 50 identical traps, exactly as the receiver replays them
    // (Duration::ZERO — a trap is a single observation, trap.rs says why).
    for _ in 0..50 {
        reporter
            .observe(trap_alert("switch01"), Some(Duration::ZERO))
            .await
            .expect("observe");
    }
    assert_eq!(reporter.active_count(), 1, "one key, however many traps");

    // Exactly one publication reaches the bus.
    let first = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("the one firing sample")
        .expect("sample");
    assert_eq!(first.kind(), zenoh::sample::SampleKind::Put);
    let extra = tokio::time::timeout(Duration::from_millis(700), sub.recv_async()).await;
    assert!(
        extra.is_err(),
        "a second publication arrived — the storm amplified: {extra:?}"
    );

    // Severity escalation is the ONE thing that re-publishes (deliberate:
    // a worsening alert must not be muted by idempotence).
    let mut worse = trap_alert("switch01");
    worse.severity = AlertSeverity::Critical;
    reporter
        .observe(worse, Some(Duration::ZERO))
        .await
        .expect("observe escalation");
    let esc = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("the escalation sample")
        .expect("sample");
    assert_eq!(esc.kind(), zenoh::sample::SampleKind::Put);
}
