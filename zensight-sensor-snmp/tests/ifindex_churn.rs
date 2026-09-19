//! An ifIndex that starts naming a different port says so (#1142).
//!
//! Alert labels carried `if_name`; telemetry labels carried only `index`. So
//! after a renumbering — a reboot, a line-card insertion, a firmware upgrade,
//! which is routine on a switch — `if.in_octets{index="3"}` silently continued
//! describing a **different physical port**. No discontinuity anywhere: the
//! graph is a straight line through two different cables.
//!
//! LibreNMS reports index churn explicitly, and this is that. Two cycles
//! against one simulated agent, with the ifName column rewritten in between.

mod harness;

use std::time::Duration;

use harness::{
    IF_TABLE, IF_X_TABLE, SimAgent, SimMib, collect_alerts, collect_points, rig_with_alerts, text,
    v2c_device,
};
use zensight_common::AlertState;
use zensight_sensor_snmp::alerts::SnmpAlertsConfig;

const IDLE: Duration = Duration::from_millis(400);

fn firing<'a>(
    events: &'a [(zenoh::sample::SampleKind, Option<zensight_common::Alert>)],
    rule: &str,
) -> Vec<&'a zensight_common::Alert> {
    events
        .iter()
        .filter_map(|(kind, alert)| match (kind, alert) {
            (zenoh::sample::SampleKind::Put, Some(a))
                if a.rule == rule && a.state == AlertState::Firing =>
            {
                Some(a)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renumbered_interface_is_reported_and_the_name_follows_the_index() {
    // Cloned before the agent takes it: clones share the underlying table, so
    // the test rewrites the ifName column while the agent serves it — which is
    // exactly what a line-card insertion does to a live switch.
    let mib = SimMib::new().with_system_group().with_if_table(2);
    let live = mib.clone();
    let agent = SimAgent::start(mib).await;
    let mut device = v2c_device("switch01", agent.addr());
    device.walks = vec![IF_TABLE.to_string(), IF_X_TABLE.to_string()];
    device.oids = vec![];

    let mut cfg = SnmpAlertsConfig::default();
    // Only the rule under test: an interface that is up fires nothing else,
    // but keeping the others on would make a failure here ambiguous.
    cfg.interface_down.enabled = false;
    cfg.interface_errors.enabled = false;
    cfg.utilization.enabled = false;
    let ar = rig_with_alerts(device, cfg).await;

    // ── Cycle 1: index 1 is eth0, index 2 is eth1 ───────────────────────────
    ar.rig.poller.poll_once().await.expect("first poll");
    let _ = collect_points(&ar.rig, IDLE).await;
    let events = collect_alerts(&ar, IDLE).await;
    assert!(
        firing(&events, "interface_index_changed").is_empty(),
        "a first cycle has nothing to compare against — it must not report churn \
         on every interface it has just met: {events:?}"
    );

    // ── Cycle 2: the same indexes, with the names carried over ──────────────
    //
    // The label comes from the PREVIOUS cycle's table, so this is the cycle in
    // which it appears at all.
    ar.rig.poller.poll_once().await.expect("second poll");
    let points = collect_points(&ar.rig, IDLE).await;
    let named: Vec<(&String, &String)> = points
        .iter()
        .filter(|(_, p)| p.labels.get("index").is_some_and(|i| i == "1"))
        .filter_map(|(m, p)| p.labels.get("if_name").map(|n| (m, n)))
        .collect();
    assert!(
        !named.is_empty(),
        "interface telemetry must carry if_name beside index (#1142) — without it \
         a series cannot be followed across a renumbering: {:?}",
        points.keys().collect::<Vec<_>>()
    );
    assert!(
        named.iter().all(|(_, n)| n.as_str() == "eth0"),
        "index 1 is eth0: {named:?}"
    );
    let _ = collect_alerts(&ar, IDLE).await;

    // ── The renumbering ─────────────────────────────────────────────────────
    //
    // What a line-card insertion does: the ports shift up an index. Index 1,
    // which was eth0, now names eth1.
    live.set(&format!("{IF_TABLE}.2.1"), text("eth1"));
    live.set(&format!("{IF_X_TABLE}.1.1"), text("eth1"));
    live.set(&format!("{IF_TABLE}.2.2"), text("eth2"));
    live.set(&format!("{IF_X_TABLE}.1.2"), text("eth2"));

    ar.rig.poller.poll_once().await.expect("third poll");
    let _ = collect_points(&ar.rig, IDLE).await;
    let events = collect_alerts(&ar, IDLE).await;
    let churn = firing(&events, "interface_index_changed");
    assert_eq!(
        churn.len(),
        2,
        "both indexes now name a different port: {events:?}"
    );

    let one = churn
        .iter()
        .find(|a| a.labels.get("if_index").is_some_and(|i| i == "1"))
        .expect("index 1 reported");
    assert_eq!(
        one.labels.get("previous_if_name").map(String::as_str),
        Some("eth0")
    );
    assert_eq!(one.labels.get("if_name").map(String::as_str), Some("eth1"));
    assert!(
        one.summary.contains("eth0") && one.summary.contains("eth1"),
        "the summary names both, so the operator can see what moved: {}",
        one.summary
    );

    // ── And the label follows ───────────────────────────────────────────────
    ar.rig.poller.poll_once().await.expect("fourth poll");
    let points = collect_points(&ar.rig, IDLE).await;
    let after: Vec<&String> = points
        .values()
        .filter(|p| p.labels.get("index").is_some_and(|i| i == "1"))
        .filter_map(|p| p.labels.get("if_name"))
        .collect();
    assert!(!after.is_empty(), "index 1 still publishes");
    assert!(
        after.iter().all(|n| n.as_str() == "eth1"),
        "index 1 now names eth1, and the telemetry says so: {after:?}"
    );

    // Reported once, not every cycle from here on. A renumbering is an event.
    let events = collect_alerts(&ar, IDLE).await;
    assert!(
        firing(&events, "interface_index_changed").is_empty(),
        "the churn is over — it must not re-fire every cycle: {events:?}"
    );
}
