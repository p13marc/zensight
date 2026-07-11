//! Pure transport: pull encoded buffers from an [`AppSinkHandle`], publish
//! them on a [`RawMediaPublisher`] with a CBOR [`FrameMeta`] attachment.
//!
//! `pull_buffer_timeout` blocks a thread, so every pull is bridged through
//! `spawn_blocking`; the publish itself is async. The loop ends on pipeline
//! EOS (clean) or on the first pull/publish error — the caller reports the
//! outcome to the session actor as an `EgressEnded` message.

use std::sync::Arc;
use std::time::Duration;

use parallax::clock::ClockTime;
use parallax::elements::AppSinkHandle;
use parallax::metadata::Metadata;
use zenoh::bytes::{Encoding, ZBytes};
use zensight_common::stream::FrameMeta;
use zensight_common::{Format, encode};
use zensight_sensor_core::RawMediaPublisher;

/// How long one blocking pull waits before re-checking for EOS/abort.
const PULL_TIMEOUT: Duration = Duration::from_millis(100);

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

/// Pump `sink` into `publisher` until EOS or error.
///
/// Returns `Ok(())` on clean end-of-stream, `Err(reason)` otherwise.
pub async fn run(
    sink: AppSinkHandle,
    publisher: Arc<RawMediaPublisher>,
    encoding: Encoding,
    width: u32,
    height: u32,
    preview: bool,
) -> Result<(), String> {
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
                continue;
            }
            Err(e) => return Err(format!("pipeline sink error: {e}")),
        };

        let frame_meta = metadata_to_frame_meta(buffer.metadata(), width, height, preview);
        let attachment =
            encode(&frame_meta, Format::Cbor).map_err(|e| format!("encode FrameMeta: {e}"))?;
        publisher
            .put(
                buffer.as_bytes().to_vec(),
                encoding.clone(),
                ZBytes::from(attachment),
            )
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
}
