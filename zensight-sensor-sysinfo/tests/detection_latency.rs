//! How fast a **poller** reaches a subscriber (#961, SYS-SUP-004).
//!
//! The companion to `zensight-sensor-netlink/tests/detection_latency.rs`, which
//! covers the event-driven legs. Split by crate rather than kept together
//! because a sensor crate should not take a dev-dependency on a sibling sensor
//! to run one test.
//!
//! SYS-SUP-004 puts a number on detection — under 10 seconds — and whether a
//! *poller* meets it is a **configuration** question, not a code one. That is
//! the point this test pins: `sysinfo` at its 5 s default fits with slack;
//! `snmp` at 30 s does not, and `docs/latency.md` says so rather than letting
//! the bound be claimed fleet-wide.
//!
//! Like its companion it measures what a **subscriber receives**, not what the
//! sensor believes it published, and it prints the measured figure — a
//! regression from 5 s to 9.5 s passes the assertion and is still a bug.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::{Duration, Instant};

/// The requirement's bound.
const BUDGET: Duration = Duration::from_secs(10);

/// `sysinfo`'s default poll interval, and what makes it able to meet a 10 s
/// bound at all.
const POLLER_INTERVAL_SECS: u64 = 5;

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

/// A 5 s poller meets the bound — which is why this is a config question and
/// not a code question.
///
/// **Measures the gap between consecutive samples, not the time to the first
/// one.** The first sample is immediate (the collector polls before it sleeps),
/// so timing it would report ~30 ms and claim a bound the poller does not
/// actually offer. A change that happens just after a poll is not visible until
/// the next one, so the *interval* is the worst-case detection latency and the
/// only figure the requirement can be checked against.
///
/// `sysinfo` at 5 s therefore fits inside 10 s with slack. `snmp` at its 30 s
/// default does not, and `docs/latency.md` says so rather than letting the
/// bound be claimed fleet-wide.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_five_second_poller_meets_the_bound() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/telemetry/sysinfo/network/*/rx_bytes")
        .await
        .expect("subscribe");

    // Built from the serde defaults, so this test exercises the SHIPPED
    // configuration rather than one hand-assembled to pass.
    let mut cfg: zensight_sensor_sysinfo::config::SysinfoConfig =
        serde_json::from_str("{}").expect("sysinfo config defaults");
    cfg.poll_interval_secs = POLLER_INTERVAL_SECS;
    let collector = zensight_sensor_sysinfo::collector::SystemCollector::new(
        "latency-test".to_string(),
        cfg,
        session.clone(),
        zensight_common::Format::Json,
    );
    let started = Instant::now();
    let handle = tokio::spawn(collector.run());

    let (first, key) = wait_for(&sub, started, |k| k.contains("/network/"))
        .await
        .unwrap_or_else(|| panic!("no network sample within {BUDGET:?}"));
    println!("MEASURED poller start -> first network sample: {first:?} on {key}");

    // Drain the rest of this sweep's samples, then time the next one: that gap
    // is the poll interval as a subscriber experiences it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    while sub.try_recv().ok().flatten().is_some() {}

    let after_drain = Instant::now();
    let (gap, key) = wait_for(&sub, after_drain, |k| k.contains("/network/"))
        .await
        .unwrap_or_else(|| {
            panic!("no SECOND network sample within {BUDGET:?} — the poller ran once and stopped")
        });
    handle.abort();

    println!("MEASURED poller sample-to-sample gap: {gap:?} on {key}");
    assert!(
        gap < BUDGET,
        "a {POLLER_INTERVAL_SECS}s poller's worst-case detection latency is {gap:?}, \
         budget {BUDGET:?}"
    );
    // And it really is the configured cadence, not an accident of a busy loop
    // that would pass the bound while burning a core.
    assert!(
        gap >= Duration::from_secs(POLLER_INTERVAL_SECS) - Duration::from_millis(1500),
        "samples arrived {gap:?} apart, far faster than the configured \
         {POLLER_INTERVAL_SECS}s — the poller is not respecting its interval"
    );
}
