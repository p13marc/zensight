//! The `@catalog` entity seed replies carry an HLC timestamp (#782).
//!
//! # Why this test exists
//!
//! `serve_entities` answers a plain GET on `v1/@catalog/state/entity/*`
//! storage-shaped — one reply per entity on its concrete state key — so a
//! late-joining frontend can seed the catalog without a router storage in the
//! picture. RFC 04 §3.2 requires that consumer to merge seed replies with live
//! samples **by HLC timestamp**, and closes with the corollary that an
//! untimestamped sample cannot be reconciled.
//!
//! Zenoh's session HLC stamps a `put`. It does **not** stamp a queryable reply.
//! `zenctl doctor --deep` reported the consequence as `unstamped-state` against
//! a real deployment, and `scripts/conformance-verify.sh` had to hold the
//! correlator back behind `CORRELATOR=1` because of it. This test is what lets
//! it stay in the default deployment.
//!
//! Two real sessions over an explicit localhost endpoint, following
//! `zensight-sensor-core/tests/liveliness_e2e.rs`: a same-session GET would
//! also work, but a reply that crosses the wire is the thing a frontend
//! actually receives.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use zensight_common::HostEvidence;
use zensight_correlator::engine::{CorrelatorState, EvidenceMsg};

/// Scouting off so concurrent tests cannot discover each other; the two peers
/// are wired together with an explicit listen/connect endpoint instead.
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

fn listen_config(port: u16) -> zenoh::Config {
    let mut config = isolated_config();
    config
        .insert_json5("listen/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]"))
        .unwrap();
    config
}

fn connect_config(port: u16) -> zenoh::Config {
    let mut config = isolated_config();
    config
        .insert_json5("connect/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]"))
        .unwrap();
    config
}

/// A port unlikely to collide: derived from the pid and time, in the dynamic
/// range, retried by the caller if the listen fails.
fn candidate_port(attempt: u16) -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u16;
    49152
        + ((std::process::id() as u16)
            .wrapping_add(nanos)
            .wrapping_add(attempt * 131))
            % 16000
}

fn self_report(sensor: &str, source: &str, host_id: &str) -> HostEvidence {
    HostEvidence {
        sensor: sensor.into(),
        source: source.into(),
        observer: None,
        host_id: Some(host_id.into()),
        boot_id: None,
        hostname: Some(source.into()),
        fqdn: None,
        ips: vec!["10.0.0.5".into()],
        macs: vec![],
        vendor: None,
        platform: None,
        container_id: None,
        cloud: None,
        last_updated: 1000,
    }
}

/// A state with one correlated entity in it, ready to be seeded.
fn state_with_one_entity() -> zensight_correlator::engine::SharedState {
    let mut s = CorrelatorState::new(Default::default());
    s.apply(EvidenceMsg::Host {
        origin: "h-test".into(),
        ev: Box::new(self_report("sysinfo", "host1", &"ab".repeat(32))),
    });
    let ops = s.recompute(2000);
    assert!(
        !ops.is_empty(),
        "the fixture must produce an entity to seed"
    );
    Arc::new(Mutex::new(s))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_entity_seed_replies_are_stamped() {
    // The correlator peer listens; the "frontend" peer dials it and GETs.
    let (catalog, port) = {
        let mut opened = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt);
            if let Ok(s) = zenoh::open(listen_config(port)).await {
                opened = Some((Arc::new(s), port));
                break;
            }
        }
        opened.expect("open listening catalog session")
    };

    let (_tx, shutdown) = watch::channel(false);
    let seed = tokio::spawn(zensight_correlator::query::serve_entities(
        catalog.clone(),
        state_with_one_entity(),
        shutdown,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let frontend = zenoh::open(connect_config(port))
        .await
        .expect("open frontend session");
    // Peers need a moment to route to each other before a GET can match.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let replies = frontend
        .get(zensight_common::entities_query_key())
        .timeout(Duration::from_secs(5))
        .await
        .expect("entity seed get");

    let mut seen = 0;
    while let Ok(reply) = replies.recv_async().await {
        let sample = reply.result().expect("seed reply is a value, not an error");
        assert!(
            sample.timestamp().is_some(),
            "the entity seed reply on {} carries no HLC timestamp — a frontend cannot \
             LWW-order it against a live sample (RFC 04 §3.2, #782)",
            sample.key_expr()
        );
        seen += 1;
    }
    assert_eq!(seen, 1, "exactly the one entity in the fixture");

    seed.abort();
}
