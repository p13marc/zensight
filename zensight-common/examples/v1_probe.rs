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

use zensight_common::{HealthSnapshot, StreamDescriptor, decode_auto};

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

    // Anything on the deployment root that is NOT under v1 is a leak. The
    // "not v1" test lives in the callback because the version chunk is plain:
    // `**` reaches it, so the selector alone can no longer exclude our own
    // traffic (see `zenkey::grammar::VERSION_CHUNK`).
    let legacy_log = legacy.clone();
    let _legacy_sub = session
        .declare_subscriber("zensight/**")
        .callback(move |s| {
            let key = s.key_expr().as_str();
            if !key.starts_with("zensight/v1/") {
                legacy_log.lock().unwrap().push(key.to_string());
            }
        })
        .await
        .expect("legacy subscriber");

    // Registry conformance (RFC 08 §5, issue #468): every telemetry key that
    // actually reaches the bus must be buildable from a registry entry. The
    // in-process guard (`zensight_common::metric_guard`) catches this at the
    // publisher, but only for metrics a given run happens to emit; this is the
    // end-to-end half — whatever the sensor *really* published on *this* host.
    let unregistered: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let telemetry_seen: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));

    let v1_log = v1.clone();
    let unreg_log = unregistered.clone();
    let seen_log = telemetry_seen.clone();
    let _v1_sub = session
        .declare_subscriber("zensight/v1/**")
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

            // The probe is deliberately UN-namespaced (RFC 09 §5: a debug tool
            // spells full keys, because the honest view of the wire is the whole
            // point of it), so it strips the base explicitly rather than hunting
            // for "v1/" with a substring search — which would also match a base
            // that happened to contain it (#466).
            if let Some(parsed) =
                zensight_common::keyexpr::parse_full_key(zensight_common::DEFAULT_BASE, key)
                && matches!(
                    parsed.class,
                    zenkey::grammar::ClassOrPlane::Class(zenkey::grammar::Class::Telemetry)
                )
                && let Some(producer) = parsed.producer.as_ref()
            {
                let tail: &[&str] = &parsed.subject;
                seen_log
                    .lock()
                    .unwrap()
                    .insert(format!("{}/{}", producer.name(), tail.join("/")));
                if zensight_common::registry::parse_subject(
                    producer.name(),
                    zenkey::grammar::Class::Telemetry,
                    tail,
                )
                .is_none()
                {
                    unreg_log.lock().unwrap().insert(format!(
                        "{}: {}",
                        producer.name(),
                        tail.join("/")
                    ));
                }
            }
        })
        .await
        .expect("v1 subscriber");

    // GUI-shaped origin map: the GUI learns source→origin from the health
    // docs' host_id (== the v1 origin id) and keys its drill-down fetches on
    // it — this phase replays that exact path.
    let origins: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let origins_log = origins.clone();
    let _health_sub = session
        .declare_subscriber("zensight/v1/*/state/*/health")
        .callback(move |s| {
            if let Ok(snapshot) = decode_auto::<HealthSnapshot>(&s.payload().to_bytes())
                && let (Some(hid), Some(src)) = (snapshot.host_id, snapshot.source)
            {
                origins_log.lock().unwrap().insert(src, hid);
            }
        })
        .await
        .expect("health subscriber");

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

    // == Registry conformance: is every telemetry subject on the wire registered?
    let seen = telemetry_seen.lock().unwrap().clone();
    let unreg = unregistered.lock().unwrap().clone();
    println!(
        "== registry conformance ==\n  {} distinct telemetry subjects observed",
        seen.len()
    );
    let registry_fail = !unreg.is_empty();
    if registry_fail {
        println!("  FAIL: {} unregistered (RFC 08 §5):", unreg.len());
        for u in &unreg {
            println!("    !! {u}");
        }
    } else if seen.is_empty() {
        println!("  (no telemetry observed — nothing to check)");
    } else {
        println!("  all registered");
    }

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
        let n = probe(format!("zensight/v1/*/@rpc/{producer}/introspect")).await;
        if n == 0 {
            eprintln!("  !! no introspect reply from {producer}");
            rpc_fail = true;
        }
    }
    if producers.contains("logs") {
        probe("zensight/v1/*/@rpc/logs/events?max=5".into()).await;
    }
    if producers.contains("systemd") {
        probe("zensight/v1/*/@rpc/systemd/units".into()).await;
    }
    if producers.contains("parallax") {
        probe("zensight/v1/*/@rpc/parallax/streams".into()).await;
    }
    probe("zensight/v1/*/state/*/alert/*".into()).await;
    probe("zensight/v1/@catalog/state/entity/*".into()).await;

    // == GUI-shaped drill-down phase: origin-scoped GETs, exactly what the
    // device detail tabs send (the fleet probes above cannot catch a broken
    // origin path — that is how the origin-map bug shipped).
    println!("== GUI-shaped drill-downs (origin-scoped) ==");
    let origin_map = origins.lock().unwrap().clone();
    let mut gui_fail = false;
    if origin_map.is_empty() {
        println!("FAIL: no source→origin mapping learned from health docs");
        gui_fail = true;
    }
    for (source, origin) in &origin_map {
        println!("  {source} -> {origin}");
        let drill =
            |producer: &str, tail: &str| format!("zensight/v1/{origin}/@rpc/{producer}/{tail}");
        let mut want = Vec::new();
        if producers.contains("sysinfo") {
            want.push(drill("sysinfo", "processes?sort=cpu&top=5"));
        }
        if producers.contains("netlink") {
            want.push(drill("netlink", "sockets"));
        }
        if producers.contains("systemd") {
            want.push(drill("systemd", "units"));
        }
        if producers.contains("netring") {
            want.push(drill("netring", "flows"));
        }
        if producers.contains("parallax") {
            want.push(drill("parallax", "streams"));
        }
        for key in want {
            if probe(key.clone()).await == 0 {
                eprintln!("  !! origin-scoped drill-down got no reply: {key}");
                gui_fail = true;
            }
        }

        // Parallax end-to-end: open a stream via the origin-scoped write
        // procedure, expect a JPEG preview frame on the concrete media key,
        // then close it — the full GUI tile lifecycle.
        if producers.contains("parallax") {
            match parallax_tile_roundtrip(&session, origin).await {
                Ok(stream) => println!("  parallax tile round-trip on '{stream}' ✓"),
                Err(e) => {
                    println!("FAIL: parallax tile round-trip: {e}");
                    gui_fail = true;
                }
            }
        }
    }

    println!("== verdict ==");
    let mut fail = rpc_fail || gui_fail || registry_fail;
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

/// Open the first advertised parallax stream (mjpeg), await one preview frame
/// on the concrete `@media` key, close the stream. Mirrors the GUI tile.
async fn parallax_tile_roundtrip(session: &zenoh::Session, origin: &str) -> Result<String, String> {
    let streams_key = format!("zensight/v1/{origin}/@rpc/parallax/streams");
    let replies = session
        .get(&streams_key)
        .timeout(Duration::from_secs(5))
        .await
        .map_err(|e| format!("streams get: {e}"))?;
    let reply = replies
        .recv_async()
        .await
        .map_err(|_| "no streams reply".to_string())?;
    let sample = reply.result().map_err(|e| format!("streams err: {e:?}"))?;
    let catalogue: Vec<StreamDescriptor> = serde_json::from_slice(&sample.payload().to_bytes())
        .map_err(|e| format!("streams decode: {e}"))?;
    let stream = catalogue
        .first()
        .map(|d| d.stream.clone())
        .ok_or("empty stream catalogue")?;

    let preview_key = format!("zensight/v1/{origin}/@media/parallax/{stream}/preview/jpeg");
    let sub = session
        .declare_subscriber(&preview_key)
        .await
        .map_err(|e| format!("preview subscribe: {e}"))?;

    let open = zensight_common::command::Command::new(zensight_common::StreamControl::OpenStream {
        stream: stream.clone(),
        codec: Some("mjpeg".into()),
        tier: None,
    });
    let set_key = format!("zensight/v1/{origin}/@rpc/parallax/stream/set");
    let body = serde_json::to_vec(&open).map_err(|e| e.to_string())?;
    let replies = session
        .get(&set_key)
        .payload(body)
        .timeout(Duration::from_secs(5))
        .await
        .map_err(|e| format!("open get: {e}"))?;
    let reply = replies
        .recv_async()
        .await
        .map_err(|_| "no open reply".to_string())?;
    reply.result().map_err(|e| format!("open refused: {e:?}"))?;

    let frame = tokio::time::timeout(Duration::from_secs(15), sub.recv_async())
        .await
        .map_err(|_| "no preview frame within 15s".to_string())?
        .map_err(|e| format!("preview recv: {e}"))?;
    let n = frame.payload().to_bytes().len();
    if n == 0 {
        return Err("empty preview frame".into());
    }

    let close =
        zensight_common::command::Command::new(zensight_common::StreamControl::CloseStream {
            stream: stream.clone(),
            codec: Some("mjpeg".into()),
            tier: None,
        });
    let body = serde_json::to_vec(&close).map_err(|e| e.to_string())?;
    if let Ok(replies) = session
        .get(&set_key)
        .payload(body)
        .timeout(Duration::from_secs(5))
        .await
    {
        let _ = replies.recv_async().await;
    }
    Ok(stream)
}
