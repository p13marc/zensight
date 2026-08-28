//! Pure transport: pull encoded buffers from an [`AppSinkHandle`], publish
//! them on a [`RawMediaPublisher`] with a CBOR [`FrameMeta`] attachment.
//!
//! Pulls are native async since parallax 0.6 (`pull_buffer_timeout` awaits
//! instead of parking a thread), so pull and publish share the task. The loop
//! ends on pipeline EOS, on a first-frame timeout, or on the first
//! pull/publish error, and says which as an [`EgressEnd`] — the caller passes
//! it to the session actor as an `EgressEnded` message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parallax::clock::ClockTime;
use parallax::codec::annexb::{NalCodec, ParamSetCache, is_entry_point};
use parallax::elements::{AppSinkHandle, Pulled};
use parallax::metadata::Metadata;
use parallax::pipeline::EndReason;
use zenoh::bytes::{Encoding, ZBytes};
use zensight_common::stream::{FrameMeta, StreamEndReason};
use zensight_common::{Format, encode};
use zensight_sensor_core::RawMediaPublisher;

use crate::stats::StreamStats;

/// How long one pull waits before re-checking for EOS/abort.
const PULL_TIMEOUT: Duration = Duration::from_millis(100);

/// Watchdog: a freshly opened profile that produces no frame at all within
/// this window is declared dead. A wedged source (e.g. a capture thread that
/// panicked without the executor noticing) otherwise leaves the viewer on
/// "waiting for frames…" forever — erroring out lets the session actor
/// publish a definitive `open: false` the GUI can surface.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// The `node` an egress-side failure is attributed to. Not a parallax element
/// name — this pump lives outside the graph — but it is the thing that failed,
/// and naming it beats an unattributed message.
const EGRESS_NODE: &str = "egress";

/// How an egress pump ended (#691).
///
/// Total, rather than the `Result<(), String>` this used to be. parallax 0.7
/// gave the pull loop a *typed* [`EndReason`] (#689) and the old signature
/// threw it straight back away: a clean end and a torn-down one both became
/// `Ok(())`, and a failure became a sentence with the element's name baked
/// into it. Three ends the pipeline distinguishes arrived at the session actor
/// as two, and reached the wire as one bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressEnd {
    /// The pipeline signalled clean end-of-stream.
    EndOfStream,
    /// The pipeline was torn down mid-stream. Only `PipelineHandle::ended`
    /// produces this — an `AppSink` never sees it, because an aborted task
    /// cannot deliver — but the match must be total.
    Aborted,
    /// An element failed, in its own words.
    Failed {
        /// The element parallax attributed the failure to, when it named one.
        node: Option<String>,
        /// The failure message, verbatim.
        message: String,
    },
    /// No buffer at all within the first-frame window.
    Stalled {
        /// The window that expired — for the caller's log line. It does not
        /// reach the wire: the window is a producer constant, and
        /// `StreamStatus` is `Eq`, which an `f64` would break.
        window: Duration,
    },
}

impl From<EgressEnd> for StreamEndReason {
    fn from(end: EgressEnd) -> Self {
        match end {
            EgressEnd::EndOfStream => Self::SourceEnded,
            // The producer ended it, not the camera — the same statement the
            // shutdown drain makes. Unreachable through an `AppSink` (parallax
            // says so in terms), kept because the match is total.
            EgressEnd::Aborted => Self::Shutdown,
            EgressEnd::Failed { node, message } => Self::Failed { node, message },
            EgressEnd::Stalled { .. } => Self::Stalled,
        }
    }
}

/// Map a pipeline buffer's [`Metadata`] to the wire [`FrameMeta`].
///
/// `ClockTime::NONE` (the u64::MAX sentinel) maps to `Option::None`; the
/// preview path overrides `keyframe` to `true` (every JPEG is independently
/// decodable, whatever the upstream flags say).
///
/// `dts_ns` is omitted when it *equals* `pts_ns`, not only when the clock is
/// absent (#728). `FrameMeta::dts_ns` is documented "if distinct from
/// `pts_ns`", and parallax's own `FrameMeta::from_metadata` elides it the same
/// way — the two encoders are byte-compatible twins pinned by a shared
/// conformance corpus (`zensight-common/tests/framemeta_corpus.rs`), so this is
/// wire shape, not a saving. It is not a small one either: our encoders emit no
/// B-frames, so *every* frame carried a redundant copy of its own pts.
pub fn metadata_to_frame_meta(
    meta: &Metadata,
    width: u32,
    height: u32,
    preview: bool,
) -> FrameMeta {
    let pts_ns = clock_ns(meta.pts);
    FrameMeta {
        keyframe: preview || meta.is_keyframe(),
        pts_ns,
        dts_ns: clock_ns(meta.dts).filter(|dts| Some(*dts) != pts_ns),
        duration_ns: clock_ns(meta.duration),
        sequence: meta.sequence,
        width,
        height,
    }
}

fn clock_ns(t: ClockTime) -> Option<u64> {
    if t.is_none() { None } else { Some(t.nanos()) }
}

/// Pump `sink` into `publisher` until it ends, feeding the stream's
/// frames/bytes counters.
///
/// It does **not** count drops: `stats/drops` is the sink's own
/// `total_dropped`, read by the session actor on its reap tick (#692). This
/// loop used to infer it from `FrameMeta.sequence` gaps, which was a proxy for
/// that very number and a worse one — see [`StreamStats::drops`].
///
/// Says *how* it ended: see [`EgressEnd`].
pub async fn run(
    sink: AppSinkHandle,
    publisher: Arc<RawMediaPublisher>,
    encoding: Encoding,
    width: u32,
    height: u32,
    preview: bool,
    stats: Arc<StreamStats>,
) -> EgressEnd {
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
) -> EgressEnd {
    let started = Instant::now();
    let mut produced_any = false;
    // H.264 hardening (#435): the published keyframe flag is derived from
    // the bitstream itself (IDR present) rather than upstream metadata, and
    // every keyframe AU is made a self-contained decoder entry point by
    // prepending the stream's cached SPS/PPS when the AU arrived without
    // its own (e.g. RTSP cameras announcing parameter sets only
    // out-of-band in the SDP).
    //
    // The extract/cache/prepend dance is upstream's `ParamSetCache` since
    // parallax 0.8 (#730) — codec-aware, and `Cow::Borrowed` on every delta
    // frame and every keyframe that already carries its sets, so only a
    // genuinely repaired keyframe copies twice.
    let h264 = !preview && encoding == Encoding::VIDEO_H264;
    let mut param_sets = ParamSetCache::new(NalCodec::H264);
    loop {
        // parallax 0.7 replaced `Result<Option<Buffer>>` with `Pulled`, and in
        // doing so made a distinction this loop could not previously draw: a
        // clean end and a failed one used to arrive as `Ok(None)` + `is_eos()`
        // versus `Err`, with the sink's own reason unavailable. `Ended` now
        // carries it, so a stream that finished and a stream that broke are
        // told apart by the pipeline rather than inferred here (#689).
        let buffer = match sink.pull_buffer_timeout(PULL_TIMEOUT).await {
            Pulled::Buffer(buffer) => buffer,
            Pulled::Ended(EndReason::Eos) => return EgressEnd::EndOfStream,
            // The accessors, not `Display`: parallax's `StreamError` renders
            // as `element '<node>': <message>`, and joining the two here would
            // make it impossible to keep them apart on the wire.
            Pulled::Ended(EndReason::Error(e)) => {
                return EgressEnd::Failed {
                    node: e.node().map(str::to_owned),
                    message: e.message().to_owned(),
                };
            }
            // Only `PipelineHandle::ended` produces this — an aborted task
            // cannot deliver to a sink — but the match must be total.
            Pulled::Ended(EndReason::Aborted) => return EgressEnd::Aborted,
            // Not terminal: nothing here ever sets the handle flushing, so
            // this is the same "nothing yet" case as a timeout.
            Pulled::Empty | Pulled::Flushing => {
                if !produced_any && started.elapsed() >= first_frame_timeout {
                    return EgressEnd::Stalled {
                        window: first_frame_timeout,
                    };
                }
                continue;
            }
        };
        produced_any = true;

        let mut frame_meta = metadata_to_frame_meta(buffer.metadata(), width, height, preview);
        // Borrow the encoded bytes rather than copying them up front: on the
        // h264 path `prepare` hands back the same slice for everything but a
        // repaired keyframe, so the repair path now copies once (into a Vec
        // sized for sets + AU) where it used to copy twice. `put` wants an
        // owned `Vec`, so the borrowed path still materializes one — but that
        // copy was always there.
        // A discontinuity re-arms the parameter-set cache (#731): the RTSP
        // source stamps DISCONT on the first buffer after a reconnect, and the
        // sets it cached belong to the *previous* session. Replaying stale
        // SPS/PPS in front of the resumed stream's first keyframe would hand a
        // decoder a picture geometry the bytes no longer match — worse than
        // publishing the keyframe unrepaired and letting the camera's own
        // in-band sets (which arrive within a keyframe on AnnexB) refill the
        // cache.
        if buffer.metadata().is_discont() {
            param_sets.reset();
        }
        let bytes = buffer.as_bytes();
        let payload: std::borrow::Cow<'_, [u8]> = if h264 {
            let prepared = param_sets.prepare(bytes);
            // Read the keyframe verdict off the bytes that actually ship.
            frame_meta.keyframe = is_entry_point(&prepared, NalCodec::H264);
            prepared
        } else {
            std::borrow::Cow::Borrowed(bytes)
        };
        // The two failures on the producer's own side of the sink. `egress`
        // is a real element name here — it is the pump that failed, and `node`
        // is exactly the field for saying which.
        let attachment = match encode(&frame_meta, Format::Cbor) {
            Ok(a) => a,
            Err(e) => {
                return EgressEnd::Failed {
                    node: Some(String::from(EGRESS_NODE)),
                    message: format!("encode FrameMeta: {e}"),
                };
            }
        };
        stats.record_frame(payload.len());
        if let Err(e) = publisher
            .put(
                payload.into_owned(),
                encoding.clone(),
                ZBytes::from(attachment),
            )
            .await
        {
            return EgressEnd::Failed {
                node: Some(String::from(EGRESS_NODE)),
                message: format!("media publish failed: {e}"),
            };
        }
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

    #[test]
    fn dts_is_omitted_when_it_equals_pts() {
        // The common case for us: no B-frames, so the encoder stamps dts == pts
        // on every single frame. `dts_ns` is "if distinct from pts_ns" (#728).
        let mut meta = Metadata::default();
        meta.pts = ClockTime::from_nanos(5_000);
        meta.dts = ClockTime::from_nanos(5_000);

        let fm = metadata_to_frame_meta(&meta, 320, 240, false);
        assert_eq!(fm.pts_ns, Some(5_000));
        assert_eq!(fm.dts_ns, None, "equal to pts_ns, so it is not on the wire");

        // A genuinely distinct dts still travels.
        meta.dts = ClockTime::from_nanos(4_000);
        assert_eq!(
            metadata_to_frame_meta(&meta, 320, 240, false).dts_ns,
            Some(4_000)
        );

        // And ZERO is a real timestamp, not an absent one: a producer that
        // stamps pts and leaves dts at its default publishes `Some(0)`.
        meta.dts = ClockTime::from_nanos(0);
        assert_eq!(
            metadata_to_frame_meta(&meta, 320, 240, false).dts_ns,
            Some(0)
        );
    }

    /// A profile whose source never delivers a single frame must end
    /// (→ `EgressEnded` → `open: false`) instead of leaving the viewer on
    /// "waiting for frames…" forever — and must say `Stalled`, not invent a
    /// failure. Nothing failed: the open succeeded and no element errored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watchdog_reports_stalled_when_source_never_produces() {
        // Lone session: scouting off, no endpoints — cannot join any mesh.
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        cfg.insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        let session = Arc::new(zenoh::open(cfg).await.unwrap());
        let publisher = zensight_sensor_core::Publisher::new(session, "parallax", Format::Json);
        let media = publisher
            .raw_media_publisher(String::from(
                zensight_common::registry::parallax::media_key(
                    &zensight_common::PROFILE.local_origin(),
                    &zensight_common::registry::parallax::Media::preview_jpeg("watchdog-test"),
                ),
            ))
            .await
            .unwrap();

        // An AppSink handle with no pipeline behind it: pulls time out forever.
        let sink = parallax::elements::AppSink::new();
        let handle = sink.handle();

        let end = run_with_watchdog(
            handle,
            Arc::new(media),
            Encoding::IMAGE_JPEG,
            320,
            240,
            true,
            Arc::new(StreamStats::default()),
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(
            end,
            EgressEnd::Stalled {
                window: Duration::from_millis(300)
            }
        );
    }

    /// The whole point of the typed outcome: what the pipeline distinguished
    /// still differs on the wire. Pins `StreamError`'s two fields staying two
    /// fields, which is the fidelity the old `format!` destroyed — and needs
    /// no camera, no runtime and no bus.
    #[test]
    fn egress_end_maps_to_the_wire_reason() {
        use zensight_common::stream::StreamEndReason as R;

        assert_eq!(R::from(EgressEnd::EndOfStream), R::SourceEnded);
        // We tore it down; the camera did not stop.
        assert_eq!(R::from(EgressEnd::Aborted), R::Shutdown);
        // A stall is not a failure with an invented message — it has no
        // payload at all, and the window stays on the producer's side.
        assert_eq!(
            R::from(EgressEnd::Stalled {
                window: Duration::from_secs(10)
            }),
            R::Stalled
        );
        assert_eq!(
            R::from(EgressEnd::Failed {
                node: Some("h264enc".into()),
                message: "encoder submit failed".into(),
            }),
            R::Failed {
                node: Some("h264enc".into()),
                message: "encoder submit failed".into(),
            },
            "node and message must survive as two fields, not one sentence"
        );
    }
}
