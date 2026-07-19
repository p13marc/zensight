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

use zensight_common::command::{Command, command_key, query_key};
use zensight_common::stream::{FrameMeta, StreamControl, StreamDescriptor, StreamStatus};
use zensight_common::{Format, decode};
use zensight_sensor_core::Publisher;
use zensight_sensor_parallax::catalog::Catalog;
use zensight_sensor_parallax::config::ParallaxConfig;
use zensight_sensor_parallax::session::SessionManager;
use zensight_sensor_parallax::stats::StatsRegistry;
use zensight_sensor_parallax::{annexb, command, query, stats};

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
/// command loop + streams queryable + a fast (1 s) stats ticker.
async fn spawn_sensor(
    session: Arc<zenoh::Session>,
    source: &str,
) -> zensight_sensor_parallax::session::SessionHandle {
    spawn_sensor_with_config(session, source, test_parallax_config())
        .await
        .0
}

/// [`spawn_sensor`] with a caller-chosen config; also hands the stats
/// registry back so tests can assert open/failure cleanup.
async fn spawn_sensor_with_config(
    session: Arc<zenoh::Session>,
    source: &str,
    config: ParallaxConfig,
) -> (
    zensight_sensor_parallax::session::SessionHandle,
    StatsRegistry,
) {
    let catalog = Arc::new(Catalog::build(&config));
    let tiers = config.video.tiers.clone();
    let publisher = Publisher::new(session.clone(), "parallax", Format::Json);
    // v1: control/query surfaces key off the producer name (the origin
    // chunk scopes per host; the source label is payload-only).
    let host_prefix = "parallax".to_string();
    let registry = StatsRegistry::default();
    tokio::spawn(stats::run_ticker(
        publisher.clone(),
        source.to_string(),
        registry.clone(),
        catalog.entries().len(),
        Duration::from_secs(1),
        None,
    ));
    let handle = SessionManager::spawn(
        catalog.clone(),
        config,
        source.to_string(),
        publisher,
        registry.clone(),
        None,
        None,
    );
    tokio::spawn(command::run(
        session.clone(),
        host_prefix.clone(),
        handle.clone(),
    ));
    tokio::spawn(query::run(
        session,
        host_prefix,
        catalog,
        tiers,
        handle.clone(),
    ));
    // Let the subscriber + queryables propagate.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (handle, registry)
}

/// The sensor runs in-process, so the test's v1 context (same global host
/// origin) yields exactly the keys the sensor publishes on (epic #453).
fn v1ctx() -> zensight_sensor_core::v1::V1Context {
    zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "parallax")
}

async fn query_catalogue(viewer: &zenoh::Session, _host_prefix: &str) -> Vec<StreamDescriptor> {
    let replies = viewer
        .get(query_key("parallax", "streams"))
        .await
        .expect("query streams");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("catalogue query timed out")
        .expect("catalogue reply channel closed");
    let sample = reply.result().expect("catalogue reply is an error");
    serde_json::from_slice(&sample.payload().to_bytes()).expect("decode catalogue")
}

/// Await the latest per-stream status document (v1: LWW state at
/// `state/parallax/stream/<stream>`, RFC 05 §5) matching `pred`.
async fn await_status(
    sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    pred: impl Fn(&StreamStatus) -> bool,
) -> StreamStatus {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sample = sub.recv_async().await.expect("status sub closed");
            if let Ok(s) = serde_json::from_slice::<StreamStatus>(&sample.payload().to_bytes())
                && pred(&s)
            {
                return s;
            }
        }
    })
    .await
    .expect("status doc timed out")
}

async fn send_control(viewer: &zenoh::Session, _host_prefix: &str, control: StreamControl) {
    // v1 (RFC 05): stream control is the `stream/set` write procedure —
    // GET with a body; the value reply is the ack.
    let cmd = Command::new(control);
    let replies = viewer
        .get(command_key("parallax", "stream"))
        .payload(serde_json::to_vec(&cmd).unwrap())
        .await
        .expect("send stream control");
    let reply = replies.recv_async().await.expect("control ack");
    assert!(reply.result().is_ok(), "control refused: {reply:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalogue_query_lists_test_stream() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-catalogue";
    let host_prefix = "parallax".to_string();
    let _handle = spawn_sensor(sensor.clone(), source).await;

    let got = query_catalogue(&viewer, &host_prefix).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].stream, "test0");
    assert_eq!(got[0].codecs, vec!["h264", "mjpeg"]);
    assert!(!got[0].active, "nothing has been opened yet");
    // Capability-bearing (#507): native geometry is a field now, not smuggled
    // through the description string.
    assert_eq!((got[0].width, got[0].height), (Some(160), Some(120)));
    assert_eq!(got[0].description.as_deref(), Some("test pattern smpte"));
    // A 120-high source can only feed the uncapped `high` tier — the 240/480
    // tiers would upscale, so the catalogue honestly omits them.
    let tier_names: Vec<&str> = got[0].tiers.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tier_names, vec!["high"]);

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_preview_streams_jpeg_frames_at_config_fps() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-preview";
    let host_prefix = "parallax".to_string();
    let handle = spawn_sensor(sensor.clone(), source).await;

    // Subscribe FIRST so the very first published frame is observed.
    let preview_key = v1ctx().media_key(&["test0", "preview", "jpeg"]);
    let sub = viewer
        .declare_subscriber(preview_key.as_keyexpr())
        .await
        .expect("declare preview subscriber");
    // Subscribe to the per-stream status doc BEFORE opening (v1 state has no
    // storage in this harness; LWW without a seed means catch-the-transition).
    let status_sub = viewer
        .declare_subscriber(zenoh::key_expr::OwnedKeyExpr::from(v1ctx().state_key(&["stream", "test0"])))
        .await
        .expect("declare status sub");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "test0".into(),
            codec: Some("mjpeg".into()),
            tier: None,
        },
    )
    .await;

    // Collect frames; the preview runs at 4 fps so 6 frames ≈ 1.5 s.
    let mut sequences: Vec<u64> = Vec::new();
    let mut pts: Vec<u64> = Vec::new();
    while sequences.len() < 6 {
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
        sequences.push(meta.sequence);
        pts.push(meta.pts_ns.expect("test-source frames carry pts"));
    }

    // Sequence numbers are strictly monotonic.
    assert!(
        sequences.windows(2).all(|w| w[1] > w[0]),
        "sequences {sequences:?}"
    );

    // Frame cadence ≈ preview fps: judge by the SOURCE's pts (stamped at the
    // configured 4 fps → 250 ms per frame), not by delivery timing — network
    // arrival bursts and loaded-machine scheduling make wall-clock gaps
    // meaningless in a parallel test run. Sequence gaps (dropped frames under
    // load) scale the expected span, so only the per-frame rate is pinned.
    let frames_spanned = sequences.last().unwrap() - sequences.first().unwrap();
    let pts_span_ns = pts.last().unwrap() - pts.first().unwrap();
    let per_frame_ms = pts_span_ns as f64 / frames_spanned as f64 / 1e6;
    assert!(
        (per_frame_ms - 250.0).abs() < 50.0,
        "per-frame pts spacing {per_frame_ms:.1} ms not ≈ 250 ms (4 fps)"
    );

    // The catalogue now reports the stream active.
    let got = query_catalogue(&viewer, &host_prefix).await;
    assert!(got[0].active, "stream must be active while open");

    // The per-stream state doc reported the open transition (v1: LWW state,
    // RFC 05 §5) — the subscriber was declared before the open.
    let status = await_status(&status_sub, |s| s.open).await;
    // A preview-only open reports `open` but carries no video tier.
    assert!(status.open);
    assert!(status.tiers.is_empty(), "preview is not a video tier");

    // Tear the stream down before the runtime drops: a live pipeline keeps a
    // blocking source task alive, and tokio's shutdown would wait forever.
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
            codec: None,
            tier: None,
        },
    )
    .await;
    drop(sub);
    wait_until_closed(&handle).await;

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_h264_video_streams_with_keyframe_control() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-h264";
    let host_prefix = "parallax".to_string();
    let handle = spawn_sensor(sensor.clone(), source).await;

    // Subscribe FIRST so the initial IDR is observed, to the EXACT default
    // tier's key (keyspace v1.3 revoked the `video/h264/*` wildcard: the tier
    // is published in the catalogue, so the viewer subscribes to it exactly).
    // The open below uses `tier: None`, which resolves to the sensor default
    // (`medium`); the matching listener must see this subscriber as a viewer
    // (asserted below via the status doc).
    let video_key = v1ctx().media_video_key("test0", "h264", "medium");
    let sub = viewer
        .declare_subscriber(video_key.as_keyexpr())
        .await
        .expect("declare video subscriber");
    let status_sub = viewer
        .declare_subscriber(zenoh::key_expr::OwnedKeyExpr::from(v1ctx().state_key(&["stream", "test0"])))
        .await
        .expect("declare status sub");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "test0".into(),
            codec: Some("h264".into()),
            tier: None,
        },
    )
    .await;

    async fn next_frame(
        sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    ) -> (FrameMeta, Vec<u8>) {
        let sample = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("video frame timed out")
            .expect("video subscriber closed");
        assert!(
            sample.encoding().to_string().starts_with("video/h264"),
            "video must carry video/h264, got {}",
            sample.encoding()
        );
        let att = sample.attachment().expect("frame has FrameMeta attachment");
        let meta: FrameMeta = decode(&att.to_bytes(), Format::Cbor).expect("decode FrameMeta");
        let payload = sample.payload().to_bytes().to_vec();
        // #435: the advertised keyframe flag must be truthful at the byte
        // level — a flag without an IDR (or vice versa) sends fresh decoders
        // into a dsNoParamSets loop.
        assert_eq!(
            meta.keyframe,
            annexb::has_idr(&payload),
            "FrameMeta.keyframe must match IDR presence (seq {})",
            meta.sequence
        );
        // #435: every published keyframe is a self-contained decoder entry
        // point — SPS/PPS ride in the same access unit.
        if meta.keyframe {
            assert!(
                annexb::has_param_sets(&payload),
                "keyframe AU without SPS/PPS (seq {})",
                meta.sequence
            );
        }
        (meta, payload)
    }

    // A keyframe must arrive within the first few access units (the sensor
    // forces an IDR at open, and openh264 starts on one anyway).
    let mut sequences: Vec<u64> = Vec::new();
    let mut first_keyframe_at = None;
    for i in 0..5 {
        let (meta, _) = next_frame(&sub).await;
        assert_eq!((meta.width, meta.height), (160, 120));
        sequences.push(meta.sequence);
        if meta.keyframe {
            first_keyframe_at = Some(i);
            break;
        }
    }
    assert!(
        first_keyframe_at.is_some(),
        "no keyframe within the first 5 access units"
    );

    // Consume a few delta frames (GOP is 60 at 8 fps — no natural IDR soon).
    // Regression for the all-frames-flagged-keyframe bug (#435 root cause,
    // parallax < 0.1.3): these must be honest delta frames.
    for _ in 0..5 {
        let (meta, _) = next_frame(&sub).await;
        assert!(
            !meta.keyframe,
            "mid-GOP frame flagged as keyframe (seq {})",
            meta.sequence
        );
        sequences.push(meta.sequence);
    }

    // Explicit RequestKeyframe → an IDR within the next few frames, long
    // before the natural GOP boundary.
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::RequestKeyframe {
            stream: "test0".into(),
            tier: None,
        },
    )
    .await;
    let mut forced = false;
    for _ in 0..10 {
        let (meta, payload) = next_frame(&sub).await;
        sequences.push(meta.sequence);
        if meta.keyframe {
            // The forced IDR is exactly what a resyncing viewer decodes
            // from — next_frame already asserted SPS/PPS are aboard, and a
            // fresh decoder gate opens here.
            assert!(annexb::has_idr(&payload));
            forced = true;
            break;
        }
    }
    assert!(forced, "RequestKeyframe must force an IDR within 10 frames");

    // Sequence numbers are strictly monotonic across the whole run.
    assert!(
        sequences.windows(2).all(|w| w[1] > w[0]),
        "sequences {sequences:?}"
    );

    // The per-stream state doc reports the h264 profile (v1 LWW state);
    // the subscriber was declared before the open, and viewer-count
    // transitions re-publish the doc.
    let status = await_status(&status_sub, |s| {
        s.open && s.tiers.iter().any(|t| t.viewers >= 1)
    })
    .await;
    // The open used `tier: None` → the sensor default tier (`medium`).
    let tier = status
        .tiers
        .iter()
        .find(|t| t.tier == "medium")
        .expect("default tier present in status");
    // The exact-tier subscriber registered as a viewer: the sensor's matching
    // listener fired on it (this also proves the idle reaper — idle_timeout_secs
    // is 1s and we streamed well past that but saw a viewer).
    assert!(
        tier.viewers >= 1,
        "matching listener must count the exact-tier subscriber as a viewer"
    );

    // Teardown before the runtime drops (blocking source task).
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
            codec: None,
            tier: None,
        },
    )
    .await;
    drop(sub);
    wait_until_closed(&handle).await;

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

/// A test source tall enough (360) to offer two *distinct* video tiers:
/// `low` (capped at 240 → downscaled) and `high` (uncapped → native 360).
/// `medium` (cap 480) would upscale a 360 source, so the catalogue omits it —
/// exactly the two-tier ladder the acceptance test needs.
fn simulcast_parallax_config() -> ParallaxConfig {
    json5::from_str(
        r#"{
            enumerate_v4l2: false,
            test_sources: [
                { name: "test0", pattern: "smpte", width: 640, height: 360, fps: 15 },
            ],
            preview: { fps: 4, quality: 70 },
            idle_timeout_secs: 3,
        }"#,
    )
    .unwrap()
}

/// The epic #494 acceptance criterion: two viewers on the *same* camera, one
/// on `…/video/h264/low` and one on `…/video/h264/high`, both rendering
/// independently. Today's single-encoder architecture structurally cannot do
/// this (one encoder, last-writer-wins on resolution); demand-driven tiered
/// simulcast runs a separate pipeline per (stream, tier), so each viewer gets
/// the resolution its link asked for and neither perturbs the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_viewers_on_distinct_tiers_stream_independently() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-simulcast";
    let host_prefix = "parallax".to_string();
    let handle = spawn_sensor_with_config(sensor.clone(), source, simulcast_parallax_config())
        .await
        .0;

    // The catalogue offers exactly the two non-upscaling tiers.
    let got = query_catalogue(&viewer, &host_prefix).await;
    let tier_names: Vec<&str> = got[0].tiers.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tier_names,
        vec!["low", "high"],
        "a 360-high source offers low (240) + high (native); medium would upscale"
    );

    // Subscribe to each tier's EXACT key (v1.3 revoked the `video/h264/*`
    // wildcard) BEFORE opening, so each first IDR is observed.
    let low_key = v1ctx().media_video_key("test0", "h264", "low");
    let high_key = v1ctx().media_video_key("test0", "h264", "high");
    let low_sub = viewer
        .declare_subscriber(low_key.as_keyexpr())
        .await
        .expect("declare low-tier subscriber");
    let high_sub = viewer
        .declare_subscriber(high_key.as_keyexpr())
        .await
        .expect("declare high-tier subscriber");
    let status_sub = viewer
        .declare_subscriber(zenoh::key_expr::OwnedKeyExpr::from(v1ctx().state_key(&["stream", "test0"])))
        .await
        .expect("declare status sub");

    // Two independent opens, one per tier.
    for tier in ["low", "high"] {
        send_control(
            &viewer,
            &host_prefix,
            StreamControl::OpenStream {
                stream: "test0".into(),
                codec: Some("h264".into()),
                tier: Some(tier.into()),
            },
        )
        .await;
    }

    async fn next_h264(
        sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    ) -> (FrameMeta, Vec<u8>) {
        let sample = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("video frame timed out")
            .expect("video subscriber closed");
        assert!(
            sample.encoding().to_string().starts_with("video/h264"),
            "video must carry video/h264, got {}",
            sample.encoding()
        );
        let att = sample.attachment().expect("frame has FrameMeta attachment");
        let meta: FrameMeta = decode(&att.to_bytes(), Format::Cbor).expect("decode FrameMeta");
        let payload = sample.payload().to_bytes().to_vec();
        (meta, payload)
    }

    // Read a keyframe from each tier: the low branch is scaled to 240, the high
    // branch stays native 360. Distinct encoded geometry on the same source is
    // the proof that these are two independent encoders, not one shared stream.
    async fn first_keyframe(
        sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    ) -> FrameMeta {
        for _ in 0..8 {
            let (meta, payload) = next_h264(sub).await;
            if meta.keyframe {
                assert!(
                    annexb::has_idr(&payload) && annexb::has_param_sets(&payload),
                    "keyframe AU must carry an IDR + SPS/PPS (seq {})",
                    meta.sequence
                );
                return meta;
            }
        }
        panic!("no keyframe within the first 8 access units");
    }

    let low_meta = first_keyframe(&low_sub).await;
    let high_meta = first_keyframe(&high_sub).await;
    assert_eq!(
        low_meta.height, 240,
        "low tier must be downscaled to its 240 cap"
    );
    assert_eq!(
        high_meta.height, 360,
        "high tier is uncapped → native source height"
    );
    assert_ne!(
        (low_meta.width, low_meta.height),
        (high_meta.width, high_meta.height),
        "the two tiers must carry genuinely different encoded geometry"
    );

    // The per-stream status doc lists BOTH tiers, each with its own applied
    // resolution and its own viewer count — the demand signal is per-tier.
    let status = await_status(&status_sub, |s| {
        s.open && s.tiers.len() == 2 && s.tiers.iter().all(|t| t.viewers >= 1)
    })
    .await;
    let low_status = status
        .tiers
        .iter()
        .find(|t| t.tier == "low")
        .expect("low tier in status");
    let high_status = status
        .tiers
        .iter()
        .find(|t| t.tier == "high")
        .expect("high tier in status");
    assert_eq!(low_status.applied.height, 240);
    assert_eq!(high_status.applied.height, 360);

    // Independence: closing the LOW tier must not disturb the HIGH tier — its
    // frames keep flowing without so much as a forced re-key.
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
            codec: Some("h264".into()),
            tier: Some("low".into()),
        },
    )
    .await;
    drop(low_sub);

    // The high tier is still live: several more frames arrive after the low
    // tier is gone.
    for _ in 0..3 {
        let (meta, _) = next_h264(&high_sub).await;
        assert_eq!(
            meta.height, 360,
            "high tier's geometry must be unchanged by the low tier closing"
        );
    }

    // Status now shows the low tier without a viewer (dropped to zero, or
    // already reaped) while the high tier still carries its viewer — the demand
    // signal is genuinely per-tier, and closing one left the other untouched.
    let status = await_status(&status_sub, |s| {
        s.tiers.iter().all(|t| t.tier != "low" || t.viewers == 0)
            && s.tiers.iter().any(|t| t.tier == "high" && t.viewers >= 1)
    })
    .await;
    assert!(
        status.open,
        "the stream stays open while the high tier still has a viewer"
    );

    // Teardown before the runtime drops (blocking source tasks).
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
            codec: Some("h264".into()),
            tier: Some("high".into()),
        },
    )
    .await;
    drop(high_sub);
    wait_until_closed(&handle).await;

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_ticker_publishes_fps_telemetry() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-stats";
    let host_prefix = "parallax".to_string();
    let handle = spawn_sensor(sensor.clone(), source).await;

    // Watch the stream's stats subtree (ordinary telemetry keys).
    let stats_sub = viewer
        .declare_subscriber(format!("{}/test0/stats/**", v1ctx().telemetry_prefix()))
        .await
        .expect("declare stats subscriber");

    // Keep a media viewer subscribed so the 1 s idle reaper never fires.
    let preview_key = v1ctx().media_key(&["test0", "preview", "jpeg"]);
    let media_sub = viewer
        .declare_subscriber(preview_key.as_keyexpr())
        .await
        .expect("declare preview subscriber");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "test0".into(),
            codec: Some("mjpeg".into()),
            tier: None,
        },
    )
    .await;

    // An fps point must arrive within two ticker intervals (1 s each; be
    // generous on a loaded machine).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_fps = false;
    while Instant::now() < deadline {
        let Ok(Ok(sample)) =
            tokio::time::timeout(Duration::from_secs(5), stats_sub.recv_async()).await
        else {
            break;
        };
        let point: zensight_common::TelemetryPoint =
            zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode stats point");
        assert_eq!(point.source, source);
        assert!(point.metric.starts_with("test0/stats/"), "{}", point.metric);
        if point.metric == "test0/stats/fps" {
            saw_fps = true;
            break;
        }
    }
    assert!(saw_fps, "no fps stats point within two intervals");

    // Teardown before the runtime drops.
    send_control(
        &viewer,
        &host_prefix,
        StreamControl::CloseStream {
            stream: "test0".into(),
            codec: None,
            tier: None,
        },
    )
    .await;
    drop(media_sub);
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
    let host_prefix = "parallax".to_string();
    let handle = spawn_sensor(sensor.clone(), source).await;

    let preview_key = v1ctx().media_key(&["test0", "preview", "jpeg"]);
    let sub = viewer
        .declare_subscriber(preview_key.as_keyexpr())
        .await
        .expect("declare preview subscriber");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "test0".into(),
            codec: Some("mjpeg".into()),
            tier: None,
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
            codec: None,
            tier: None,
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

/// One unreachable RTSP camera (connection refused fast on loopback) plus
/// the usual test source, short idle timeout.
fn rtsp_parallax_config(url: &str) -> ParallaxConfig {
    json5::from_str(&format!(
        r#"{{
            enumerate_v4l2: false,
            rtsp: [ {{ name: "deadcam", url: "{url}" }} ],
            test_sources: [
                {{ name: "test0", pattern: "smpte", width: 160, height: 120, fps: 8 }},
            ],
            preview: {{ fps: 4, quality: 70 }},
            idle_timeout_secs: 1,
        }}"#
    ))
    .unwrap()
}

/// A failed open must (a) publish a definitive `open: false` StreamStatus
/// transition (the GUI flags the waiting tile with it), and (b) leave no
/// entry in the stats registry — a leaked entry means phantom zero-valued
/// stats telemetry forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_open_publishes_closed_status_and_leaks_no_stats() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-failed-open";
    let host_prefix = "parallax".to_string();
    // Loopback port 1: nothing listens there, the connect is refused fast.
    let (handle, registry) = spawn_sensor_with_config(
        sensor.clone(),
        source,
        rtsp_parallax_config("rtsp://127.0.0.1:1/dead"),
    )
    .await;

    // Watch the status transitions BEFORE opening.
    let status_sub = viewer
        .declare_subscriber(zenoh::key_expr::OwnedKeyExpr::from(v1ctx().state_key(&["stream", "deadcam"])))
        .await
        .expect("declare status subscriber");

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "deadcam".into(),
            codec: Some("mjpeg".into()),
            tier: None,
        },
    )
    .await;

    // Transitions: `open: true` while the connect is pending, then the
    // definitive `open: false` when it fails. Bounded well above the RTSP
    // connect timeout (5 s + 1 s grace).
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_closed = false;
    while Instant::now() < deadline {
        let Ok(Ok(sample)) =
            tokio::time::timeout(Duration::from_secs(10), status_sub.recv_async()).await
        else {
            break;
        };
        let status: StreamStatus =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("decode StreamStatus");
        assert_eq!(status.stream, "deadcam");
        if !status.open {
            saw_closed = true;
            break;
        }
    }
    assert!(
        saw_closed,
        "a failed open must publish an open: false StreamStatus transition"
    );

    // No session left open, and — the leak this test pins — no stats entry.
    assert!(handle.open_streams().await.is_empty());
    assert!(
        registry.snapshot().is_empty(),
        "a failed open must remove the stream's stats entry"
    );

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}

/// An in-flight RTSP connect must not stall the actor: catalogue and status
/// queries answer immediately while the connect is pending off-actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rtsp_connect_does_not_block_stream_queries() {
    let (sensor, viewer) = isolated_pair().await;
    let source = "e2e-nonblocking";
    let host_prefix = "parallax".to_string();
    // TEST-NET-3 address: blackholed on any sane network, so the connect
    // hangs until the 5 s timeout (if the local network answers fast the
    // test still passes — it just exercises less waiting).
    let (handle, _registry) = spawn_sensor_with_config(
        sensor.clone(),
        source,
        rtsp_parallax_config("rtsp://203.0.113.1:554/blackhole"),
    )
    .await;

    send_control(
        &viewer,
        &host_prefix,
        StreamControl::OpenStream {
            stream: "deadcam".into(),
            codec: Some("h264".into()),
            tier: None,
        },
    )
    .await;

    // While the connect is (very likely still) pending, the catalogue must
    // answer well within the RTSP connect timeout.
    let started = Instant::now();
    let got = tokio::time::timeout(
        Duration::from_secs(3),
        query_catalogue(&viewer, &host_prefix),
    )
    .await
    .expect("catalogue query must not be stalled by an in-flight rtsp connect");
    assert_eq!(got.len(), 2);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "catalogue reply took {:?}",
        started.elapsed()
    );

    // The pending open shows as an open stream in the actor's snapshot —
    // unless the network answered with a fast unreachable and the connect
    // already failed (then the reservation is legitimately gone).
    let open = handle.open_streams().await;
    assert!(
        open == HashSet::from(["deadcam".to_string()]) || open.is_empty(),
        "unexpected open set {open:?}"
    );

    // The pending reservation resolves (here: fails) on its own; it never
    // leaks past the bounded connect timeout.
    wait_until_closed(&handle).await;

    viewer.close().await.unwrap();
    sensor.close().await.unwrap();
}
