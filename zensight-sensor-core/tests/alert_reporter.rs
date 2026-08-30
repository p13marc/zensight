//! Integration tests for `AlertReporter` lifecycle over an in-process Zenoh peer.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::v1::V1ContextExt;
use zensight_common::{Alert, AlertKind, AlertSeverity, AlertState, Format, Protocol, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher};

/// A standalone Zenoh config: scouting disabled so concurrent test peers don't
/// discover each other and cross-contaminate the shared alert state space.
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
        .declare_subscriber("v1/*/state/netlink/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
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
        .declare_subscriber("v1/*/state/netlink/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
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

/// `reconcile_labeled` resolves only alerts carrying the matching label —
/// the proxy-sensor case (snmp/modbus/gnmi) where several observed devices
/// share one reporter and each device sweeps independently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_labeled_scopes_to_the_label() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    let alert_a = sample_alert(&source).with_label("device", "dev-a");
    let alert_b = sample_alert(&source).with_label("device", "dev-b");
    reporter
        .observe(alert_a.clone(), Some(Duration::ZERO))
        .await
        .expect("observe a");
    reporter
        .observe(alert_b, Some(Duration::ZERO))
        .await
        .expect("observe b");
    assert_eq!(reporter.active_count(), 2);

    // dev-b's clean sweep: only dev-b's alert resolves.
    reporter
        .reconcile_labeled("ssh-listening", "device", "dev-b", &[])
        .await
        .expect("reconcile b");
    let firing = reporter.firing_alerts();
    assert_eq!(firing.len(), 1);
    assert_eq!(firing[0].labels["device"], "dev-a");

    // dev-a's sweep with its key still firing keeps it.
    reporter
        .reconcile_labeled("ssh-listening", "device", "dev-a", &[alert_a.alert_key()])
        .await
        .expect("reconcile a keep");
    assert_eq!(reporter.active_count(), 1);

    // dev-a recovered: now empty.
    reporter
        .reconcile_labeled("ssh-listening", "device", "dev-a", &[])
        .await
        .expect("reconcile a clear");
    assert_eq!(reporter.active_count(), 0);
}

/// **The `host.*` stability invariant** (#738).
///
/// `AlertReporter.active` is keyed by `Alert::alert_key()`, and the resolve
/// path re-derives that key from the (possibly re-stamped) alert. So if a
/// `host.*` annotation changing between fire and resolve changed the key, the
/// `Firing` would sit on the old key forever while the `Resolved` + tombstone
/// landed on a new one — a permanent phantom alert, with nothing logged.
///
/// This is why ZenSight's `host.*` namespace is *documented host-scoped* and
/// excluded from the derivation, which RFC 11 §3.1 explicitly provides for
/// ("the label named `host`, **and any label the producer documents as
/// host-scoped**, are excluded before sorting"). Adopting
/// `zenkey::alert::alert_key` without passing that vocabulary through fails
/// here, which is the point of the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_annotation_change_does_not_orphan_a_firing_alert() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();

    // Fire, stamped with one identity.
    let firing = sample_alert(&source).with_label("host.id", "h-aaaaaaaaaaaa");
    reporter
        .observe(firing.clone(), Some(Duration::ZERO))
        .await
        .expect("observe");
    assert_eq!(reporter.active_count(), 1, "the alert fired");

    // The identity envelope refreshes mid-flight — a re-mint, a boot-id
    // change, a late-arriving `host.id`. Everything that identifies the
    // *alert* (rule, discriminating labels) is untouched.
    let restamped = sample_alert(&source).with_label("host.id", "h-bbbbbbbbbbbb");
    assert_eq!(
        firing.alert_key(),
        restamped.alert_key(),
        "a host.* annotation must not re-key a firing alert"
    );

    // Resolving the re-stamped alert must clear the entry the first one made.
    reporter
        .resolve_matching("ssh-listening", &[("port", "22")])
        .await
        .expect("resolve");
    assert_eq!(
        reporter.active_count(),
        0,
        "the firing alert was orphaned: its Resolved landed on a different key"
    );
    assert!(reporter.firing_alerts().is_empty(), "alert list not empty");
}

/// Alert puts are encoding-stamped with the *reporter's* format, and the
/// seed replies ride that same format (#830).
///
/// Two agreements, both previously accidental. The put path went through an
/// unstamped `put`, so consumers resolved the payload by first-byte sniff —
/// which reads an empty or non-JSON body as CBOR (that sniff is how a
/// tombstone became a `payload-undecodable` finding in the conformance
/// gate). And the seed hardcoded `serde_json::to_vec` while the live samples
/// used `encode(alert, self.format)`; they agreed only because every sensor
/// happened to pass `Format::Json`. The reporter here deliberately uses CBOR
/// over a JSON-format publisher session, so either regression — stamping the
/// session's format instead of the reporter's, or the seed falling back to
/// JSON — fails by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alert_puts_are_stamped_and_the_seed_rides_the_reporter_format() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/netlink/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Session format JSON, reporter format CBOR — the reporter's must win.
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher,
        Protocol::Netlink,
        Format::Cbor,
    ));

    let source = unique_source();
    reporter
        .observe(sample_alert(&source), Some(Duration::ZERO))
        .await
        .expect("observe");

    let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("recv firing timed out")
        .expect("recv firing");
    assert_eq!(
        *s.encoding(),
        Format::Cbor.encoding(),
        "a live alert put must be stamped with the reporter's encoding, not \
         sniffed and not the session's (RFC 08 §7, #830)"
    );
    let got: Alert = decode_auto(&s.payload().to_bytes()).expect("decode firing");
    assert_eq!(got.state, AlertState::Firing);

    let selector = format!(
        "{}/*",
        reporter.publisher().v1().const_state_key(&["alert"])
    );
    let seed = tokio::spawn(zensight_sensor_core::serve_alerts_query(reporter.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let replies = session
        .get(&selector)
        .timeout(Duration::from_secs(5))
        .await
        .expect("seed get");
    let mut seen = 0;
    while let Ok(reply) = replies.recv_async().await {
        let sample = reply.result().expect("value reply");
        let bytes = sample.payload().to_bytes();
        assert_ne!(
            bytes.first(),
            Some(&b'{'),
            "a seed reply serialized as JSON under a CBOR reporter — the seed \
             must ride the same format as the live samples on the key (#830)"
        );
        let got: Alert = decode_auto(&bytes).expect("decode seed as cbor");
        assert_eq!(got.state, AlertState::Firing);
        seen += 1;
    }
    assert_eq!(seen, 1, "exactly the one firing alert");

    seed.abort();
}

/// The alert seed's replies carry an HLC timestamp (#782).
///
/// # Why this test exists, and why it is here rather than in a doc
///
/// `serve_alerts_query` answers a plain GET on `state/<producer>/alert/*`
/// storage-shaped — one reply per firing alert on its concrete state key — so
/// a late-joining consumer can seed without a router storage in the picture.
/// RFC 04 §3.2 requires that consumer to merge seed replies with live samples
/// **by HLC timestamp**, and closes with the corollary that an untimestamped
/// sample cannot be reconciled.
///
/// Zenoh's session HLC stamps a `put`. It does **not** stamp a queryable
/// reply. So every alert this seed ever served was unorderable against its own
/// successors — silently, because nothing in the workspace read
/// `Sample::timestamp()`.
///
/// It was not silent to `zenctl doctor --deep`, which reported it as
/// `unstamped-state` at warning severity — and, because
/// `scripts/conformance-verify.sh` gates on warnings, it turned CI's
/// `conformance` job into a coin flip: green when the runner tripped no sysinfo
/// threshold, red when it tripped two. This test is what keeps that from coming
/// back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_alert_seed_replies_are_stamped() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher,
        Protocol::Netlink,
        Format::Json,
    ));

    let source = unique_source();
    reporter
        .observe(sample_alert(&source), Some(Duration::ZERO))
        .await
        .expect("observe");
    assert_eq!(reporter.active_count(), 1, "one alert must be firing");

    let selector = format!(
        "{}/*",
        reporter.publisher().v1().const_state_key(&["alert"])
    );
    let seed = tokio::spawn(zensight_sensor_core::serve_alerts_query(reporter.clone()));
    // The queryable is declared inside the task; give it a moment to land
    // before the GET, or the GET matches nothing and proves nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let replies = session
        .get(&selector)
        .timeout(Duration::from_secs(5))
        .await
        .expect("seed get");

    let mut seen = 0;
    while let Ok(reply) = replies.recv_async().await {
        let sample = reply.result().expect("seed reply is a value, not an error");
        assert!(
            sample.timestamp().is_some(),
            "a state-class seed reply on {} carries no HLC timestamp — it cannot be \
             LWW-ordered against a live sample (RFC 04 §3.2, #782)",
            sample.key_expr()
        );
        let got: Alert = decode_auto(&sample.payload().to_bytes()).expect("decode seed");
        assert_eq!(got.state, AlertState::Firing);
        seen += 1;
    }
    assert_eq!(seen, 1, "exactly the one firing alert");

    seed.abort();
}

/// The seed's stamp is drawn with the snapshot, not per reply (#782).
///
/// Stated as the observable ordering property, because "the stamp is taken
/// inside the critical section" is not directly assertable: a seed batch must
/// not be able to out-stamp a `put` that happened after the snapshot was taken.
/// If it could, an alert that fires mid-loop would have its live `put` stamped
/// `T`, the loop would reply the stale snapshot value stamped `T' > T`, and LWW
/// would keep the stale one — a resurrection bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seed_batch_never_out_stamps_a_later_put() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher,
        Protocol::Netlink,
        Format::Json,
    ));

    let source = unique_source();
    reporter
        .observe(sample_alert(&source), Some(Duration::ZERO))
        .await
        .expect("observe");

    let selector = format!(
        "{}/*",
        reporter.publisher().v1().const_state_key(&["alert"])
    );
    let seed = tokio::spawn(zensight_sensor_core::serve_alerts_query(reporter.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let replies = session
        .get(&selector)
        .timeout(Duration::from_secs(5))
        .await
        .expect("seed get");
    let mut seed_stamps = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        let sample = reply.result().expect("value reply");
        seed_stamps.push(*sample.timestamp().expect("stamped"));
    }
    assert!(!seed_stamps.is_empty());

    // Anything the same session stamps AFTER the seed batch must sort after
    // every reply in it. Same HLC, so this is a total order, not a race.
    let later = zensight_common::served::seed_stamp(&session);
    for stamp in &seed_stamps {
        assert!(
            *stamp < later,
            "a seed reply stamped {stamp} sorts at or after a later stamp {later}; \
             a batch that can out-stamp a subsequent put resurrects stale state (#782)"
        );
    }

    seed.abort();
}
