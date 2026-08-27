//! The receiver half of the `@media` plane, against a **live** sensor
//! (#716, #717, #718 — epic #712).
//!
//! Everything else about this feature is testable in isolation: the frame-age
//! rule is arithmetic, the playout policy is a pure function, the drop taxonomy
//! is a counter. What none of that can show is the property the epic exists
//! for — that the loop **closes**: a tile subscribes, decodes, measures itself,
//! and its producer publishes what it heard back onto the bus.
//!
//! So this is `#[ignore]`d, needs a running sensor, and is run by hand:
//!
//! ```sh
//! ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:17451 ZENSIGHT_ZENOH_SCOUTING=false \
//!   ZENSIGHT_ZENOH_GOSSIP=false \
//!   cargo run -p zensight-sensor-parallax -- --config configs/parallax.json5 &
//!
//! ZENSIGHT_MEDIA_LIVE_ENDPOINT=tcp/127.0.0.1:17451 \
//!   cargo test -p zensight --features h264 --test media_receiver_live -- --ignored --nocapture
//! ```
//!
//! It is not in CI, and could not be: the `features` job only `cargo check`s
//! the `h264` feature, and no CI job stands a camera up. The conformance gate
//! (`scripts/conformance-verify.sh`) is where a live deployment gets judged.

#![cfg(feature = "h264")]

use std::sync::Arc;
use std::time::Duration;

use iced::futures::StreamExt;
use zensight::message::Message;
use zensight::view::specialized::parallax_h264::h264_tile_stream;
use zensight_common::command::stream_report_key;
use zensight_common::config::ZenohConfig;
use zensight_common::stream::{MediaReceiverReport, StreamControl};

/// The stream and tier `configs/parallax.json5` ships: a synthetic SMPTE test
/// pattern, so this runs on a headless box with no camera.
const STREAM: &str = "test0";
const TIER: &str = "medium";

fn endpoint() -> String {
    std::env::var("ZENSIGHT_MEDIA_LIVE_ENDPOINT")
        .unwrap_or_else(|_| "tcp/127.0.0.1:17451".to_string())
}

async fn viewer() -> Arc<zenoh::Session> {
    let config = ZenohConfig {
        mode: "client".to_string(),
        connect: vec![endpoint()],
        listen: vec![],
        scouting: Some(false),
        gossip: Some(false),
        ..Default::default()
    };
    Arc::new(
        zensight_common::connect(&config)
            .await
            .expect("connect to the sensor — is one running on the endpoint above?"),
    )
}

/// Resolve the sensor's origin from the key its catalogue reply comes back on.
///
/// A viewer must not wildcard the origin on `@media` (RFC 07 §1), so the test
/// has to do what the GUI does: resolve the host first, then subscribe exactly.
async fn sensor_origin(session: &zenoh::Session) -> zenkey::RemoteOrigin {
    let replies = session
        .get(zensight_common::fleet_rpc_key("parallax", "streams"))
        .timeout(Duration::from_secs(5))
        .await
        .expect("catalogue query");
    let reply = replies.recv_async().await.expect("a catalogue reply");
    let key = reply
        .result()
        .expect("catalogue reply is a value")
        .key_expr();
    let origin = key
        .as_str()
        .split('/')
        .find(|chunk| chunk.starts_with("h-"))
        .expect("the reply key names the host origin");
    zenkey::RemoteOrigin::parse(origin).expect("a legal v1 origin")
}

/// Open one tier on the sensor, exactly as the GUI's per-tier button does.
async fn open_tier(session: &zenoh::Session, origin: &zenkey::RemoteOrigin) {
    let open = zensight_common::command::Command::new(StreamControl::OpenStream {
        stream: STREAM.to_string(),
        codec: Some("h264".to_string()),
        tier: Some(TIER.to_string()),
    });
    session
        .get(zensight_common::origin_rpc_key(
            origin,
            "parallax",
            "stream/set",
        ))
        .payload(serde_json::to_vec(&open).expect("encode open"))
        .timeout(Duration::from_secs(5))
        .await
        .expect("open_stream")
        .recv_async()
        .await
        .expect("open_stream ack");
}

/// #716's acceptance criterion, against a real encoder: a tile whose deadline
/// it cannot meet **still plays**, at keyframe rate, and says so honestly.
///
/// A zero-millisecond deadline is the cheapest way to reproduce a permanently
/// late tile without netem: every delta frame arrives "too old", so the tile
/// sheds each one and rides the IDRs. What must NOT happen is the failure this
/// policy was written to avoid — shedding the keyframes too and showing a black
/// rectangle forever — and what must be true afterwards is that the shedding
/// lands in `dropped_frames`, never in `lost_frames`, because nothing was lost
/// on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a running parallax sensor; see the module header"]
async fn a_tile_that_cannot_meet_its_deadline_rides_the_keyframes_and_says_so() {
    let session = viewer().await;
    let origin = sensor_origin(&session).await;
    open_tier(&session, &origin).await;

    let mut tile = Box::pin(h264_tile_stream(
        session.clone(),
        origin.clone(),
        STREAM.to_string(),
        TIER.to_string(),
        2,
        Some(Duration::ZERO),
    ));

    let mut frames = 0usize;
    let mut keyframe_requests = 0usize;
    let mut report: Option<MediaReceiverReport> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while frames == 0 || report.as_ref().is_none_or(|r| r.dropped_frames == 0) {
        match tokio::time::timeout_at(deadline, tile.next()).await {
            Err(_) => panic!(
                "40 s with a zero deadline: frames = {frames}, report = {report:?}. \
                 A permanently late tile must ride the keyframes, not go black."
            ),
            Ok(None) => panic!("the tile stream ended early"),
            Ok(Some(Message::ParallaxFrame { .. })) => frames += 1,
            Ok(Some(Message::ParallaxRequestKeyframe { .. })) => keyframe_requests += 1,
            Ok(Some(Message::ParallaxReceiverReport { report: r, .. })) => report = Some(*r),
            Ok(Some(Message::ParallaxTileEnded { error, .. })) => {
                panic!("the tile ended: {error:?}")
            }
            Ok(Some(_)) => {}
        }
    }

    let report = report.expect("a report");
    eprintln!(
        "zero deadline: decoded {frames} keyframes, asked for {keyframe_requests}; \
         report = {report:#?}"
    );
    assert!(
        frames > 0,
        "a late keyframe is always decoded — otherwise the tile is a black rectangle"
    );
    assert!(report.dropped_frames > 0, "the late deltas were shed");
    assert_eq!(
        report.lost_frames, 0,
        "our own sheds must never be reported to the producer as network loss: {report:?}"
    );
    // The resync backoff is what keeps a permanent deadline miss from becoming
    // the keyframe storm #435 was about: one request per 2 s, not one per
    // shed frame.
    assert!(
        keyframe_requests <= frames + report.dropped_frames as usize,
        "keyframe requests must be backed off, not one per shed frame"
    );
}

/// The whole loop, in one test, because the loop is the thing under test:
/// subscribe → decode → measure → report → the producer publishes the
/// aggregate. Splitting it would mean standing the sensor up four times and
/// asserting four halves of one claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a running parallax sensor; see the module header"]
async fn a_live_tile_decodes_measures_and_its_report_reaches_the_producers_aggregate() {
    let session = viewer().await;
    let origin = sensor_origin(&session).await;

    // The rx aggregate is published only while a live report exists, so watch
    // for it BEFORE reporting — otherwise the first tick is missed and the
    // test waits a whole stats period for the second.
    // Subscribed through the origin builder rather than a hand-spelled key,
    // and filtered by suffix below: there is no per-subject telemetry builder,
    // and inventing the key here is exactly the ad-hoc `format!` the v1
    // conventions forbid.
    let rx = session
        .declare_subscriber(zensight_common::keyexpr::origin_telemetry_wildcard(
            origin.host_id().as_str(),
        ))
        .await
        .expect("subscribe to this host's telemetry");
    let rx_consumers = format!("{STREAM}/rx/{TIER}/consumers");

    open_tier(&session, &origin).await;

    // Drive the real tile stream — the same function the GUI spawns — with a
    // deadline generous enough that a slow debug-build decoder does not shed
    // everything the moment the machine hiccups.
    let mut tile = Box::pin(h264_tile_stream(
        session.clone(),
        origin.clone(),
        STREAM.to_string(),
        TIER.to_string(),
        1,
        Some(Duration::from_secs(5)),
    ));

    let mut frames = 0usize;
    let mut report: Option<MediaReceiverReport> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while report.is_none() || frames == 0 {
        let next = tokio::time::timeout_at(deadline, tile.next()).await;
        match next {
            Err(_) => panic!("no frame and/or no report within 30 s (frames = {frames})"),
            Ok(None) => panic!("the tile stream ended early (frames = {frames})"),
            Ok(Some(Message::ParallaxFrame { .. })) => frames += 1,
            Ok(Some(Message::ParallaxReceiverReport { report: r, .. })) => report = Some(*r),
            Ok(Some(Message::ParallaxTileEnded { error, .. })) => {
                panic!("the tile ended: {error:?}")
            }
            Ok(Some(_)) => {}
        }
    }

    let report = report.expect("a report inside the cadence");
    // Printed because the point of running this by hand is to LOOK at the
    // numbers: a green assertion that frame age is `Some` says nothing about
    // whether it is 40 ms or 4 s.
    eprintln!("decoded {frames} frames; report = {report:#?}");
    assert!(frames > 0, "the tile decoded nothing");
    assert!(
        report.received_frames > 0,
        "a tile that decoded {frames} frames cannot have received none: {report:?}"
    );
    assert_eq!(report.stream, STREAM);
    assert_eq!(report.tier.as_deref(), Some(TIER));
    assert!(
        report.decoder_queue_depth.is_some(),
        "a video tile HAS a decode queue (#717) and must report its depth, \
         not omit it the way a preview tile does: {report:?}"
    );
    assert!(
        report.frame_age_ms.is_some(),
        "our own session forces timestamping on, so frame age is measurable — \
         an absent age here means the clock RFC 07 §1.3 names is not on the \
         wire after all: {report:?}"
    );

    // Now the half the tile cannot do for itself: hand the report to the
    // producer, the way `ZenSight::send_parallax_report` does.
    let ack = session
        .get(stream_report_key("parallax"))
        .payload(serde_json::to_vec(&report).expect("encode report"))
        .timeout(Duration::from_secs(5))
        .await
        .expect("stream/report query")
        .recv_async()
        .await
        .expect("stream/report reply");
    if let Err(e) = ack.result() {
        panic!(
            "the producer refused a report its own consumer produced: {}",
            String::from_utf8_lossy(&e.payload().to_bytes())
        );
    }

    // …and the producer publishes what it heard. This is the loop closing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let consumers = loop {
        let sample = tokio::time::timeout_at(deadline, rx.recv_async())
            .await
            .expect("rx/consumers within one stats period of the report")
            .expect("rx subscriber alive");
        let Ok(point) = zensight_common::decode_auto::<zensight_common::TelemetryPoint>(
            &sample.payload().to_bytes(),
        ) else {
            continue;
        };
        if point.metric == rx_consumers {
            break point.value;
        }
    };
    match consumers {
        zensight_common::TelemetryValue::Gauge(n) => assert!(
            n >= 1.0,
            "the producer counts at least this consumer, got {n}"
        ),
        other => panic!("rx/consumers must be a gauge, got {other:?}"),
    }
}
