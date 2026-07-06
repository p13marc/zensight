//! End-to-end media-plane test (#359) over a real Zenoh session.
//!
//! Exercises the whole minimal media enabler without the H.264/parallax daemon:
//! a mock stream sensor advertises a stream on `@/query/streams`, honors an
//! `OpenStream` command on `@/commands/stream`, then publishes canned JPEG bytes
//! on the opaque `@media/.../preview/jpeg` key via [`RawMediaPublisher`]. A
//! viewer queries the catalogue, opens the stream, subscribes to the media key,
//! and must observe a **keyframe-flagged** frame produced by the publisher's
//! matching listener firing when the viewer appears.
//!
//! Zenoh needs a multi-thread runtime; a unique per-run source keeps parallel
//! test runs from interfering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use zenoh::bytes::{Encoding, ZBytes};
use zensight_common::command::{Command, command_key, query_key};
use zensight_common::keyexpr::media_preview_key;
use zensight_common::stream::{StreamControl, StreamDescriptor};
use zensight_common::{Format, Protocol};
use zensight_sensor_core::Publisher;

/// One-pixel-ish canned JPEG stand-in (opaque bytes; the test only cares that
/// the exact payload survives the media plane unaltered).
const CANNED_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF,
    0xFF, 0xD9,
];

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
    let session = Arc::new(
        zenoh::open(zenoh::Config::default())
            .await
            .expect("open zenoh session"),
    );
    let source = unique_source();
    let prefix = format!("zensight/netring/{source}");
    let publisher = Publisher::new(session.clone(), prefix.clone(), Format::Json);

    let descriptors = vec![StreamDescriptor {
        stream: "cam0".into(),
        codecs: vec!["mjpeg".into()],
        active: false,
        description: Some("front door".into()),
    }];

    // --- Sensor: serve the stream catalogue on @/query/streams. ---
    let streams_key = query_key(&prefix, "streams");
    let queryable = session
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
    let cmd_sub = session
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
    let replies = session.get(&streams_key).await.expect("query streams");
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
    session
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

    // Publish loop: canned JPEG, tagging the first frame after a viewer joins.
    let media_task = {
        let fk = force_keyframe.clone();
        tokio::spawn(async move {
            let mut seq: u64 = 0;
            loop {
                let keyframe = fk.swap(false, Ordering::SeqCst);
                let attachment =
                    serde_json::to_vec(&serde_json::json!({ "keyframe": keyframe, "seq": seq }))
                        .unwrap();
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
    let media_sub = session
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
                if let Some(att) = sample.attachment() {
                    let meta: serde_json::Value =
                        serde_json::from_slice(&att.to_bytes()).expect("decode frame metadata");
                    if meta["keyframe"] == serde_json::Value::Bool(true) {
                        saw_keyframe = true;
                        break;
                    }
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
    session.close().await.expect("close session");
}
