//! End-to-end media-plane test (#359) over real Zenoh sessions.
//!
//! Exercises the whole minimal media enabler without the H.264/parallax daemon:
//! a mock stream sensor advertises a stream on `@/query/streams`, honors an
//! `OpenStream` command on `@/commands/stream`, then publishes canned JPEG bytes
//! on the opaque `@media/.../preview/jpeg` key via [`RawMediaPublisher`]. A
//! viewer queries the catalogue, opens the stream, subscribes to the media key,
//! and must observe a **keyframe-flagged** [`FrameMeta`] attachment produced by
//! the publisher's matching listener firing when the viewer appears.
//!
//! Two real Zenoh sessions (sensor + viewer) with scouting disabled and an
//! explicit loopback endpoint, so a live sensor container on the host can't
//! contaminate the test (same pattern as `liveliness_e2e.rs`). Zenoh needs a
//! multi-thread runtime; a unique per-run source keeps parallel runs apart.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use zenoh::bytes::{Encoding, ZBytes};
use zensight_common::command::{Command, command_key, query_key};
use zensight_common::keyexpr::media_preview_key;
use zensight_common::stream::{FrameMeta, StreamControl, StreamDescriptor};
use zensight_common::{Format, Protocol, decode, encode};
use zensight_sensor_core::Publisher;

/// One-pixel-ish canned JPEG stand-in (opaque bytes; the test only cares that
/// the exact payload survives the media plane unaltered).
const CANNED_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF,
    0xFF, 0xD9,
];

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

fn unique_source() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cam_{nanos}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_plane_e2e_stream_control_and_keyframe() {
    // "Sensor" peer: listens on an explicit loopback endpoint.
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
    // "Viewer" peer: connects to the sensor.
    let viewer = zenoh::open(connect_config(port))
        .await
        .expect("open connecting viewer session");

    let source = unique_source();
    let prefix = format!("zensight/netring/{source}");
    let publisher = Publisher::new(sensor.clone(), prefix.clone(), Format::Json);

    let descriptors = vec![StreamDescriptor {
        stream: "cam0".into(),
        codecs: vec!["mjpeg".into()],
        active: false,
        description: Some("front door".into()),
    }];

    // --- Sensor: serve the stream catalogue on @/query/streams. ---
    let streams_key = query_key(&prefix, "streams");
    let queryable = sensor
        .declare_queryable(&streams_key)
        .await
        .expect("declare streams queryable");
    {
        let catalogue = descriptors.clone();
        tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                let bytes = serde_json::to_vec(&catalogue).unwrap();
                let key = query.key_expr().clone();
                let _ = query.reply(key, bytes).await;
            }
        });
    }

    // --- Sensor: honor OpenStream on @/commands/stream. ---
    let cmd_key = command_key(&prefix, "stream");
    let cmd_sub = sensor
        .declare_subscriber(&cmd_key)
        .await
        .expect("declare command subscriber");
    let (open_tx, mut open_rx) = tokio::sync::mpsc::channel::<StreamControl>(4);
    tokio::spawn(async move {
        while let Ok(sample) = cmd_sub.recv_async().await {
            let payload = sample.payload().to_bytes();
            if let Ok(cmd) = serde_json::from_slice::<Command<StreamControl>>(&payload) {
                let _ = open_tx.send(cmd.body).await;
            }
        }
    });

    // Let the queryable + subscriber propagate.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- Viewer: query the catalogue (late-joiner seed). ---
    let replies = viewer.get(&streams_key).await.expect("query streams");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("catalogue query timed out")
        .expect("catalogue reply channel closed");
    let sample = reply.result().expect("catalogue reply is an error");
    let got: Vec<StreamDescriptor> =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("decode catalogue");
    assert_eq!(got, descriptors, "viewer must see the advertised stream");

    // --- Viewer: open the stream. ---
    let open = Command::new(StreamControl::OpenStream {
        stream: "cam0".into(),
        codec: Some("mjpeg".into()),
        max_height: None,
    });
    viewer
        .put(&cmd_key, serde_json::to_vec(&open).unwrap())
        .await
        .expect("send OpenStream");

    // --- Sensor: receive OpenStream, spin up the media publisher. ---
    let ctrl = tokio::time::timeout(Duration::from_secs(5), open_rx.recv())
        .await
        .expect("OpenStream not received")
        .expect("command channel closed");
    assert!(
        matches!(ctrl, StreamControl::OpenStream { ref stream, .. } if stream == "cam0"),
        "sensor must receive the OpenStream command"
    );

    let preview_key = media_preview_key(Protocol::Netring, &source, "cam0");
    let media = publisher
        .raw_media_publisher(preview_key.clone())
        .await
        .expect("declare raw media publisher");

    // Matching listener → force a keyframe when a viewer appears.
    let force_keyframe = Arc::new(AtomicBool::new(false));
    {
        let listener = media
            .matching_listener()
            .await
            .expect("declare matching listener");
        let fk = force_keyframe.clone();
        tokio::spawn(async move {
            while let Ok(status) = listener.recv_async().await {
                if status.matching() {
                    fk.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    // Publish loop: canned JPEG with a CBOR FrameMeta attachment, tagging the
    // first frame after a viewer joins as a keyframe.
    let media_task = {
        let fk = force_keyframe.clone();
        tokio::spawn(async move {
            let mut seq: u64 = 0;
            loop {
                let meta = FrameMeta {
                    keyframe: fk.swap(false, Ordering::SeqCst),
                    sequence: seq,
                    width: 320,
                    height: 240,
                    ..FrameMeta::default()
                };
                let attachment = encode(&meta, Format::Cbor).unwrap();
                if media
                    .put(
                        CANNED_JPEG.to_vec(),
                        Encoding::IMAGE_JPEG,
                        ZBytes::from(attachment),
                    )
                    .await
                    .is_err()
                {
                    break;
                }
                seq += 1;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
    };

    // --- Viewer: subscribe to the media key; its arrival trips the keyframe. ---
    let media_sub = viewer
        .declare_subscriber(&preview_key)
        .await
        .expect("declare media subscriber");

    let mut saw_jpeg = false;
    let mut saw_keyframe = false;
    for _ in 0..60 {
        match tokio::time::timeout(Duration::from_secs(3), media_sub.recv_async()).await {
            Ok(Ok(sample)) => {
                assert!(
                    sample.encoding().to_string().starts_with("image/jpeg"),
                    "media samples must carry their real encoding, got {}",
                    sample.encoding()
                );
                assert_eq!(
                    &sample.payload().to_bytes()[..],
                    CANNED_JPEG,
                    "opaque JPEG bytes must survive the media plane unchanged"
                );
                saw_jpeg = true;
                let att = sample.attachment().expect("media sample has an attachment");
                let meta: FrameMeta =
                    decode(&att.to_bytes(), Format::Cbor).expect("decode FrameMeta attachment");
                assert_eq!((meta.width, meta.height), (320, 240));
                if meta.keyframe {
                    saw_keyframe = true;
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(saw_jpeg, "viewer must receive canned JPEG media bytes");
    assert!(
        saw_keyframe,
        "matching listener must force a keyframe once the viewer subscribes"
    );

    media_task.abort();
    viewer.close().await.expect("close viewer session");
    sensor.close().await.expect("close sensor session");
}
