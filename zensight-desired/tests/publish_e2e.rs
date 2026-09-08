//! The property this daemon lives or dies by: **a pass with unchanged inputs
//! publishes nothing** (#938).
//!
//! Against a real Zenoh session, because the claim is about what crosses the
//! wire. A unit test over the publisher's own bookkeeping would assert that
//! the map it keeps agrees with itself.
//!
//! Two peers on an isolated loopback endpoint — scouting off, so a concurrent
//! test's bus cannot join this one and make the sample counts lie.

use std::sync::Arc;
use std::time::Duration;

use zenoh::sample::SampleKind;
use zensight_common::HostEntity;
use zensight_common::entity::MemberClaim;
use zensight_desired::compile::compile;
use zensight_desired::policy::Policy;
use zensight_desired::publish::Publisher0;

const POLICY: &str = r#"{
  classes: [
    { name: "all-hosts",
      matches: { always: true },
      docs: { "sysinfo/thresholds": { rules: [
        { name: "disk-full", metric: "disk/used_pct", op: "GreaterThan", value: 90 },
      ] } },
    },
  ],
  hosts: {},
}"#;

fn isolated() -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    // The publisher-side diff is byte equality on the payload, but the
    // reconciler downstream refuses an unstamped sample, so the test bus
    // stamps like the router does.
    c.insert_json5(
        "timestamping/enabled",
        "{router:true,peer:true,client:true}",
    )
    .unwrap();
    c
}

/// A port in the ephemeral range, varied per attempt (#1170).
///
/// Every draw is a guess: the range is shared with the runner's own outgoing
/// connections and with every other test binary `cargo test --workspace`
/// starts at the same moment. Which is why the caller retries — see
/// [`listening_session`].
fn candidate_port(attempt: u16) -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u16;
    49152
        + ((std::process::id() as u16)
            .wrapping_add(n)
            .wrapping_add(attempt.wrapping_mul(131)))
            % 16000
}

/// A listening peer on a free loopback port, and the port it got (#1170).
///
/// The five tests in this file run concurrently in one binary and used to draw
/// from one `port()` with fixed `+1`/`+2` offsets, then `expect` the open — so
/// two draws landing within the offset spread turned into
/// `Address already in use` and a red `test` job on a PR that had touched
/// neither this crate nor any port. Every sibling rig already retries
/// (`zensight-correlator/tests/*`) or probes and hands out
/// (`zensight-sensor-logs/tests/harness`); this one did neither.
async fn listening_session() -> (Arc<zenoh::Session>, u16) {
    for attempt in 0..8 {
        let p = candidate_port(attempt);
        let mut listen = isolated();
        listen
            .insert_json5("listen/endpoints", &format!("[\"tcp/127.0.0.1:{p}\"]"))
            .unwrap();
        if let Ok(s) = zenoh::open(listen).await {
            return (Arc::new(s), p);
        }
    }
    panic!("no free loopback port after 8 attempts");
}

fn entity(id: &str, host: &str) -> HostEntity {
    HostEntity {
        entity_id: format!("h_{id}"),
        aliases: Vec::new(),
        host_id: Some(host.to_string()),
        boot_id: None,
        ips: Vec::new(),
        macs: Vec::new(),
        container_ids: Vec::new(),
        origins: vec![host.to_string()],
        hostname: Some(id.to_string()),
        fqdn: None,
        names: Vec::new(),
        vendor: None,
        platform: Some("debian-13".into()),
        members: vec![MemberClaim {
            sensor: "sysinfo".into(),
            source: id.into(),
            rule: "host_id".into(),
            confidence: 1.0,
            last_seen: 0,
        }],
        status: None,
        last_updated: 0,
    }
}

/// One real host id, so the key the publisher builds is a key a sensor would
/// actually reconcile rather than a plausible-looking string.
fn host_id() -> String {
    zenkey::origin::HostId::from_machine_id(
        "0123456789abcdef0123456789abcdef",
        zensight_common::PROFILE.salt(),
    )
    .as_str()
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unchanged_pass_publishes_nothing() {
    let (controller, p) = listening_session().await;

    let mut conn = isolated();
    conn.insert_json5("connect/endpoints", &format!("[\"tcp/127.0.0.1:{p}\"]"))
        .unwrap();
    let watcher = zenoh::open(conn).await.expect("watcher session");

    // Watch everything this daemon may write.
    let sub = watcher
        .declare_subscriber("v1/@desired/state/**")
        .with(flume::unbounded())
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let policy: Policy = json5::from_str(POLICY).expect("policy parses");
    assert!(policy.validate().is_empty(), "{}", policy.validate());
    let fleet = vec![entity("web01", &host_id())];
    let compiled = compile(&policy, &fleet);
    assert_eq!(compiled.docs.len(), 1, "one host, one topic");

    let mut pubr = Publisher0::new(controller.clone(), 2);

    // Pass 1: the document is new.
    let first = pubr.apply(&compiled).await;
    assert_eq!(first.added.len(), 1, "{first:?}");
    assert!(first.changed.is_empty() && first.deleted.is_empty());

    // Pass 2 and 3 with identical inputs: nothing crosses the wire. This is
    // the restart-is-a-no-op property, and it is what stops every refresh
    // from rewriting the fleet.
    let second = pubr.apply(&compiled).await;
    assert!(second.wrote_nothing(), "second pass wrote: {second:?}");
    assert_eq!(second.unchanged, 1);
    let third = pubr.apply(&compiled).await;
    assert!(third.wrote_nothing(), "third pass wrote: {third:?}");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut puts = 0;
    let mut key = String::new();
    while let Ok(s) = sub.try_recv() {
        if s.kind() == SampleKind::Put {
            puts += 1;
            key = s.key_expr().as_str().to_string();
        }
    }
    assert_eq!(puts, 1, "three passes, one sample on the wire");
    assert!(
        key.ends_with(&format!("{}/sysinfo/thresholds", host_id())),
        "the key a sensor reconciles: {key}"
    );
}

/// **A catalog that goes quiet must never delete anything.**
///
/// This is the failure with no upper bound on its blast radius, and #902 names
/// it: documents go "only when the policy stops yielding them for a host that
/// is still in the catalog — never because the catalog stopped showing the
/// host". A failed GET empties the compiled set, so without the guard every
/// document in the fleet becomes a deletion candidate at once, and after
/// `grace × refresh` — ten minutes on the shipped defaults — the whole fleet
/// reverts to its file baselines.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_catalog_deletes_nothing_ever() {
    let (controller, _p) = listening_session().await;

    let policy: Policy = json5::from_str(POLICY).expect("policy parses");
    let present = compile(&policy, &[entity("web01", &host_id())]);
    // What a failed or slow catalog GET produces: no entities at all.
    let blind = compile(&policy, &[]);
    assert!(blind.docs.is_empty() && blind.hosts_seen.is_empty());

    let mut pubr = Publisher0::new(controller.clone(), 2);
    assert_eq!(pubr.apply(&present).await.added.len(), 1);

    for pass in 1..=6 {
        let r = pubr.apply(&blind).await;
        assert!(
            r.deleted.is_empty(),
            "pass {pass} deleted a document while the catalog showed nothing: {r:?}"
        );
    }
    // And when the catalog comes back, the document is still there and
    // unchanged — the sensor was reconciling it the whole time.
    assert!(
        pubr.apply(&present).await.wrote_nothing(),
        "the held document must not be rewritten when the catalog returns"
    );
}

/// A document the policy stops yielding **for a host that is still here** is
/// deleted — but only after the grace, and never on the first pass that misses
/// it.
///
/// The grace is the guard against the failure with no upper bound: one slow
/// catalog GET must not delete the fleet's configuration and revert every
/// sensor to its file baseline at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_is_deleted_only_after_the_grace() {
    let (controller, _p) = listening_session().await;

    let policy: Policy = json5::from_str(POLICY).expect("policy parses");
    let fleet = vec![entity("web01", &host_id())];
    let present = compile(&policy, &fleet);
    // The host is STILL IN THE CATALOG; the policy simply no longer selects
    // it. That — not a missing host — is what a deletion means.
    let empty_policy: Policy = json5::from_str("{ classes: [], hosts: {} }").expect("parses");
    let absent = compile(&empty_policy, &fleet);
    assert!(absent.docs.is_empty(), "the policy yields nothing for it");
    assert!(
        absent.hosts_seen.contains(&host_id()),
        "but the host is still there, which is what makes deletion legitimate"
    );

    let mut pubr = Publisher0::new(controller.clone(), 3);
    assert_eq!(pubr.apply(&present).await.added.len(), 1);

    // Two passes without it: held, not deleted.
    for pass in 1..=2 {
        let r = pubr.apply(&absent).await;
        assert!(
            r.deleted.is_empty(),
            "pass {pass} deleted before the grace: {r:?}"
        );
    }
    // The third crosses it.
    let r = pubr.apply(&absent).await;
    assert_eq!(r.deleted.len(), 1, "the grace expired: {r:?}");

    // And a document that comes back before the grace expires resets it.
    assert_eq!(pubr.apply(&present).await.added.len(), 1);
    assert!(pubr.apply(&absent).await.deleted.is_empty());
    assert!(
        pubr.apply(&present).await.wrote_nothing(),
        "back, unchanged"
    );
    assert!(
        pubr.apply(&absent).await.deleted.is_empty(),
        "grace restarted"
    );
}

/// The daemon's only input, read the way it reads it.
///
/// `fleet::fetch` is a GET against the catalog's state selector, answered
/// **storage-shaped** — one reply per entity, on its own concrete key. Nothing
/// else in this crate exercises that, and a compiler that cannot read the
/// fleet compiles an empty one: it would refuse nothing, publish nothing, and
/// after the grace **delete everything**. This is the path that must not fail
/// quietly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fleet_is_read_from_the_catalog_seed() {
    let (catalog, p) = listening_session().await;

    // A stand-in catalog: answers the entity selector storage-shaped.
    let selector = zensight_common::keyexpr::entities_query_key();
    let q = catalog
        .declare_queryable(&selector)
        .await
        .expect("entity queryable");
    let host = host_id();
    let entities = [entity("web01", &host), entity("web02", &host)];
    // The same entity_id twice with different timestamps — two catalogs
    // mid-handover both answering. The newer must win, and there must be one.
    let mut stale = entities[0].clone();
    stale.hostname = Some("stale".into());
    stale.last_updated = 1;
    let mut fresh = entities[0].clone();
    fresh.hostname = Some("fresh".into());
    fresh.last_updated = 2;
    let answers = vec![fresh, stale, entities[1].clone()];
    tokio::spawn(async move {
        while let Ok(query) = q.recv_async().await {
            for e in &answers {
                let key = format!("v1/@catalog/state/entity/{}", e.entity_id);
                let payload = serde_json::to_vec(e).expect("encode");
                let _ = query.reply(key, payload).await;
            }
        }
    });

    let mut conn = isolated();
    conn.insert_json5("connect/endpoints", &format!("[\"tcp/127.0.0.1:{p}\"]"))
        .unwrap();
    let controller = Arc::new(zenoh::open(conn).await.expect("controller session"));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let fleet = zensight_desired::fleet::fetch(&controller, Duration::from_secs(3)).await;
    assert_eq!(fleet.len(), 2, "two entities, deduped by id: {fleet:?}");
    let web01 = fleet
        .iter()
        .find(|e| e.entity_id == "h_web01")
        .expect("h_web01");
    assert_eq!(
        web01.hostname.as_deref(),
        Some("fresh"),
        "the newer answer wins a handover, deterministically"
    );

    // And the compiler turns that fleet into documents, which is the whole
    // chain this daemon is: catalog -> policy -> keys.
    let policy: Policy = json5::from_str(POLICY).expect("policy parses");
    let compiled = compile(&policy, &fleet);
    assert_eq!(compiled.docs.len(), 1, "both entities share one host_id");
    assert!(compiled.rejected.is_empty(), "{:?}", compiled.rejected);
}

/// An unreachable catalog yields an **empty** fleet, and the caller must be
/// able to tell that apart from "the fleet is empty" — because after the
/// delete grace those two mean opposite things.
///
/// `fetch` cannot make that distinction for the caller (a GET that times out
/// and a fleet of zero look identical on the wire), so this test pins the
/// behaviour it *does* guarantee: it returns, promptly, without panicking, and
/// the compiler that consumes it produces nothing rather than something wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_catalog_yields_an_empty_fleet_rather_than_a_hang() {
    let alone = Arc::new(zenoh::open(isolated()).await.expect("session"));
    let started = std::time::Instant::now();
    let fleet = zensight_desired::fleet::fetch(&alone, Duration::from_millis(500)).await;
    assert!(fleet.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "fetch must bound itself by its timeout, not by zenoh's default"
    );
}
