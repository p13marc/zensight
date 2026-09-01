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

// ===========================================================================
// #882 — the firing set outlives the process
// ===========================================================================

/// A stand-in for a `latest` storage on `v1/*/state/**`: answers a GET on the
/// alert selector with the documents a previous incarnation left behind.
///
/// A plain `declare_queryable` is fine here and only here — the #484 guard
/// covers `zensight*/src`, and what this fakes is a router plugin, not a
/// producer surface this build claims to serve.
async fn stranded_storage(
    session: &Arc<zenoh::Session>,
    selector: &str,
    stored: Vec<(String, Vec<u8>)>,
) -> zenoh::query::Queryable<zenoh::handlers::FifoChannelHandler<zenoh::query::Query>> {
    let queryable = session
        .declare_queryable(selector)
        .await
        .expect("fake storage queryable");
    let stored = Arc::new(stored);
    let q = queryable.clone();
    tokio::spawn(async move {
        while let Ok(query) = q.recv_async().await {
            for (key, payload) in stored.iter() {
                let _ = query.reply(key.clone(), payload.clone()).await;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    queryable
}

fn concrete_alert_key(reporter: &AlertReporter, alert: &Alert) -> String {
    let selector = reporter.alert_selector();
    format!(
        "{}{}",
        selector.strip_suffix('*').expect("selector ends in *"),
        alert.alert_key()
    )
}

/// The #882 regression. A sensor fires an alert, then dies without resolving
/// it — SIGKILL, OOM, or a restart whose new config no longer defines the
/// target. The successor must not leave that document `firing` forever: it
/// adopts the claim, finds it no longer true on its first sweep, and retracts
/// it exactly as if it had raised it itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_does_not_strand_a_firing_alert() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    let alert = sample_alert(&source);
    let key = concrete_alert_key(&reporter, &alert);
    let payload = zensight_common::encode(&alert, Format::Json).expect("encode");

    let _storage = stranded_storage(
        &session,
        &reporter.alert_selector(),
        vec![(key.clone(), payload)],
    )
    .await;

    let sub = session
        .declare_subscriber(key.clone())
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(reporter.adopt_persisted(Duration::from_secs(3)).await, 1);
    assert_eq!(
        reporter.active_count(),
        1,
        "an adopted alert is firing until a sweep says otherwise"
    );

    // The successor's first sweep: the condition is not violated any more.
    reporter
        .reconcile("ssh-listening", &[])
        .await
        .expect("reconcile");

    let mut saw_resolved = false;
    let mut saw_delete = false;
    for _ in 0..2 {
        let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("recv timed out")
            .expect("recv");
        match s.kind() {
            zenoh::sample::SampleKind::Put => {
                let got: Alert = decode_auto(&s.payload().to_bytes()).expect("decode");
                assert_eq!(got.state, AlertState::Resolved);
                assert_eq!(got.rule, "ssh-listening");
                saw_resolved = true;
            }
            zenoh::sample::SampleKind::Delete => saw_delete = true,
        }
    }
    assert!(saw_resolved, "the inherited alert was never retracted");
    assert!(saw_delete, "the inherited key was never tombstoned");
    assert_eq!(reporter.active_count(), 0);
}

/// Adoption must be silent when the condition is still true: the document on
/// the bus already says exactly what this process would say, so re-observing
/// it publishes nothing. Without this, every restart of every sensor would
/// re-put its whole firing set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_alert_that_is_still_true_is_not_republished() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    let alert = sample_alert(&source);
    let key = concrete_alert_key(&reporter, &alert);
    let payload = zensight_common::encode(&alert, Format::Json).expect("encode");

    let _storage = stranded_storage(
        &session,
        &reporter.alert_selector(),
        vec![(key.clone(), payload)],
    )
    .await;

    let sub = session
        .declare_subscriber(key.clone())
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(reporter.adopt_persisted(Duration::from_secs(3)).await, 1);

    // The successor's first sweep finds the same violation.
    reporter
        .observe(alert.clone(), Some(Duration::ZERO))
        .await
        .expect("observe");
    reporter
        .reconcile("ssh-listening", &[alert.alert_key()])
        .await
        .expect("reconcile");

    assert!(
        tokio::time::timeout(Duration::from_millis(600), sub.recv_async())
            .await
            .is_err(),
        "an adopted, still-true alert must not be republished"
    );
    assert_eq!(reporter.active_count(), 1);
}

/// Two shapes no sweep can ever reach, because this build will never write
/// their key again: a `Resolved` document whose tombstone was lost, and a
/// document whose key does not match the `alert_key` its own payload derives
/// (the #737 re-key stranding). Adoption retires both on the spot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adoption_tombstones_documents_no_sweep_can_reach() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    let resolved = sample_alert(&source).resolved();
    let resolved_key = concrete_alert_key(&reporter, &resolved);
    // Same payload, a key from an older derivation — nothing this build emits
    // will ever land here again.
    let phantom = sample_alert(&source);
    let phantom_key = format!(
        "{}0000000000000000",
        reporter
            .alert_selector()
            .strip_suffix('*')
            .expect("selector ends in *")
    );

    let _storage = stranded_storage(
        &session,
        &reporter.alert_selector(),
        vec![
            (
                resolved_key.clone(),
                zensight_common::encode(&resolved, Format::Json).expect("encode"),
            ),
            (
                phantom_key.clone(),
                zensight_common::encode(&phantom, Format::Json).expect("encode"),
            ),
        ],
    )
    .await;

    let sub = session
        .declare_subscriber(reporter.alert_selector())
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        reporter.adopt_persisted(Duration::from_secs(3)).await,
        0,
        "neither document is a claim this build can still make"
    );
    assert_eq!(reporter.active_count(), 0);

    let mut tombstoned = std::collections::HashSet::new();
    for _ in 0..2 {
        let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("recv timed out")
            .expect("recv");
        assert_eq!(s.kind(), zenoh::sample::SampleKind::Delete);
        tombstoned.insert(s.key_expr().to_string());
    }
    assert!(tombstoned.contains(&resolved_key), "{tombstoned:?}");
    assert!(tombstoned.contains(&phantom_key), "{tombstoned:?}");
}

/// A clean stop retracts everything, so the graceful path leaves nothing
/// behind at all — the half `resolve_all` was written for and never wired to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_all_retracts_and_tombstones_the_whole_firing_set() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/netlink/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "netlink", Format::Json);
    let reporter = AlertReporter::new(publisher, Protocol::Netlink, Format::Json);

    let source = unique_source();
    for port in ["22", "443"] {
        let alert = sample_alert(&source).with_label("port", port);
        reporter
            .observe(alert, Some(Duration::ZERO))
            .await
            .expect("observe");
    }
    assert_eq!(reporter.active_count(), 2);
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("recv firing timed out")
            .expect("recv firing");
    }

    reporter.resolve_all().await.expect("resolve_all");
    assert_eq!(reporter.active_count(), 0);

    let (mut resolved, mut deleted) = (0, 0);
    for _ in 0..4 {
        let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("recv timed out")
            .expect("recv");
        match s.kind() {
            zenoh::sample::SampleKind::Put => {
                let got: Alert = decode_auto(&s.payload().to_bytes()).expect("decode");
                assert_eq!(got.state, AlertState::Resolved);
                resolved += 1;
            }
            zenoh::sample::SampleKind::Delete => deleted += 1,
        }
    }
    assert_eq!((resolved, deleted), (2, 2));
}

/// A rule *deleted from the build* is the one case adoption alone cannot fix:
/// the inherited alert would be adopted and then never reconciled, because no
/// sweep of that rule will ever run again. A producer that declares its rule
/// table lets adoption retire it — and a producer that declares nothing keeps
/// everything, because silence is not a licence to delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_this_build_no_longer_has_is_retired_not_adopted() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let source = unique_source();

    let gone = Alert::new(
        &source,
        Protocol::Netlink,
        AlertKind::Expectation,
        "a-rule-that-was-deleted",
        AlertSeverity::Warning,
        "raised by a build that no longer exists",
    );
    let kept = sample_alert(&source);

    let plain = AlertReporter::new(
        Publisher::new(session.clone(), "netlink", Format::Json),
        Protocol::Netlink,
        Format::Json,
    );
    let stored = vec![
        (
            concrete_alert_key(&plain, &gone),
            zensight_common::encode(&gone, Format::Json).expect("encode"),
        ),
        (
            concrete_alert_key(&plain, &kept),
            zensight_common::encode(&kept, Format::Json).expect("encode"),
        ),
    ];
    let gone_key = stored[0].0.clone();
    let _storage = stranded_storage(&session, &plain.alert_selector(), stored).await;

    // A producer that has not declared its rules keeps both.
    assert_eq!(plain.adopt_persisted(Duration::from_secs(3)).await, 2);

    let declared = AlertReporter::new(
        Publisher::new(session.clone(), "netlink", Format::Json),
        Protocol::Netlink,
        Format::Json,
    )
    .with_known_rules(["ssh-listening"]);
    let sub = session
        .declare_subscriber(declared.alert_selector())
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(declared.adopt_persisted(Duration::from_secs(3)).await, 1);
    let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("recv timed out")
        .expect("recv");
    assert_eq!(s.kind(), zenoh::sample::SampleKind::Delete);
    assert_eq!(s.key_expr().to_string(), gone_key);
}
