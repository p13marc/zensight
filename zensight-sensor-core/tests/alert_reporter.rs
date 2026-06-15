//! Integration tests for `AlertReporter` lifecycle over an in-process Zenoh peer.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::{Alert, AlertKind, AlertSeverity, AlertState, Format, Protocol, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher};

/// A standalone Zenoh config: scouting disabled so concurrent test peers don't
/// discover each other and cross-contaminate the shared `@/alerts/**` space.
/// Local pub/sub within one session still works.
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

fn unique_source() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("host_{}", nanos)
}

fn sample_alert(source: &str) -> Alert {
    Alert::new(
        source,
        Protocol::Netlink,
        AlertKind::Expectation,
        "ssh-listening",
        AlertSeverity::Critical,
        "sshd not listening on :22",
    )
    .with_label("port", "22")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fires_then_resolves() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("zensight/netlink/@/alerts/**")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "zensight/netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    let alert = sample_alert(&source);

    // for_duration = 0 → fires immediately.
    reporter
        .observe(alert.clone(), Some(Duration::ZERO))
        .await
        .expect("observe");

    let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("recv firing timed out")
        .expect("recv firing");
    assert_eq!(s.kind(), zenoh::sample::SampleKind::Put);
    let got: Alert = decode_auto(&s.payload().to_bytes()).expect("decode firing");
    assert_eq!(got.state, AlertState::Firing);
    assert_eq!(got.rule, "ssh-listening");
    assert_eq!(reporter.active_count(), 1);

    // Reconcile with nothing still firing → resolve + delete tombstone.
    reporter
        .reconcile("ssh-listening", &[])
        .await
        .expect("reconcile");

    // Expect a Put(Resolved) and a Delete (order: put then delete).
    let mut saw_resolved = false;
    let mut saw_delete = false;
    for _ in 0..2 {
        let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("recv resolve timed out")
            .expect("recv resolve");
        match s.kind() {
            zenoh::sample::SampleKind::Put => {
                let got: Alert = decode_auto(&s.payload().to_bytes()).expect("decode resolved");
                assert_eq!(got.state, AlertState::Resolved);
                saw_resolved = true;
            }
            zenoh::sample::SampleKind::Delete => saw_delete = true,
        }
    }
    assert!(saw_resolved, "expected a Put(Resolved)");
    assert!(saw_delete, "expected a Delete tombstone");
    assert_eq!(reporter.active_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debounce_suppresses_first_observe() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("zensight/netlink/@/alerts/**")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "zensight/netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    // Long debounce: the first observe must NOT publish.
    reporter
        .observe(sample_alert(&source), Some(Duration::from_secs(3600)))
        .await
        .expect("observe");

    let res = tokio::time::timeout(Duration::from_millis(500), sub.recv_async()).await;
    assert!(
        res.is_err(),
        "no alert should be published before debounce elapses"
    );
    assert_eq!(reporter.active_count(), 0);
}
