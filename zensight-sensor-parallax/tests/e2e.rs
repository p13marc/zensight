//! End-to-end tests: catalogue → open → JPEG preview frames → close →
//! idle-reaper teardown, over real Zenoh sessions.
//!
//! Two peers (sensor + viewer) with scouting disabled and an explicit
//! loopback endpoint (same pattern as `zensight-sensor-core`'s
//! `liveliness_e2e.rs`), so live sensors on the host cannot contaminate the
//! test. Pipelines use the synthetic test source — the identical
//! catalogue/command/session/egress path as a real camera.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::command::{Command, command_key, query_key, status_key};
use zensight_common::keyexpr::media_preview_key;
use zensight_common::stream::{FrameMeta, StreamControl, StreamDescriptor, StreamStatus};
use zensight_common::{Format, Protocol, decode};
use zensight_sensor_core::Publisher;
use zensight_sensor_parallax::catalog::Catalog;
use zensight_sensor_parallax::config::ParallaxConfig;
use zensight_sensor_parallax::session::SessionManager;
use zensight_sensor_parallax::{command, query};

/// Scouting off so concurrent tests (and live sensors on the host) can't
/// cross-contaminate; the two peers are wired together with an explicit
/// listen/connect endpoint instead.
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

/// A port unlikely to collide: derived from the pid and time, in the
/// dynamic range. Retried by the caller if the listen fails.
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

async fn isolated_pair() -> (Arc<zenoh::Session>, zenoh::Session) {
    let (sensor, port) = {
        let mut opened = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt);
            if let Ok(s) = zenoh::open(listen_config(port)).await {
                opened = Some((Arc::new(s), port));
                break;
            }
        }
        opened.expect("open listening sensor session")
    };
    let viewer = zenoh::open(connect_config(port))
        .await
        .expect("open connecting viewer session");
    (sensor, viewer)
}

/// One test-pattern stream at 160x120, preview at 4 fps, 1 s idle timeout.
fn test_parallax_config() -> ParallaxConfig {
    json5::from_str(
        r#"{
            enumerate_v4l2: false,
            test_sources: [
                { name: "test0", pattern: "smpte", width: 160, height: 120, fps: 8 },
            ],
            preview: { fps: 4, quality: 70 },
            idle_timeout_secs: 1,
        }"#,
    )
    .unwrap()
}

/// Wire the sensor side up exactly like `main.rs`: catalogue + actor +
/// command loop + streams queryable.
async fn spawn_sensor(
    session: Arc<zenoh::Session>,
    source: &str,
) -> zensight_sensor_parallax::session::SessionHandle {
    let config = test_parallax_config();
    let catalog = Arc::new(Catalog::build(&config));
    let publisher = Publisher::new(session.clone(), config.key_prefix.clone(), Format::Json);
    let host_prefix = format!("{}/{}", config.key_prefix, source);
    let handle = SessionManager::spawn(catalog.clone(), config, source.to_string(), publisher);
    tokio::spawn(command::run(
        session.clone(),
        host_prefix.clone(),
        handle.clone(),
    ));
    tokio::spawn(query::run(session, host_prefix, catalog, handle.clone()));
    // Let the subscriber + queryables propagate.
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle
}

async fn query_catalogue(viewer: &zenoh::Session, host_prefix: &str) -> Vec<StreamDescriptor> {
    let replies = viewer
        .get(query_key(host_prefix, "streams"))
        .await
        .expect("query streams");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("catalogue query timed out")
        .expect("catalogue reply channel closed");
    let sample = reply.result().expect("catalogue reply is an error");
    serde_json::from_slice(&sample.payload().to_bytes()).expect("decode catalogue")
}

async fn send_control(viewer: &zenoh::Session, host_prefix: &str, control: StreamControl) {
    let cmd = Command::new(control);
    viewer
        .put(
            command_key(host_prefix, "stream"),
            serde_json::to_vec(&cmd).unwrap(),
        )
        .await
        .expect("send stream control");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalogue_query_lists_test_stream() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-catalogue";
    let host_prefix = format!("zensight/parallax/{source}");
    let _handle = spawn_sensor(sensor.clone(), source).await;

    let got = query_catalogue(&viewer, &host_prefix).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].stream, "test0");
    assert_eq!(got[0].codecs, vec!["h264", "mjpeg"]);
    assert!(!got[0].active, "nothing has been opened yet");
    assert_eq!(
        got[0].description.as_deref(),
        Some("test pattern smpte 160x120@8")
    );

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_preview_streams_jpeg_frames_at_config_fps() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-preview";
    let host_prefix = format!("zensight/parallax/{source}");
    let handle = spawn_sensor(sensor.clone(), source).await;

    // Subscribe FIRST so the very first published frame is observed.
    let preview_key = media_preview_key(Protocol::Parallax, source, "test0");
    let sub = viewer
        .declare_subscriber(&preview_key)
        .await
        .expect("declare preview subscriber");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "test0".into(),
            codec: Some("mjpeg".into()),
            max_height: None,
        },
    )
    .await;

    // Collect frames; the preview runs at 4 fps so 6 frames ≈ 1.5 s.
    let mut arrivals: Vec<Instant> = Vec::new();
    let mut sequences: Vec<u64> = Vec::new();
    while arrivals.len() < 6 {
        let sample = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("preview frame timed out")
            .expect("preview subscriber closed");
        assert!(
            sample.encoding().to_string().starts_with("image/jpeg"),
            "preview must carry image/jpeg, got {}",
            sample.encoding()
        );
        let payload = sample.payload().to_bytes();
        assert_eq!(&payload[..2], &[0xFF, 0xD8], "JPEG must start with SOI");
        let att = sample.attachment().expect("frame has FrameMeta attachment");
        let meta: FrameMeta = decode(&att.to_bytes(), Format::Cbor).expect("decode FrameMeta");
        assert!(meta.keyframe, "every JPEG preview frame is a keyframe");
        assert_eq!((meta.width, meta.height), (160, 120));
        arrivals.push(Instant::now());
        sequences.push(meta.sequence);
    }

    // Sequence numbers are strictly monotonic.
    assert!(
        sequences.windows(2).all(|w| w[1] > w[0]),
        "sequences {sequences:?}"
    );

    // Frame cadence ≈ preview fps (4 fps → 250 ms nominal; be generous).
    let spans: Vec<Duration> = arrivals.windows(2).map(|w| w[1] - w[0]).collect();
    let avg = spans.iter().sum::<Duration>() / spans.len() as u32;
    assert!(
        avg >= Duration::from_millis(125) && avg <= Duration::from_millis(750),
        "average inter-frame gap {avg:?} not ≈ 250 ms"
    );

    // The catalogue now reports the stream active.
    let got = query_catalogue(&viewer, &host_prefix).await;
    assert!(got[0].active, "stream must be active while open");

    // The status queryable reports the open session.
    let replies = viewer
        .get(status_key(&host_prefix, "streams"))
        .await
        .expect("query status");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("status query timed out")
        .expect("status reply channel closed");
    let statuses: Vec<StreamStatus> =
        serde_json::from_slice(&reply.result().unwrap().payload().to_bytes()).unwrap();
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].open);
    assert_eq!(statuses[0].profile.as_deref(), Some("mjpeg"));

    // Tear the stream down before the runtime drops: a live pipeline keeps a
    // blocking source task alive, and tokio's shutdown would wait forever.
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
        },
    )
    .await;
    drop(sub);
    wait_until_closed(&handle).await;

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

/// Poll the actor until no stream is open (bounded).
async fn wait_until_closed(handle: &zensight_sensor_parallax::session::SessionHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if handle.open_streams().await.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "stream still open after close + idle timeout"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_and_idle_reaper_tear_stream_down() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-teardown";
    let host_prefix = format!("zensight/parallax/{source}");
    let handle = spawn_sensor(sensor.clone(), source).await;

    let preview_key = media_preview_key(Protocol::Parallax, source, "test0");
    let sub = viewer
        .declare_subscriber(&preview_key)
        .await
        .expect("declare preview subscriber");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "test0".into(),
            codec: Some("mjpeg".into()),
            max_height: None,
        },
    )
    .await;

    // Stream is up: at least one frame arrives.
    let _ = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("first preview frame timed out")
        .expect("preview subscriber closed");
    assert_eq!(
        handle.open_streams().await,
        HashSet::from(["test0".to_string()])
    );

    // Close + drop the subscriber (falling viewer edge). The idle reaper
    // (idle_timeout_secs: 1) must tear the profile down shortly after.
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
        },
    )
    .await;
    drop(sub);
    wait_until_closed(&handle).await;

    // And the catalogue is inactive again.
    let got = query_catalogue(&viewer, &host_prefix).await;
    assert!(!got[0].active, "stream must be inactive after teardown");

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}
