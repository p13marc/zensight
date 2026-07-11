//! Pure pipeline construction — no Zenoh, no tokio.
//!
//! Each open profile is one parallax [`Pipeline`] ending in an [`AppSink`];
//! the egress task pulls encoded buffers from the returned
//! [`AppSinkHandle`] and publishes them. The H.264 encoder's
//! [`KeyframeHandle`] is cloned **before** the encoder is consumed by the
//! pipeline (elements are unreachable once running).
//!
//! Shapes per source kind are documented in `docs/streams.md`. Frame-rate
//! limiting uses the drop-based `Throttle` element, never the delay-based
//! `RateLimiter` (which would backpressure a live source).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use parallax::buffer::Buffer;
use parallax::converters::PixelFormat as ConvFormat;
use parallax::element::{Element, ProduceContext, ProduceResult, Source};
use parallax::elements::codec::KeyframeHandle;
use parallax::elements::transform::VideoConvertElement;
use parallax::elements::{
    AppSink, AppSinkHandle, AppSrc, AppSrcHandle, ColorType, H264Decoder, H264Encoder,
    H264EncoderConfig, JpegDecoder, JpegEncoder, Throttle, V4l2Src, VideoPattern, VideoTestSrc,
};
use parallax::pipeline::{Executor, Pipeline, UnifiedExecutorConfig};

use crate::catalog::SourceKind;
use crate::config::{PreviewConfig, VideoConfig};
use crate::stats::StreamStats;

/// How many encoded frames an AppSink may queue before dropping the oldest.
/// Live media: a slow egress must never block the encoder.
const SINK_QUEUE: usize = 4;

/// Inter-element channel capacity for our executors (see [`executor`]).
const CHANNEL_CAPACITY: usize = 4;

/// Build the executor these pipelines MUST be started with.
///
/// The default inter-element channel capacity (16) exceeds the
/// `JpegEncoder`'s 16-slot output arena: once the AppSink queue (4) is full,
/// the in-flight JPEG buffers (channel backlog + queue) pin every arena slot
/// and the encoder dies with "Failed to acquire buffer slot" (the
/// `H264Encoder` survives only because its arena has 64 slots). A small
/// channel keeps the whole in-flight budget inside the arena; the AppSink's
/// `drop_on_full` sheds frames for slow consumers.
pub fn executor() -> Executor {
    Executor::with_config(UnifiedExecutorConfig {
        channel_capacity: CHANNEL_CAPACITY,
        ..Default::default()
    })
}

/// Cooperative stop signal for a pipeline's source.
///
/// The unified executor runs source loops on blocking threads and ignores
/// downstream channel closure, so `PipelineHandle::abort()` alone cannot stop
/// a live (infinite) source — the blocking task would keep the runtime (and
/// the pipeline) alive forever. Every source we build is wrapped in a
/// [`StoppableSource`]; flipping this flag makes its next `produce()` return
/// EOS, which unwinds the whole pipeline cleanly. Teardown latency is at most
/// one frame period (the live source's internal pacing).
#[derive(Clone, Debug)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Request the source to end its stream at the next produce call.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Wraps any [`Source`] with the [`StopHandle`] EOS switch.
struct StoppableSource<S: Source> {
    inner: S,
    stop: Arc<AtomicBool>,
}

impl<S: Source> StoppableSource<S> {
    fn new(inner: S) -> (Self, StopHandle) {
        let handle = StopHandle::new();
        (
            Self {
                inner,
                stop: handle.0.clone(),
            },
            handle,
        )
    }
}

impl<S: Source> Source for StoppableSource<S> {
    fn produce(&mut self, ctx: &mut ProduceContext) -> parallax::error::Result<ProduceResult> {
        if self.stop.load(Ordering::Relaxed) {
            return Ok(ProduceResult::Eos);
        }
        self.inner.produce(ctx)
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn output_caps(&self) -> parallax::format::Caps {
        self.inner.output_caps()
    }

    fn output_media_caps(&self) -> parallax::format::ElementMediaCaps {
        self.inner.output_media_caps()
    }

    fn preferred_buffer_size(&self) -> Option<usize> {
        self.inner.preferred_buffer_size()
    }

    fn execution_hints(&self) -> parallax::element::ExecutionHints {
        self.inner.execution_hints()
    }

    fn handle_flow_signal(&mut self, signal: parallax::pipeline::flow::FlowSignal) {
        self.inner.handle_flow_signal(signal)
    }

    fn flow_policy(&self) -> parallax::pipeline::flow::FlowPolicy {
        self.inner.flow_policy()
    }
}

/// Wraps an encoder [`Element`] to time each `process()` call into the
/// stream's stats (`encode_ms` telemetry + the encoder-overrun rule).
struct TimedElement<E: Element> {
    inner: E,
    stats: Arc<StreamStats>,
}

impl<E: Element> TimedElement<E> {
    fn new(inner: E, stats: Arc<StreamStats>) -> Self {
        Self { inner, stats }
    }
}

impl<E: Element> Element for TimedElement<E> {
    fn process(&mut self, buffer: Buffer) -> parallax::error::Result<Option<Buffer>> {
        let start = std::time::Instant::now();
        let out = self.inner.process(buffer);
        self.stats.record_encode(start.elapsed().as_nanos() as u64);
        out
    }

    fn flush(&mut self) -> parallax::error::Result<Option<Buffer>> {
        self.inner.flush()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn input_caps(&self) -> parallax::format::Caps {
        self.inner.input_caps()
    }

    fn output_caps(&self) -> parallax::format::Caps {
        self.inner.output_caps()
    }

    fn input_media_caps(&self) -> parallax::format::ElementMediaCaps {
        self.inner.input_media_caps()
    }

    fn output_media_caps(&self) -> parallax::format::ElementMediaCaps {
        self.inner.output_media_caps()
    }

    fn execution_hints(&self) -> parallax::element::ExecutionHints {
        self.inner.execution_hints()
    }
}

/// A constructed (not yet started) profile pipeline.
pub struct BuiltPipeline {
    pub pipeline: Pipeline,
    /// Pull side of the terminal AppSink.
    pub sink: AppSinkHandle,
    /// Force-IDR handle — present only for encoder-backed video profiles
    /// (RTSP passthrough cannot force a remote camera's keyframe).
    pub keyframe: Option<KeyframeHandle>,
    /// Cooperative source stop — MUST be triggered at teardown (see
    /// [`StopHandle`]); `PipelineHandle::abort()` alone leaks the source.
    pub stop: StopHandle,
    /// Push side of an `AppSrc`-fed pipeline (RTSP): the caller must run a
    /// feeder task that pushes buffers and calls `end_stream()` on source
    /// loss. `None` for self-driving sources (test pattern, V4L2).
    pub feed: Option<AppSrcHandle>,
    /// Encoded frame dimensions (stamped into every `FrameMeta`;
    /// `0` = unknown, e.g. RTSP passthrough without SDP dimensions).
    pub width: u32,
    pub height: u32,
}

/// How many buffers an RTSP feeder may queue in the `AppSrc` before frames
/// are shed (live video: never let the feeder back up).
const FEED_QUEUE: usize = 8;

/// Build the H.264 video-profile pipeline for one source.
///
/// `max_height` caps the encoded height (aspect preserved, dimensions
/// even-aligned for I420) on encoder-backed paths.
pub fn build_video(
    kind: &SourceKind,
    video: &VideoConfig,
    max_height: Option<u32>,
    stats: &Arc<StreamStats>,
) -> Result<BuiltPipeline> {
    match kind {
        SourceKind::Test {
            pattern,
            width,
            height,
            fps,
        } => {
            let (w, h) = capped_dimensions(*width, *height, max_height);
            let src = VideoTestSrc::new()
                .with_pattern(parse_pattern(pattern))
                .with_resolution(w, h)
                .with_framerate(*fps, 1)
                .live(true);

            // VideoTestSrc produces packed Rgb24 (its PixelFormat enum has no
            // planar YUV) — convert to I420 for the encoder.
            let convert = VideoConvertElement::new()
                .with_input_format(ConvFormat::Rgb24)
                .with_output_format(ConvFormat::I420)
                .with_size(w, h);

            let encoder = H264Encoder::new(
                H264EncoderConfig::new(w, h)
                    .bitrate(video.bitrate_kbps.saturating_mul(1000))
                    .frame_rate(*fps as f32)
                    .keyframe_interval(video.gop_frames),
            )
            .context("create H.264 encoder")?;
            // Clone BEFORE the encoder moves into the pipeline.
            let keyframe = encoder.keyframe_handle();

            stats.tighten_budget(1_000_000_000 / (*fps).max(1) as u64);
            let sink = AppSink::with_max_buffers(SINK_QUEUE).drop_on_full(true);
            let sink_handle = sink.handle();
            let (src, stop) = StoppableSource::new(src);

            let mut pipeline = Pipeline::new();
            let src_id = pipeline.add_source("test-src", src);
            let conv_id = pipeline.add_filter("convert-i420", convert);
            let enc_id =
                pipeline.add_filter("h264-encoder", TimedElement::new(encoder, stats.clone()));
            let sink_id = pipeline.add_sink("app-sink", sink);
            pipeline.link(src_id, conv_id).context("link src→convert")?;
            pipeline
                .link(conv_id, enc_id)
                .context("link convert→encoder")?;
            pipeline
                .link(enc_id, sink_id)
                .context("link encoder→sink")?;

            Ok(BuiltPipeline {
                pipeline,
                sink: sink_handle,
                keyframe: Some(keyframe),
                stop,
                feed: None,
                width: w,
                height: h,
            })
        }
        SourceKind::V4l2 { device } => {
            let src = V4l2Src::new(device)
                .map_err(|e| anyhow::anyhow!("open v4l2 device {device}: {e}"))?;
            let (w, h) = (src.width(), src.height());
            let fourcc = *src.fourcc();
            let fps = src
                .framerate()
                .map(|(num, den)| num as f32 / den.max(1) as f32)
                .unwrap_or(30.0);
            if max_height.is_some_and(|mh| mh < h) {
                // In-pipeline rescale is not wired yet; encode at the
                // camera's negotiated size rather than fail the open.
                tracing::warn!(device = %device, native = h, requested = ?max_height,
                    "max_height below camera resolution ignored (no rescale element yet)");
            }

            let encoder = H264Encoder::new(
                H264EncoderConfig::new(w, h)
                    .bitrate(video.bitrate_kbps.saturating_mul(1000))
                    .frame_rate(fps)
                    .keyframe_interval(video.gop_frames),
            )
            .context("create H.264 encoder")?;
            let keyframe = encoder.keyframe_handle();

            stats.tighten_budget((1e9 / f64::from(fps.max(1.0))) as u64);
            let sink = AppSink::with_max_buffers(SINK_QUEUE).drop_on_full(true);
            let sink_handle = sink.handle();
            let (src, stop) = StoppableSource::new(src);

            let mut pipeline = Pipeline::new();
            let src_id = pipeline.add_source("v4l2-src", src);
            let enc_id =
                pipeline.add_filter("h264-encoder", TimedElement::new(encoder, stats.clone()));
            let sink_id = pipeline.add_sink("app-sink", sink);
            // The camera negotiates MJPG first, YUYV as fallback; both paths
            // end in I420 for the encoder.
            let to_i420 = match &fourcc {
                b"MJPG" => {
                    // JpegDecoder emits packed RGB (metadata-described).
                    let dec_id = pipeline.add_filter("mjpg-decoder", JpegDecoder::new());
                    let conv_id = pipeline.add_filter(
                        "convert-i420",
                        VideoConvertElement::new()
                            .with_input_format(ConvFormat::Rgb24)
                            .with_output_format(ConvFormat::I420)
                            .with_size(w, h),
                    );
                    pipeline.link(src_id, dec_id).context("link src→decoder")?;
                    pipeline
                        .link(dec_id, conv_id)
                        .context("link decoder→convert")?;
                    conv_id
                }
                b"YUYV" => {
                    let conv_id = pipeline.add_filter(
                        "convert-i420",
                        VideoConvertElement::new()
                            .with_input_format(ConvFormat::Yuyv)
                            .with_output_format(ConvFormat::I420)
                            .with_size(w, h),
                    );
                    pipeline.link(src_id, conv_id).context("link src→convert")?;
                    conv_id
                }
                other => bail!(
                    "unsupported v4l2 pixel format {:?} on {device}",
                    String::from_utf8_lossy(other)
                ),
            };
            pipeline
                .link(to_i420, enc_id)
                .context("link convert→encoder")?;
            pipeline
                .link(enc_id, sink_id)
                .context("link encoder→sink")?;

            Ok(BuiltPipeline {
                pipeline,
                sink: sink_handle,
                keyframe: Some(keyframe),
                stop,
                feed: None,
                width: w,
                height: h,
            })
        }
        SourceKind::Rtsp { .. } => {
            // RTSP needs an async connect first — the session actor calls
            // [`build_rtsp_video_passthrough`] with the SDP info instead.
            bail!("rtsp pipelines are built via the rtsp-specific builders")
        }
    }
}

/// Build the RTSP video-profile pipeline: pure **passthrough** (`AppSrc` →
/// `AppSink`), no re-encode — the camera's H.264 access units are forwarded
/// as-is, so there is no keyframe handle (`request_keyframe` is a no-op) and
/// bitrate/GOP config does not apply. `dimensions` come from the SDP when
/// known (`None` → 0x0 = unknown in `FrameMeta`).
pub fn build_rtsp_video_passthrough(dimensions: Option<(u32, u32)>) -> Result<BuiltPipeline> {
    let src = AppSrc::with_max_buffers(FEED_QUEUE);
    let feed = src.handle();
    let sink = AppSink::with_max_buffers(SINK_QUEUE).drop_on_full(true);
    let sink_handle = sink.handle();
    let (src, stop) = StoppableSource::new(src);

    let mut pipeline = Pipeline::new();
    let src_id = pipeline.add_source("rtsp-feed", src);
    let sink_id = pipeline.add_sink("app-sink", sink);
    pipeline.link(src_id, sink_id).context("link feed→sink")?;

    let (width, height) = dimensions.unwrap_or((0, 0));
    Ok(BuiltPipeline {
        pipeline,
        sink: sink_handle,
        keyframe: None,
        stop,
        feed: Some(feed),
        width,
        height,
    })
}

/// Build the RTSP preview-profile pipeline: decode the camera's H.264,
/// throttle to the preview fps, convert to RGB, and JPEG-encode. Needs the
/// stream dimensions (from the SDP) to size the JPEG encoder.
pub fn build_rtsp_preview(
    width: u32,
    height: u32,
    preview: &PreviewConfig,
    stats: &Arc<StreamStats>,
) -> Result<BuiltPipeline> {
    let src = AppSrc::with_max_buffers(FEED_QUEUE);
    let feed = src.handle();
    let decoder = H264Decoder::new().context("create H.264 decoder")?;
    // Decode everything (delta frames need their references), THEN drop down
    // to the preview rate before the expensive convert+encode.
    let throttle = Throttle::rate(preview.fps as f64);
    let convert = VideoConvertElement::new()
        .with_input_format(ConvFormat::I420)
        .with_output_format(ConvFormat::Rgb24)
        .with_size(width, height);
    let encoder = JpegEncoder::new(width, height, ColorType::Rgb).with_quality(preview.quality);
    stats.tighten_budget(1_000_000_000 / preview.fps.max(1) as u64);
    let sink = AppSink::with_max_buffers(SINK_QUEUE).drop_on_full(true);
    let sink_handle = sink.handle();
    let (src, stop) = StoppableSource::new(src);

    let mut pipeline = Pipeline::new();
    let src_id = pipeline.add_source("rtsp-feed", src);
    let dec_id = pipeline.add_filter("h264-decoder", decoder);
    let thr_id = pipeline.add_filter("preview-throttle", throttle);
    let conv_id = pipeline.add_filter("convert-rgb", convert);
    let enc_id = pipeline.add_filter("jpeg-encoder", TimedElement::new(encoder, stats.clone()));
    let sink_id = pipeline.add_sink("app-sink", sink);
    pipeline.link(src_id, dec_id).context("link feed→decoder")?;
    pipeline
        .link(dec_id, thr_id)
        .context("link decoder→throttle")?;
    pipeline
        .link(thr_id, conv_id)
        .context("link throttle→convert")?;
    pipeline
        .link(conv_id, enc_id)
        .context("link convert→encoder")?;
    pipeline
        .link(enc_id, sink_id)
        .context("link encoder→sink")?;

    Ok(BuiltPipeline {
        pipeline,
        sink: sink_handle,
        keyframe: None,
        stop,
        feed: Some(feed),
        width,
        height,
    })
}

/// Build the JPEG preview-profile pipeline for one source.
pub fn build_preview(
    kind: &SourceKind,
    preview: &PreviewConfig,
    stats: &Arc<StreamStats>,
) -> Result<BuiltPipeline> {
    match kind {
        SourceKind::Test {
            pattern,
            width,
            height,
            ..
        } => {
            // Generate directly at the preview fps — no throttle needed, and
            // Rgb24 output feeds the JPEG encoder without conversion.
            let src = VideoTestSrc::new()
                .with_pattern(parse_pattern(pattern))
                .with_resolution(*width, *height)
                .with_framerate(preview.fps, 1)
                .live(true);

            let encoder =
                JpegEncoder::new(*width, *height, ColorType::Rgb).with_quality(preview.quality);
            stats.tighten_budget(1_000_000_000 / preview.fps.max(1) as u64);

            let sink = AppSink::with_max_buffers(SINK_QUEUE).drop_on_full(true);
            let sink_handle = sink.handle();
            let (src, stop) = StoppableSource::new(src);

            let mut pipeline = Pipeline::new();
            let src_id = pipeline.add_source("test-src", src);
            let enc_id =
                pipeline.add_filter("jpeg-encoder", TimedElement::new(encoder, stats.clone()));
            let sink_id = pipeline.add_sink("app-sink", sink);
            pipeline.link(src_id, enc_id).context("link src→encoder")?;
            pipeline
                .link(enc_id, sink_id)
                .context("link encoder→sink")?;

            Ok(BuiltPipeline {
                pipeline,
                sink: sink_handle,
                keyframe: None,
                stop,
                feed: None,
                width: *width,
                height: *height,
            })
        }
        SourceKind::V4l2 { device } => {
            let src = V4l2Src::new(device)
                .map_err(|e| anyhow::anyhow!("open v4l2 device {device}: {e}"))?;
            let (w, h) = (src.width(), src.height());
            let fourcc = *src.fourcc();
            // Drop frames down to the preview fps right at the source; only
            // then pay for any decode/convert/encode.
            let throttle = Throttle::rate(preview.fps as f64);

            let sink = AppSink::with_max_buffers(SINK_QUEUE).drop_on_full(true);
            let sink_handle = sink.handle();
            let (src, stop) = StoppableSource::new(src);

            let mut pipeline = Pipeline::new();
            let src_id = pipeline.add_source("v4l2-src", src);
            let thr_id = pipeline.add_filter("preview-throttle", throttle);
            let sink_id = pipeline.add_sink("app-sink", sink);
            pipeline.link(src_id, thr_id).context("link src→throttle")?;
            match &fourcc {
                // Camera already produces JPEG frames: pure passthrough.
                b"MJPG" => {
                    pipeline
                        .link(thr_id, sink_id)
                        .context("link throttle→sink")?;
                }
                b"YUYV" => {
                    let conv_id = pipeline.add_filter(
                        "convert-rgb",
                        VideoConvertElement::new()
                            .with_input_format(ConvFormat::Yuyv)
                            .with_output_format(ConvFormat::Rgb24)
                            .with_size(w, h),
                    );
                    stats.tighten_budget(1_000_000_000 / preview.fps.max(1) as u64);
                    let enc_id = pipeline.add_filter(
                        "jpeg-encoder",
                        TimedElement::new(
                            JpegEncoder::new(w, h, ColorType::Rgb).with_quality(preview.quality),
                            stats.clone(),
                        ),
                    );
                    pipeline
                        .link(thr_id, conv_id)
                        .context("link throttle→convert")?;
                    pipeline
                        .link(conv_id, enc_id)
                        .context("link convert→encoder")?;
                    pipeline
                        .link(enc_id, sink_id)
                        .context("link encoder→sink")?;
                }
                other => bail!(
                    "unsupported v4l2 pixel format {:?} on {device}",
                    String::from_utf8_lossy(other)
                ),
            }

            Ok(BuiltPipeline {
                pipeline,
                sink: sink_handle,
                keyframe: None,
                stop,
                feed: None,
                width: w,
                height: h,
            })
        }
        SourceKind::Rtsp { .. } => {
            // RTSP needs an async connect first — the session actor calls
            // [`build_rtsp_preview`] with the SDP dimensions instead.
            bail!("rtsp pipelines are built via the rtsp-specific builders")
        }
    }
}

/// Scale (w, h) down to `max_height` preserving aspect, even-aligning both
/// dimensions (I420 / H.264 requirement). `None` or a cap at/above the native
/// height keeps the native size (still even-aligned).
fn capped_dimensions(width: u32, height: u32, max_height: Option<u32>) -> (u32, u32) {
    let (w, h) = match max_height {
        Some(mh) if mh < height && mh >= 2 => {
            let w = (width as u64 * mh as u64 / height as u64) as u32;
            (w, mh)
        }
        _ => (width, height),
    };
    (w.max(2) & !1, h.max(2) & !1)
}

/// Map a config pattern name onto a `VideoPattern` (unknown → SMPTE + warn).
fn parse_pattern(name: &str) -> VideoPattern {
    match name {
        "smpte" => VideoPattern::SmpteColorBars,
        "checkerboard" => VideoPattern::Checkerboard,
        "solid" => VideoPattern::SolidColor,
        "ball" => VideoPattern::MovingBall,
        "gradient" => VideoPattern::Gradient,
        "black" => VideoPattern::Black,
        "white" => VideoPattern::White,
        "red" => VideoPattern::Red,
        "green" => VideoPattern::Green,
        "blue" => VideoPattern::Blue,
        "circular" => VideoPattern::Circular,
        "snow" => VideoPattern::Snow,
        other => {
            tracing::warn!(pattern = %other, "unknown test pattern; using smpte");
            VideoPattern::SmpteColorBars
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_kind(fps: u32) -> SourceKind {
        SourceKind::Test {
            pattern: "smpte".into(),
            width: 320,
            height: 240,
            fps,
        }
    }

    /// What the tests need from one pulled frame; the Buffer itself is
    /// dropped immediately (mirrors the egress task, which copies and drops).
    struct PulledFrame {
        head: Vec<u8>,
        keyframe: bool,
        sequence: u64,
    }

    /// Start a built (live, unbounded) pipeline, collect `count` frames, then
    /// stop it via the [`StopHandle`] and require a CLEAN shutdown — this
    /// pins the stoppable-source contract (`abort()` alone cannot end a live
    /// source's blocking task; only the EOS switch can).
    async fn pull_frames(built: BuiltPipeline, count: usize) -> Vec<PulledFrame> {
        let mut built = built;
        let handle = executor()
            .start(&mut built.pipeline)
            .expect("start pipeline");
        let sink = built.sink.clone();
        let frames = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for _ in 0..200 {
                match sink.pull_buffer_timeout(Some(Duration::from_millis(500))) {
                    Ok(Some(buf)) => {
                        out.push(PulledFrame {
                            head: buf.as_bytes()[..8.min(buf.len())].to_vec(),
                            keyframe: buf.metadata().is_keyframe(),
                            sequence: buf.metadata().sequence,
                        });
                        if out.len() >= count {
                            break;
                        }
                    }
                    Ok(None) => {
                        if sink.is_eos() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            out
        })
        .await
        .expect("pull task");

        built.stop.stop();
        tokio::time::timeout(Duration::from_secs(10), handle.wait())
            .await
            .expect("pipeline must shut down cleanly after StopHandle::stop()")
            .expect("pipeline tasks must end without error");
        frames
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_preview_pipeline_emits_jpeg() {
        let built = build_preview(
            &test_kind(15),
            &PreviewConfig {
                fps: 10,
                quality: 75,
            },
            &Arc::default(),
        )
        .expect("build preview");
        assert!(built.keyframe.is_none());
        assert_eq!((built.width, built.height), (320, 240));

        let frames = pull_frames(built, 3).await;
        assert!(frames.len() >= 3, "got {} frames", frames.len());
        for frame in &frames {
            // JPEG SOI magic.
            assert_eq!(&frame.head[..2], &[0xFF, 0xD8], "JPEG must start with SOI");
            // Every JPEG is a sync point.
            assert!(frame.keyframe);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_video_pipeline_emits_h264_with_keyframe() {
        let stats: Arc<StreamStats> = Arc::default();
        let built = build_video(&test_kind(30), &VideoConfig::default(), None, &stats)
            .expect("build video");
        let keyframe = built.keyframe.clone();
        assert!(keyframe.is_some());

        let frames = pull_frames(built, 3).await;
        assert!(frames.len() >= 3, "got {} frames", frames.len());
        // The first encoded access unit is an IDR (SYNC_POINT).
        assert!(frames[0].keyframe, "first H.264 frame must be a keyframe");
        // Sequence numbers are monotonic.
        let seqs: Vec<u64> = frames.iter().map(|f| f.sequence).collect();
        assert!(seqs.windows(2).all(|w| w[1] > w[0]), "sequence {seqs:?}");

        // The timed encoder fed the stats counters and set a frame budget.
        use std::sync::atomic::Ordering;
        assert!(stats.encoded_frames.load(Ordering::Relaxed) >= 3);
        assert!(stats.encode_ns.load(Ordering::Relaxed) > 0);
        assert_eq!(
            stats.budget_ns.load(Ordering::Relaxed),
            1_000_000_000 / 30,
            "budget derives from the source fps"
        );
    }

    #[test]
    fn video_respects_max_height() {
        let built = build_video(
            &SourceKind::Test {
                pattern: "smpte".into(),
                width: 640,
                height: 360,
                fps: 15,
            },
            &VideoConfig::default(),
            Some(180),
            &Arc::default(),
        )
        .expect("build capped video");
        assert_eq!((built.width, built.height), (320, 180));
    }

    #[test]
    fn capped_dimensions_even_aligned() {
        assert_eq!(capped_dimensions(641, 361, None), (640, 360));
        assert_eq!(capped_dimensions(640, 360, Some(181)), (320, 180));
        assert_eq!(capped_dimensions(640, 360, Some(720)), (640, 360));
        assert_eq!(capped_dimensions(3, 3, Some(1)), (2, 2));
    }

    #[test]
    fn missing_camera_fails_cleanly() {
        // No such device on this machine: the builders must error, not panic.
        let v4l2 = SourceKind::V4l2 {
            device: "/dev/video-does-not-exist".into(),
        };
        assert!(build_video(&v4l2, &VideoConfig::default(), None, &Arc::default()).is_err());
        assert!(build_preview(&v4l2, &PreviewConfig::default(), &Arc::default()).is_err());
    }

    #[test]
    fn rtsp_kind_requires_the_rtsp_builders() {
        let rtsp = SourceKind::Rtsp {
            url: "rtsp://cam.local/1".into(),
            username: None,
            password: None,
        };
        assert!(build_video(&rtsp, &VideoConfig::default(), None, &Arc::default()).is_err());
        assert!(build_preview(&rtsp, &PreviewConfig::default(), &Arc::default()).is_err());
    }

    #[test]
    fn rtsp_builders_construct() {
        let v = build_rtsp_video_passthrough(Some((1280, 720))).expect("passthrough");
        assert!(v.keyframe.is_none(), "passthrough cannot force keyframes");
        assert!(v.feed.is_some(), "rtsp pipelines are AppSrc-fed");
        assert_eq!((v.width, v.height), (1280, 720));

        let unknown = build_rtsp_video_passthrough(None).expect("passthrough w/o dims");
        assert_eq!((unknown.width, unknown.height), (0, 0), "0 = unknown");

        let p = build_rtsp_preview(640, 360, &PreviewConfig::default(), &Arc::default())
            .expect("preview");
        assert!(p.keyframe.is_none());
        assert!(p.feed.is_some());
        assert_eq!((p.width, p.height), (640, 360));
    }

    /// Feed canned "access units" through the RTSP passthrough pipeline and
    /// require them to come out unaltered, then a clean EOS unwind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rtsp_passthrough_forwards_bytes_and_ends_cleanly() {
        use parallax::buffer::{Buffer, MemoryHandle};
        use parallax::memory::SharedArena;
        use parallax::metadata::{BufferFlags, Metadata};

        let mut built = build_rtsp_video_passthrough(Some((320, 240))).expect("build");
        let feed = built.feed.take().expect("feed handle");
        let sink = built.sink.clone();
        let handle = executor()
            .start(&mut built.pipeline)
            .expect("start pipeline");

        // Push three fake NAL payloads, first flagged as a keyframe.
        let arena = SharedArena::new(64, 8).expect("arena");
        for seq in 0..3u64 {
            let payload = [0x00, 0x00, 0x00, 0x01, 0x65, seq as u8];
            let mut slot = arena.acquire().expect("slot");
            slot.data_mut()[..payload.len()].copy_from_slice(&payload);
            let mut metadata = Metadata::from_sequence(seq);
            if seq == 0 {
                metadata.flags |= BufferFlags::SYNC_POINT;
            }
            feed.push_buffer(Buffer::new(
                MemoryHandle::with_len(slot, payload.len()),
                metadata,
            ))
            .expect("push");
        }
        feed.end_stream();

        let frames = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for _ in 0..100 {
                match sink.pull_buffer_timeout(Some(Duration::from_millis(200))) {
                    Ok(Some(buf)) => out.push((
                        buf.as_bytes().to_vec(),
                        buf.metadata().is_keyframe(),
                        buf.metadata().sequence,
                    )),
                    Ok(None) => {
                        if sink.is_eos() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            out
        })
        .await
        .expect("pull task");

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].0, vec![0x00, 0x00, 0x00, 0x01, 0x65, 0]);
        assert!(frames[0].1, "keyframe flag survives passthrough");
        assert!(!frames[1].1);
        assert_eq!(
            frames.iter().map(|f| f.2).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        // end_stream → source EOS → the whole pipeline unwinds cleanly.
        tokio::time::timeout(Duration::from_secs(10), handle.wait())
            .await
            .expect("pipeline must end after end_stream")
            .expect("pipeline tasks must end without error");
    }
}
