//! Headless end-to-end: one lone Zenoh session (scouting off, no endpoints),
//! real bus publications (declared publishers, CBOR), the real subscriber +
//! worker + `RerunSink` in record mode, and assertions on the produced `.rrd`
//! (existence, size, `RRF2` magic, decodable message stream).
//!
//! HAZARD guard: the session must never scout — a live sensor fleet on the
//! host would contaminate the recording (docs/plans/rerun/08-multi-process.md §5).

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use zensight_common::entity::{HostEntity, MemberClaim};
use zensight_rerun::config::{FilterConfig, RerunMode, RerunSinkConfig, SamplingConfig};
use zensight_rerun::demo::{self, DemoContext};
use zensight_rerun::{CounterPolicy, RerunSink, SinkWorker};

/// One isolated, lone session: scouting off, NO listen/connect endpoints.
async fn lone_session() -> Arc<zenoh::Session> {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    Arc::new(zenoh::open(config).await.expect("open lone session"))
}

fn demo_entity() -> HostEntity {
    HostEntity {
        entity_id: "h_0123456789ab".into(),
        aliases: vec![],
        host_id: Some("ab".repeat(32)),
        boot_id: None,
        ips: vec!["10.0.0.5".into()],
        macs: vec![],
        container_ids: vec![],
        hostname: Some(demo::metrics::SOURCE.into()),
        fqdn: None,
        names: vec![],
        vendor: None,
        platform: None,
        os_name: None,
        os_version: None,
        kernel: None,
        arch: None,
        members: vec![
            // Join the demo's sysinfo series onto the entity (the netlink/
            // netring series stay on the sensors/ fallback — both paths land
            // in the recording).
            MemberClaim {
                sensor: "sysinfo".into(),
                source: demo::metrics::SOURCE.into(),
                rule: "host_id".into(),
                confidence: 1.0,
                last_seen: 1,
            },
        ],
        status: Some("online".into()),
        last_updated: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn record_mode_produces_a_valid_rrd() {
    let dir = tempfile::tempdir().unwrap();
    let rrd_path = dir.path().join("e2e.rrd");

    // Sink: record mode into the tempdir.
    let sink_config = RerunSinkConfig {
        mode: RerunMode::Record,
        rrd_path: Some(rrd_path.clone()),
        recording_id: Some("e2e-test".into()),
        ..RerunSinkConfig::default()
    };
    let sink = RerunSink::new(&sink_config).expect("create record-mode sink");

    let session = lone_session().await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx_telemetry, rx_telemetry) = mpsc::channel(zensight_rerun::sink::TELEMETRY_QUEUE);
    let (tx_control, rx_control) = mpsc::channel(zensight_rerun::sink::CONTROL_QUEUE);

    let worker = SinkWorker::new(
        rx_telemetry,
        rx_control,
        Box::new(sink),
        CounterPolicy::Rate,
        SamplingConfig::default(),
    );
    let worker_task = tokio::spawn(worker.run(shutdown_rx.clone()));

    let subscriber_task = tokio::spawn(zensight_rerun::subscriber::run_with_session(
        session.clone(),
        FilterConfig::default(),
        tx_telemetry,
        tx_control,
        shutdown_rx,
    ));

    // Let the subscriber declare all four subscribers before publishing —
    // plain subscribers have no history/cache. Declarations are in-process
    // awaits on the same lone session and the entity-seed GET runs
    // concurrently (it no longer gates the drain loop), so a short grace
    // period suffices.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish through the same lone session, via the real bus contract.
    let ctx = DemoContext::over(session.clone());
    ctx.publish_entity(&demo_entity()).await.unwrap();

    // ~60 points: 10 deterministic metric ticks x 6 series (5 gauges + 1
    // counter whose first sample the rate converter absorbs).
    let base_ts = zensight_common::telemetry::current_timestamp_millis();
    for tick in 0..10 {
        for point in demo::metrics::points_for_tick(base_ts, 500, tick) {
            ctx.publish_point(&point).await.unwrap();
        }
    }

    // 2 alerts: firing then resolved, same alert_key.
    let alert = demo::events::demo_alert();
    ctx.publish_alert(&alert).await.unwrap();
    ctx.publish_alert(&alert.resolved()).await.unwrap();

    // Let everything flow subscriber -> worker -> sink.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Shut down: worker flushes the RecordingStream (flush_blocking) and
    // drops the sink, closing the file.
    shutdown_tx.send(true).unwrap();
    let sub_stats = tokio::time::timeout(Duration::from_secs(10), subscriber_task)
        .await
        .expect("subscriber shutdown timed out")
        .expect("subscriber task panicked")
        .expect("subscriber errored");
    let stats = tokio::time::timeout(Duration::from_secs(10), worker_task)
        .await
        .expect("worker shutdown timed out")
        .expect("worker task panicked");

    // Pipeline assertions.
    assert_eq!(
        sub_stats
            .telemetry_received
            .load(std::sync::atomic::Ordering::Relaxed),
        60
    );
    assert_eq!(stats.entities_published, 1, "{stats:?}");
    assert_eq!(stats.alerts_published, 2, "{stats:?}");
    // 5 gauge series x 10 ticks + 9 counter rates (first sample absorbed).
    assert_eq!(stats.metrics_published, 59, "{stats:?}");
    assert_eq!(stats.rate_absorbed, 1, "{stats:?}");
    assert_eq!(stats.sink_errors, 0, "{stats:?}");

    // File assertions: exists, non-trivial, 4-byte magic pinned in
    // docs/plans/rerun/01-capabilities.md §3.
    let len = std::fs::metadata(&rrd_path).expect("rrd file exists").len();
    assert!(len > 1024, "rrd too small: {len} bytes");

    let mut magic = [0u8; 4];
    std::fs::File::open(&rrd_path)
        .unwrap()
        .read_exact(&mut magic)
        .unwrap();
    assert_eq!(&magic, b"RRF2", "unexpected rrd magic: {magic:?}");

    // Decode pass (re_log_encoding, same version as the sink): the stream
    // must parse end-to-end and contain the expected message mix.
    let file = std::io::BufReader::new(std::fs::File::open(&rrd_path).unwrap());
    let messages: Vec<_> = re_log_encoding::rrd::DecoderApp::decode_lazy(file)
        .collect::<Result<Vec<_>, _>>()
        .expect("rrd stream must decode cleanly");
    let set_store_info = messages
        .iter()
        .filter(|m| matches!(m, re_log_types::LogMsg::SetStoreInfo(_)))
        .count();
    let arrow_msgs = messages
        .iter()
        .filter(|m| matches!(m, re_log_types::LogMsg::ArrowMsg(..)))
        .count();
    assert!(set_store_info >= 1, "missing SetStoreInfo");
    assert!(
        arrow_msgs >= 10,
        "expected a healthy chunk count, got {arrow_msgs}"
    );

    session.close().await.unwrap();
}
