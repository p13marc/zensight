//! Pure transport: pull encoded buffers from an [`AppSinkHandle`], publish
//! them on a [`RawMediaPublisher`] with a CBOR [`FrameMeta`] attachment.
//!
//! `pull_buffer_timeout` blocks a thread, so every pull is bridged through
//! `spawn_blocking`; the publish itself is async. The loop ends on pipeline
//! EOS (clean) or on the first pull/publish error — the caller reports the
//! outcome to the session actor as an `EgressEnded` message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parallax::clock::ClockTime;
use parallax::elements::AppSinkHandle;
use parallax::metadata::Metadata;
use zenoh::bytes::{Encoding, ZBytes};
use zensight_common::stream::FrameMeta;
use zensight_common::{Format, encode};
use zensight_sensor_core::RawMediaPublisher;

use crate::stats::StreamStats;

/// How long one blocking pull waits before re-checking for EOS/abort.
const PULL_TIMEOUT: Duration = Duration::from_millis(100);

/// Watchdog: a freshly opened profile that produces no frame at all within
/// this window is declared dead. A wedged source (e.g. a capture thread that
/// panicked without the executor noticing) otherwise leaves the viewer on
/// "waiting for frames…" forever — erroring out lets the session actor
/// publish a definitive `open: false` the GUI can surface.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// Map a pipeline buffer's [`Metadata`] to the wire [`FrameMeta`].
///
/// `ClockTime::NONE` (the u64::MAX sentinel) maps to `Option::None`; the
/// preview path overrides `keyframe` to `true` (every JPEG is independently
/// decodable, whatever the upstream flags say).
pub fn metadata_to_frame_meta(
    meta: &Metadata,
    width: u32,
    height: u32,
    preview: bool,
) -> FrameMeta {
    FrameMeta {
        keyframe: preview || meta.is_keyframe(),
        pts_ns: clock_ns(meta.pts),
        dts_ns: clock_ns(meta.dts),
        duration_ns: clock_ns(meta.duration),
        sequence: meta.sequence,
        width,
        height,
    }
}

fn clock_ns(t: ClockTime) -> Option<u64> {
    if t.is_none() { None } else { Some(t.nanos()) }
}

/// Pump `sink` into `publisher` until EOS or error, feeding the stream's
/// stats counters (frames/bytes; on the video path, sequence gaps count as
/// drops — preview throttling is intentional and never counted).
///
/// Returns `Ok(())` on clean end-of-stream, `Err(reason)` otherwise.
pub async fn run(
    sink: AppSinkHandle,
    publisher: Arc<RawMediaPublisher>,
    encoding: Encoding,
    width: u32,
    height: u32,
    preview: bool,
    stats: Arc<StreamStats>,
) -> Result<(), String> {
    run_with_watchdog(
        sink,
        publisher,
        encoding,
        width,
        height,
        preview,
        stats,
        FIRST_FRAME_TIMEOUT,
    )
    .await
}

/// [`run`] with an injectable first-frame watchdog window (tests).
#[allow(clippy::too_many_arguments)]
async fn run_with_watchdog(
    sink: AppSinkHandle,
    publisher: Arc<RawMediaPublisher>,
    encoding: Encoding,
    width: u32,
    height: u32,
    preview: bool,
    stats: Arc<StreamStats>,
    first_frame_timeout: Duration,
) -> Result<(), String> {
    let mut last_sequence: Option<u64> = None;
    let started = Instant::now();
    let mut produced_any = false;
    loop {
        let pull_sink = sink.clone();
        let pulled =
            tokio::task::spawn_blocking(move || pull_sink.pull_buffer_timeout(Some(PULL_TIMEOUT)))
                .await
                .map_err(|e| format!("egress pull task panicked: {e}"))?;

        let buffer = match pulled {
            Ok(Some(buffer)) => buffer,
            Ok(None) => {
                if sink.is_eos() {
                    return Ok(());
                }
                if !produced_any && started.elapsed() >= first_frame_timeout {
                    return Err(format!(
                        "no frames from source within {:.1}s of open",
                        first_frame_timeout.as_secs_f64()
                    ));
                }
                continue;
            }
            Err(e) => return Err(format!("pipeline sink error: {e}")),
        };
        produced_any = true;

        let frame_meta = metadata_to_frame_meta(buffer.metadata(), width, height, preview);
        if !preview {
            if let Some(prev) = last_sequence
                && frame_meta.sequence > prev + 1
            {
                stats.record_drops(frame_meta.sequence - prev - 1);
            }
            last_sequence = Some(frame_meta.sequence);
        }
        let attachment =
            encode(&frame_meta, Format::Cbor).map_err(|e| format!("encode FrameMeta: {e}"))?;
        let payload = buffer.as_bytes().to_vec();
        stats.record_frame(payload.len());
        publisher
            .put(payload, encoding.clone(), ZBytes::from(attachment))
            .await
            .map_err(|e| format!("media publish failed: {e}"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax::metadata::BufferFlags;

    #[test]
    fn frame_meta_maps_clock_none_and_keyframe() {
        let mut meta = Metadata::default();
        meta.pts = ClockTime::from_nanos(1_000);
        meta.dts = ClockTime::NONE;
        meta.duration = ClockTime::from_millis(33);
        meta.sequence = 7;
        meta.flags |= BufferFlags::SYNC_POINT;

        let fm = metadata_to_frame_meta(&meta, 320, 240, false);
        assert!(fm.keyframe);
        assert_eq!(fm.pts_ns, Some(1_000));
        assert_eq!(fm.dts_ns, None, "ClockTime::NONE maps to Option::None");
        assert_eq!(fm.duration_ns, Some(33_000_000));
        assert_eq!(fm.sequence, 7);
        assert_eq!((fm.width, fm.height), (320, 240));

        // Delta frame → not a keyframe on the video path…
        let delta = Metadata::default();
        assert!(!metadata_to_frame_meta(&delta, 320, 240, false).keyframe);
        // …but the preview path always flags keyframe (JPEG).
        assert!(metadata_to_frame_meta(&delta, 320, 240, true).keyframe);
    }

    /// A profile whose source never delivers a single frame must error out
    /// (→ `EgressEnded` → `open: false`) instead of leaving the viewer on
    /// "waiting for frames…" forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watchdog_errors_when_source_never_produces() {
        // Lone session: scouting off, no endpoints — cannot join any mesh.
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        cfg.insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        let session = Arc::new(zenoh::open(cfg).await.unwrap());
        let publisher = zensight_sensor_core::Publisher::new(
            session,
            "zensight/parallax/watchdog-test",
            Format::Json,
        );
        let media = publisher
            .raw_media_publisher("zensight/parallax/watchdog-test/@media/x/preview/jpeg")
            .await
            .unwrap();

        // An AppSink handle with no pipeline behind it: pulls time out forever.
        let sink = parallax::elements::AppSink::new();
        let handle = sink.handle();

        let err = run_with_watchdog(
            handle,
            Arc::new(media),
            Encoding::IMAGE_JPEG,
            320,
            240,
            true,
            Arc::new(StreamStats::default()),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("no frames from source"),
            "unexpected error: {err}"
        );
    }
}
