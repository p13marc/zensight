//! How fast a link-state change reaches a **subscriber** (#961, SYS-SUP-004).
//!
//! SYS-SUP-004 puts a number on it — a newly connected communication means is
//! detected and its state shown in under 10 seconds — and until now nothing
//! measured or asserted that number anywhere. A requirement with a number in it
//! needs a test with the same number.
//!
//! # What this measures, and what it deliberately does not
//!
//! It measures what a **subscriber receives**, not what a sensor believes it
//! published. Those differ: the sensor's own timing says nothing about
//! encoding, publisher declaration, or delivery, and it is the subscriber's
//! number that the requirement is about.
//!
//! Three legs, because "10 seconds" means three different things depending on
//! what happened, and conflating them is how a bound gets claimed for a path
//! that does not meet it. Two live here; the poller leg lives in
//! `zensight-sensor-sysinfo/tests/detection_latency.rs`, because a sensor crate
//! should not take a dev-dependency on a sibling sensor to run one test:
//!
//! | Leg | Measures | Needs |
//! |---|---|---|
//! | `a_started_sensor_reports_link_state_fast` | sensor start → first link-state sample | nothing |
//! | `a_new_interface_is_detected_fast` | interface appears → sample for it | `CAP_NET_ADMIN` |
//!
//! The second is the literal requirement and the one that needs a capability,
//! so it **skips with a printed reason** when unprivileged rather than failing
//! or, worse, silently passing. The first runs everywhere.
//!
//! Every leg prints its measured figure. A test that only passes or fails
//! cannot show a regression from 400 ms to 8 s — both are "under 10 s", and
//! the second one is a bug.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::{Duration, Instant};

/// The requirement's bound.
const BUDGET: Duration = Duration::from_secs(10);

fn isolated_config() -> zenoh::Config {
    // Fully isolated: no multicast, no listen, no connect. A test that joined
    // the developer's live bus would measure their fleet, and would publish
    // into it.
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    c.insert_json5("listen/endpoints", "[]").unwrap();
    c.insert_json5("connect/endpoints", "[]").unwrap();
    c.insert_json5("timestamping/enabled", "true").unwrap();
    c
}

/// Wait for a sample whose key satisfies `want`, returning how long it took.
async fn wait_for(
    sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    started: Instant,
    want: impl Fn(&str) -> bool,
) -> Option<(Duration, String)> {
    // One budget for the whole wait, not per sample: a stream of samples that
    // never includes the one being waited for must still time out.
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, sub.recv_async()).await {
            Ok(Ok(sample)) => {
                let key = sample.key_expr().to_string();
                if want(&key) {
                    return Some((started.elapsed(), key));
                }
            }
            _ => return None,
        }
    }
}

fn netlink_config() -> zensight_sensor_netlink::config::NetlinkConfig {
    // Built from the serde defaults, so this exercises the SHIPPED
    // configuration rather than one hand-assembled to pass.
    let mut cfg: zensight_sensor_netlink::config::NetlinkConfig =
        serde_json::from_str("{}").expect("netlink config defaults");
    cfg.source = "latency-test".to_string();
    // The evidence feed publishes identity claims; this test is about link
    // state and a quiet bus makes the assertion's failure message legible.
    cfg.evidence.enabled = false;
    cfg
}

/// Leg 1 — a sensor that has just started reports link state well inside the
/// bound.
///
/// This is *not* the SYS-SUP-004 change-detection number; it is the restart
/// case, which matters for a different reason: after a sensor restart an
/// operator's map is blank until this completes, and "under 10 s" is the
/// difference between a redeploy being invisible and being alarming.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_sensor_reports_link_state_fast() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/telemetry/netlink/iface/*/up")
        .await
        .expect("subscribe");

    let collector = zensight_sensor_netlink::collector::Collector::new(
        "latency-test".to_string(),
        netlink_config(),
        session.clone(),
        zensight_common::Format::Json,
        std::sync::Arc::new(zensight_common::PublishCounters::default()),
    );
    let started = Instant::now();
    let handle = tokio::spawn(collector.run());

    let got = wait_for(&sub, started, |k| k.contains("/iface/")).await;
    handle.abort();

    let (elapsed, key) = got.unwrap_or_else(|| {
        panic!(
            "no link-state sample within {BUDGET:?} — the sensor published nothing a subscriber saw"
        )
    });
    // The number, not just the verdict: a regression from 400 ms to 8 s passes
    // this assertion and is still a bug someone needs to see.
    println!("MEASURED start -> first link state: {elapsed:?} on {key}");
    assert!(
        elapsed < BUDGET,
        "first link-state sample took {elapsed:?}, budget {BUDGET:?}"
    );
}

/// Leg 2 — the literal requirement: an interface that appears is detected.
///
/// Needs `CAP_NET_ADMIN` to create a dummy interface. Skipped, loudly, when
/// unprivileged: a capability test that quietly passes without the capability
/// is worse than no test, because it reports a bound nobody measured.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_interface_is_detected_fast() {
    let iface = format!("zslat{}", std::process::id() % 10_000);
    if !can_make_interface(&iface) {
        println!(
            "SKIPPED a_new_interface_is_detected_fast: creating a dummy interface needs \
             CAP_NET_ADMIN and this process does not have it. This is the leg that measures \
             SYS-SUP-004's actual number; run it on the privileged CI leg."
        );
        return;
    }

    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/telemetry/netlink/iface/*/up")
        .await
        .expect("subscribe");

    let collector = zensight_sensor_netlink::collector::Collector::new(
        "latency-test".to_string(),
        netlink_config(),
        session.clone(),
        zensight_common::Format::Json,
        std::sync::Arc::new(zensight_common::PublishCounters::default()),
    );
    let handle = tokio::spawn(collector.run());

    // Let the sensor finish its first sweep, so what is measured below is the
    // detection of a NEW interface and not the startup sweep leg 1 covers.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while sub.try_recv().ok().flatten().is_some() {}

    let created = Instant::now();
    run_ip(&["link", "add", "name", &iface, "type", "dummy"]);
    run_ip(&["link", "set", &iface, "up"]);

    let slug = zenkey::Chunk::slug(&iface).to_string();
    let got = wait_for(&sub, created, |k| k.contains(&format!("/iface/{slug}/"))).await;

    run_ip(&["link", "delete", &iface]);
    handle.abort();

    let (elapsed, key) =
        got.unwrap_or_else(|| panic!("interface {iface} was never reported within {BUDGET:?}"));
    println!("MEASURED interface up -> subscriber: {elapsed:?} on {key}");
    assert!(
        elapsed < BUDGET,
        "detecting {iface} took {elapsed:?}, budget {BUDGET:?}"
    );
}

/// Whether this process can create a dummy interface, tested by trying.
///
/// A capability probe rather than a `geteuid() == 0` check: root is neither
/// necessary (a file capability or an ambient set is enough) nor sufficient (a
/// user namespace without the network namespace is not), and the thing that
/// matters is whether the syscall works.
fn can_make_interface(name: &str) -> bool {
    if !run_ip(&["link", "add", "name", name, "type", "dummy"]) {
        return false;
    }
    run_ip(&["link", "delete", name]);
    true
}

fn run_ip(args: &[&str]) -> bool {
    std::process::Command::new("ip")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
