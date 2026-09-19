//! A dropped write call still leaves an audit record (#1156).
//!
//! `WriteQuery`'s three answer methods — `executed`, `executed_but`, `refused`
//! — were built so that every call on a write procedure leaves a record. They
//! all consume the value, which made **dropping** it a fourth path that
//! spelled nothing: no record, no reply. An early `return`, a `?` on an
//! unrelated error, or a `match` arm that falls through was enough, and the
//! trail said the call never happened.
//!
//! The record is captured off the `zensight::audit` tracing target, which is
//! where `audit::record` writes when the `linux-audit` feature is off — the
//! default, and what this test builds with.
//!
//! The capture is installed **globally, once**: the record is emitted on
//! whichever tokio worker thread runs the handler, so a thread-local default
//! would not see it. Both tests share the buffer and filter by their own
//! unique procedure name.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

type Buffer = Arc<Mutex<Vec<String>>>;

/// Collects every event on the `zensight::audit` target.
#[derive(Clone)]
struct Capture(Buffer);

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "zensight::audit" {
            return;
        }
        struct V(String);
        impl tracing::field::Visit for V {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={:?} ", f.name(), v));
            }
        }
        let mut v = V(String::new());
        event.record(&mut v);
        self.0.lock().unwrap().push(v.0);
    }
}

fn buffer() -> Buffer {
    static BUF: OnceLock<Buffer> = OnceLock::new();
    BUF.get_or_init(|| {
        let buf: Buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default().with(Capture(buf.clone()));
        // Once per test binary; a second call would fail and is not made.
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in this test binary");
        buf
    })
    .clone()
}

fn records_mentioning(buf: &Buffer, needle: &str) -> Vec<String> {
    buf.lock()
        .unwrap()
        .iter()
        .filter(|r| r.contains(needle))
        .cloned()
        .collect()
}

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

/// A real v1 `@rpc` write key, so the seam classifies it the way it will in
/// production. `test-<nanos>-logs` is a legal producer chunk (RFC 03 §1.5).
fn unique_key() -> (String, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let producer = format!("test-{nanos}-logs");
    (format!("test-{nanos}/@rpc/{producer}/filter/set"), producer)
}

async fn drain(session: &zenoh::Session, key: &str) -> usize {
    let replies = session
        .get(key)
        .timeout(Duration::from_secs(3))
        .await
        .expect("get");
    let mut n = 0;
    while replies.recv_async().await.is_ok() {
        n += 1;
    }
    n
}

/// The regression: a handler that drops the call leaves a record saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_write_call_is_recorded() {
    let buf = buffer();
    let (key, producer) = unique_key();
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open"));

    let q = zensight_common::served::serve_write_queryable(&session, &key)
        .await
        .expect("declare");

    let handler = tokio::spawn(async move {
        // The bug: receive the call and do nothing with it.
        if let Ok(query) = q.recv_async().await {
            drop(query);
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let replies = drain(&session, &key).await;
    let _ = handler.await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(replies, 0, "a dropped call sends no reply — unchanged");

    let mine = records_mentioning(&buf, &producer);
    assert!(
        mine.iter().any(|r| r.contains("handler dropped the call")),
        "a dropped write call must leave an audit record; for {producer} got {mine:?}"
    );
}

/// The control: a call answered properly leaves its own record and **not** the
/// drop one, so the `Drop` impl does not double-count every ordinary answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answered_write_call_is_not_recorded_as_dropped() {
    let buf = buffer();
    let (key, producer) = unique_key();
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open"));

    let q = zensight_common::served::serve_write_queryable(&session, &key)
        .await
        .expect("declare");

    let reply_key = key.clone();
    let handler = tokio::spawn(async move {
        if let Ok(query) = q.recv_async().await {
            let _ = query
                .executed(&reply_key, vec![1u8], Some("a-target"))
                .await;
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let replies = drain(&session, &key).await;
    let _ = handler.await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(replies, 1, "an answered call replies once");

    let mine = records_mentioning(&buf, &producer);
    assert!(
        !mine.is_empty(),
        "the answer itself must be recorded; for {producer} got nothing"
    );
    assert!(
        !mine.iter().any(|r| r.contains("handler dropped the call")),
        "an answered call must not be recorded as dropped; for {producer} got {mine:?}"
    );
}
