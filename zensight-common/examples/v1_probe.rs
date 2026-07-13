//! Debug probe for the v1 keyspace (RFC 09 §5 etiquette, epic #453).
//!
//! Opens an ISOLATED listening session (scouting off — it can never join a
//! mesh beyond the sensors explicitly connecting to it), watches the bus for
//! a few seconds, then exercises the @rpc plane. Exits non-zero when the
//! retired legacy bus (`zensight/**`) carries anything or the v1 bus stays
//! silent.
//!
//! ```bash
//! PROBE_LISTEN=tcp/127.0.0.1:17471 PROBE_SECS=10 \
//!     cargo run -p zensight-common --example v1_probe
//! ```
//!
//! Point sensors at it with
//! `ZENSIGHT_ZENOH_CONNECT=tcp/127.0.0.1:17471 ZENSIGHT_ZENOH_SCOUTING=false`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let listen = std::env::var("PROBE_LISTEN").unwrap_or_else(|_| "tcp/127.0.0.1:17471".into());
    let secs: u64 = std::env::var("PROBE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let mut config = zenoh::Config::default();
    // Multicast OFF: the probe can never join a mesh beyond the sensors that
    // explicitly connect to it. Gossip stays ON — the probe is the hub, and
    // gossip is what lets its spokes (sensors ↔ correlator) find each other.
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("listen/endpoints", &format!("[\"{listen}\"]"))
        .unwrap();
    config.insert_json5("timestamping/enabled", "true").unwrap();
    let session = zenoh::open(config).await.expect("open probe session");
    eprintln!("probe listening on {listen} for {secs}s (multicast scouting off)");

    let legacy: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let v1: Arc<Mutex<BTreeMap<String, u64>>> = Arc::new(Mutex::new(BTreeMap::new()));

    let legacy_log = legacy.clone();
    let _legacy_sub = session
        .declare_subscriber("zensight/**")
        .callback(move |s| {
            legacy_log
                .lock()
                .unwrap()
                .push(s.key_expr().as_str().to_string());
        })
        .await
        .expect("legacy subscriber");

    let v1_log = v1.clone();
    let _v1_sub = session
        .declare_subscriber("zensight/@v1/**")
        .callback(move |s| {
            let key = s.key_expr().as_str();
            // producer = chunk 5 for data classes, chunk 4 origin for planes.
            let chunks: Vec<&str> = key.split('/').collect();
            let bucket = if chunks.len() > 4 {
                format!("{}/{}", chunks[3], chunks[4])
            } else {
                key.to_string()
            };
            *v1_log.lock().unwrap().entry(bucket).or_default() += 1;
        })
        .await
        .expect("v1 subscriber");

    tokio::time::sleep(Duration::from_secs(secs)).await;

    let v1_counts = v1.lock().unwrap().clone();
    let legacy_keys = legacy.lock().unwrap().clone();
    println!("== bus traffic ({secs}s) ==");
    for (bucket, n) in &v1_counts {
        println!("  {bucket}: {n}");
    }
    let producers: BTreeSet<String> = v1_counts
        .keys()
        .filter_map(|b| b.split('/').nth(1).map(str::to_string))
        .collect();

    // @rpc round-trips against whatever producers are live.
    println!("== @rpc probes ==");
    let mut rpc_fail = false;
    let probe = |key: String| {
        let session = session.clone();
        async move {
            let ok = match session
                .get(&key)
                .target(zenoh::query::QueryTarget::All)
                .timeout(Duration::from_secs(5))
                .await
            {
                Ok(replies) => {
                    let mut n = 0;
                    while let Ok(reply) = replies.recv_async().await {
                        if reply.result().is_ok() {
                            n += 1;
                        }
                    }
                    n
                }
                Err(_) => 0,
            };
            println!("  {key} -> {ok} repl{}", if ok == 1 { "y" } else { "ies" });
            ok
        }
    };
    for producer in &producers {
        if producer.starts_with('@') {
            continue;
        }
        let n = probe(format!("zensight/@v1/*/@rpc/{producer}/introspect")).await;
        if n == 0 {
            eprintln!("  !! no introspect reply from {producer}");
            rpc_fail = true;
        }
    }
    if producers.contains("logs") {
        probe("zensight/@v1/*/@rpc/logs/events?max=5".into()).await;
    }
    if producers.contains("systemd") {
        probe("zensight/@v1/*/@rpc/systemd/units".into()).await;
    }
    if producers.contains("parallax") {
        probe("zensight/@v1/*/@rpc/parallax/streams".into()).await;
    }
    probe("zensight/@v1/*/state/*/alert/*".into()).await;
    probe("zensight/@v1/@catalog/state/entity/*".into()).await;

    println!("== verdict ==");
    let mut fail = rpc_fail;
    if !legacy_keys.is_empty() {
        println!("FAIL: legacy bus carried {} samples:", legacy_keys.len());
        for k in legacy_keys.iter().take(10) {
            println!("  {k}");
        }
        fail = true;
    } else {
        println!("legacy bus silent ✓");
    }
    if v1_counts.is_empty() {
        println!("FAIL: no v1 traffic observed");
        fail = true;
    } else {
        println!(
            "v1 traffic from producers: {} ✓",
            producers.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    std::process::exit(if fail { 1 } else { 0 });
}
