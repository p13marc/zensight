//! The generic `<topic>/set` seam takes **either** encoding (#1148).
//!
//! `docs/data-model.md` says "every consumer decodes via `decode_auto`". The
//! read half of `serve_topic` did; the write half was `serde_json::from_slice`,
//! so a caller whose session serialises CBOR — which is this tree's *default* —
//! got `error/invalid-args` from `logs rules/set`, `netlink
//! expectations/set` and `collection/set`, and `hostspec` and `systemd`
//! `expectations/set`. A read procedure answered and the write beside it did
//! not, which reads as "the sensor is up but rejects my config".
//!
//! Against a real bus, because the bug lives in what arrives over one.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zensight_common::{Format, encode};
use zensight_sensor_core::v1;

/// Multicast scouting OFF. A default-config session joins whatever mesh it can
/// reach — including a live fleet on the same host — so a test that scouts is
/// not a test, it is a participant (RFC 09 §0.1).
fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("disable multicast scouting");
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .expect("disable gossip");
    config
}

fn unique_producer() -> String {
    // A producer *chunk*: lowercase alnum + `-` (RFC 03 §1.5).
    format!(
        "test-{}-rpc",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Rules {
    names: Vec<String>,
}

/// Stand up `serve_topic` and POST `body` to `<topic>/set`; returns Ok(()) when
/// the procedure accepted it.
async fn set(
    session: &Arc<zenoh::Session>,
    producer: &str,
    body: Vec<u8>,
) -> std::result::Result<(), String> {
    let key = format!(
        "{}/set",
        zensight_common::command::query_key(producer, "rules")
    );
    let replies = session
        .get(&key)
        .payload(body)
        .timeout(Duration::from_secs(5))
        .await
        .map_err(|e| format!("get failed: {e}"))?;
    let reply = replies
        .recv_async()
        .await
        .map_err(|e| format!("no reply: {e}"))?;
    match reply.result() {
        Ok(_) => Ok(()),
        Err(e) => Err(String::from_utf8_lossy(&e.payload().to_bytes()).to_string()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_body_is_accepted_in_either_encoding() {
    let session = Arc::new(
        zenoh::open(isolated_config())
            .await
            .expect("open zenoh session"),
    );
    let producer = unique_producer();
    let ctx = v1::for_producer(&producer);

    let applied = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Rules>::new()));

    let apply_count = applied.clone();
    let apply_seen = seen.clone();
    let _tasks = zensight_sensor_core::rpc::serve_topic::<Rules, _, _, _, _>(
        session.clone(),
        &ctx,
        "rules",
        move |cfg: Rules| {
            let count = apply_count.clone();
            let seen = apply_seen.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push(cfg);
                Ok(())
            }
        },
        || async { Ok(Vec::new()) },
    )
    .await
    .expect("serve the topic");

    tokio::time::sleep(Duration::from_millis(300)).await;

    let json_rules = Rules {
        names: vec!["from-json".to_string()],
    };
    let cbor_rules = Rules {
        names: vec!["from-cbor".to_string()],
    };

    set(
        &session,
        &producer,
        encode(&json_rules, Format::Json).unwrap(),
    )
    .await
    .expect("a JSON body was always accepted");

    // The regression. Before #1148 this came back `error/invalid-args`, and
    // CBOR is the *default* every session in this tree publishes with.
    set(
        &session,
        &producer,
        encode(&cbor_rules, Format::Cbor).unwrap(),
    )
    .await
    .expect("a CBOR body must be accepted too");

    assert_eq!(applied.load(Ordering::SeqCst), 2, "both bodies applied");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![json_rules, cbor_rules],
        "and both decoded to what was sent, in order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_is_neither_is_still_refused() {
    // The tolerance must not become "anything goes": a handler that accepted
    // junk would apply a default rule set and report success.
    let session = Arc::new(
        zenoh::open(isolated_config())
            .await
            .expect("open zenoh session"),
    );
    let producer = unique_producer();
    let ctx = v1::for_producer(&producer);

    let applied = Arc::new(AtomicUsize::new(0));
    let apply_count = applied.clone();
    let _tasks = zensight_sensor_core::rpc::serve_topic::<Rules, _, _, _, _>(
        session.clone(),
        &ctx,
        "rules",
        move |_cfg: Rules| {
            let count = apply_count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
        || async { Ok(Vec::new()) },
    )
    .await
    .expect("serve the topic");

    tokio::time::sleep(Duration::from_millis(300)).await;

    let err = set(&session, &producer, b"not a config at all".to_vec())
        .await
        .expect_err("junk must be refused");
    assert!(err.contains("bad request body"), "{err}");
    assert_eq!(applied.load(Ordering::SeqCst), 0, "nothing was applied");
}
