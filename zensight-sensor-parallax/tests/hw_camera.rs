//! Hardware-gated end-to-end test: a REAL V4L2 camera through the full
//! catalogue → open → JPEG preview path (the pipeline that a synthetic test
//! source cannot exercise: V4l2Src negotiation, MJPG/YUYV branches, driver
//! buffer sizing).
//!
//! Ignored by default — run manually on a machine with a camera:
//!
//! ```sh
//! cargo test -p zensight-sensor-parallax --test hw_camera -- --ignored
//! ```
//!
//! Uses the same isolated scouting-off loopback pair as `e2e.rs`, so live
//! sensors/GUI on the host are never contaminated.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::command::{Command, command_key, query_key};
use zensight_common::keyexpr::media_preview_key;
use zensight_common::stream::{FrameMeta, StreamControl, StreamDescriptor};
use zensight_common::{Format, Protocol, decode};
use zensight_sensor_core::Publisher;
use zensight_sensor_parallax::catalog::Catalog;
use zensight_sensor_parallax::config::ParallaxConfig;
use zensight_sensor_parallax::session::SessionManager;
use zensight_sensor_parallax::stats::StatsRegistry;
use zensight_sensor_parallax::{command, query};

// Same isolation helpers as tests/e2e.rs (test binaries cannot share modules
// without a common-mod file; keep these in sync).
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
            let mut cfg = isolated_config();
            cfg.insert_json5("listen/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]"))
                .unwrap();
            if let Ok(s) = zenoh::open(cfg).await {
                opened = Some((Arc::new(s), port));
                break;
            }
        }
        opened.expect("open listening sensor session")
    };
    let mut cfg = isolated_config();
    cfg.insert_json5("connect/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]"))
        .unwrap();
    let viewer = zenoh::open(cfg).await.expect("open viewer session");
    (sensor, viewer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a V4L2 camera (run with -- --ignored)"]
async fn v4l2_camera_preview_delivers_real_frames() {
    let config: ParallaxConfig = json5::from_str(
        r#"{
            enumerate_v4l2: true,
            test_sources: [],
            preview: { fps: 4, quality: 70 },
            idle_timeout_secs: 30,
        }"#,
    )
    .unwrap();

    let catalog = Arc::new(Catalog::build(&config));
    let cameras: Vec<String> = catalog.entries().iter().map(|e| e.name.clone()).collect();
    assert!(
        !cameras.is_empty(),
        "no V4L2 capture device enumerated — is a camera connected?"
    );

    let (sensor, viewer) = isolated_pair().await;
    let source = "hw-camera";
    let host_prefix = format!("zensight/parallax/{source}");

    let publisher = Publisher::new(sensor.clone(), config.key_prefix.clone(), Format::Json);
    let registry = StatsRegistry::default();
    let handle = SessionManager::spawn(
        catalog.clone(),
        config,
        source.to_string(),
        publisher,
        registry,
        None,
        None,
    );
    tokio::spawn(command::run(
        sensor.clone(),
        host_prefix.clone(),
        handle.clone(),
    ));
    tokio::spawn(query::run(
        sensor.clone(),
        host_prefix.clone(),
        catalog,
        handle.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The catalogue must advertise the camera.
    let replies = viewer
        .get(query_key(&host_prefix, "streams"))
        .await
        .expect("query streams");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("catalogue query timed out")
        .expect("catalogue reply channel closed");
    let descriptors: Vec<StreamDescriptor> =
        serde_json::from_slice(&reply.result().expect("reply").payload().to_bytes())
            .expect("decode catalogue");
    let camera = descriptors
        .first()
        .expect("catalogue lists the camera")
        .stream
        .clone();

    // Subscribe first, then open — keyframe-on-subscribe covers the rest.
    let preview_key = media_preview_key(Protocol::Parallax, source, &camera);
    let sub = viewer
        .declare_subscriber(&preview_key)
        .await
        .expect("declare preview subscriber");
    viewer
        .put(
            command_key(&host_prefix, "stream"),
            serde_json::to_vec(&Command::new(StreamControl::OpenStream {
                stream: camera.clone(),
                codec: Some("mjpeg".into()),
                max_height: None,
            }))
            .unwrap(),
        )
        .await
        .expect("send OpenStream");

    // Real cameras need a moment to start streaming; 15 s is generous but
    // finite. Before the parallax-pipeline 0.1.2 arena fix this hung forever
    // (source panicked on the first frame).
    let mut frames = 0u32;
    while frames < 3 {
        let sample = tokio::time::timeout(Duration::from_secs(15), sub.recv_async())
            .await
            .expect("no camera frame within 15 s of open")
            .expect("preview subscriber closed");
        let payload = sample.payload().to_bytes();
        assert_eq!(&payload[..2], &[0xFF, 0xD8], "JPEG must start with SOI");
        let att = sample.attachment().expect("frame has FrameMeta attachment");
        let meta: FrameMeta = decode(&att.to_bytes(), Format::Cbor).expect("decode FrameMeta");
        assert!(meta.keyframe, "preview frames are keyframes");
        assert!(meta.width > 0 && meta.height > 0);
        frames += 1;
    }

    viewer
        .put(
            command_key(&host_prefix, "stream"),
            serde_json::to_vec(&Command::new(StreamControl::CloseStream { stream: camera }))
                .unwrap(),
        )
        .await
        .expect("send CloseStream");

    viewer.close().await.unwrap();
    Arc::try_unwrap(sensor).ok(); // handle/actor still hold clones; just drop ours
}
