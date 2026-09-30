//! A broadcast write is refused at the server (RFC 05 §2.1, v1.38).
//!
//! Zenoh ACL denies by inclusion, so a query on `v1/*/@rpc/<producer>/<write>`
//! walks past a deny rule however literal, and under a permissive default
//! reaches every host. The write seam is the one layer that sees the key a
//! query actually carried: unless the registry declares the procedure
//! `fanout = "allowed"`, a query that is not the queryable's own concrete key
//! is answered `error/fanout-forbidden`, recorded, and never handed to the
//! handler.
//!
//! The audit capture is the same global `zensight::audit` layer
//! `write_query_drop.rs` installs, for the same reason: the record is emitted
//! on whichever worker thread runs the seam.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

type Buffer = Arc<Mutex<Vec<String>>>;

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
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in this test binary");
        buf
    })
    .clone()
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

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Every reply to `selector`: `Ok(payload)` or `Err(error name)`.
async fn ask(session: &zenoh::Session, selector: &str) -> Vec<Result<Vec<u8>, String>> {
    let replies = session
        .get(selector)
        .timeout(Duration::from_secs(3))
        .await
        .expect("get");
    let mut out = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        out.push(match reply.result() {
            Ok(sample) => Ok(sample.payload().to_bytes().to_vec()),
            Err(err) => {
                let body: serde_json::Value =
                    serde_json::from_slice(&err.payload().to_bytes()).expect("RpcError JSON");
                Err(body["error"].as_str().unwrap_or_default().to_string())
            }
        });
    }
    out
}

/// Serve `key` through the write seam, answering every call it lets through
/// with `[1]`, and count those calls.
async fn serve_counting(
    session: &zenoh::Session,
    key: &str,
) -> (Arc<Mutex<usize>>, tokio::task::JoinHandle<()>) {
    let q = zensight_common::served::serve_write_queryable(session, key)
        .await
        .expect("declare");
    let seen = Arc::new(Mutex::new(0usize));
    let counter = seen.clone();
    let reply_key = key.to_string();
    let handle = tokio::spawn(async move {
        while let Ok(query) = q.recv_async().await {
            *counter.lock().unwrap() += 1;
            let _ = query.executed(&reply_key, vec![1u8], None).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    (seen, handle)
}

/// The rule: a wildcard query on a write the registry does not open to
/// fan-out is refused and recorded, and the handler never sees it. An
/// unclassifiable producer takes the write default — forbidden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broadcast_write_is_refused_and_recorded() {
    let buf = buffer();
    let n = nanos();
    let producer = format!("test-{n}-logs");
    let key = format!("test-{n}/@rpc/{producer}/filter/set");
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open"));
    let (seen, handler) = serve_counting(&session, &key).await;

    let replies = ask(&session, &format!("*/@rpc/{producer}/filter/set")).await;
    assert_eq!(
        replies,
        vec![Err("error/fanout-forbidden".to_string())],
        "a wildcard write is answered with the reserved refusal, once"
    );
    assert_eq!(*seen.lock().unwrap(), 0, "the handler never sees it");

    // The concrete key still reaches the handler: the rule refuses the
    // spelling, not the procedure.
    assert_eq!(ask(&session, &key).await, vec![Ok(vec![1u8])]);
    assert_eq!(*seen.lock().unwrap(), 1);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let refused: Vec<String> = buf
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.contains(&producer) && r.contains("error/fanout-forbidden"))
        .cloned()
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "the refusal is in the trail, once; for {producer} got {refused:?}"
    );
    handler.abort();
}

/// A real write with no `fanout` column is forbidden — `snmp targets/set`,
/// which the registry keeps per host because a target table is one host's
/// (the lookup agrees with the default rather than falling through to it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registered_write_defaults_to_forbidden() {
    let _ = buffer();
    let origin = format!("test-{}", nanos());
    let key = format!("{origin}/@rpc/snmp/targets/set");
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open"));
    let (seen, handler) = serve_counting(&session, &key).await;

    let replies = ask(&session, "*/@rpc/snmp/targets/set").await;
    assert_eq!(replies, vec![Err("error/fanout-forbidden".to_string())]);
    assert_eq!(*seen.lock().unwrap(), 0);
    handler.abort();
}

/// The control: `fanout = "allowed"` (an operator-console fleet push) keeps
/// answering a wildcard query through its handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_fanout_write_still_answers_a_wildcard() {
    let _ = buffer();
    let origin = format!("test-{}", nanos());
    let key = format!("{origin}/@rpc/logs/filter/set");
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open"));
    let (seen, handler) = serve_counting(&session, &key).await;

    let replies = ask(&session, "*/@rpc/logs/filter/set").await;
    assert_eq!(replies, vec![Ok(vec![1u8])]);
    assert_eq!(*seen.lock().unwrap(), 1);
    handler.abort();
}
