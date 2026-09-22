//! Stills and clips over the artifact channel (#414).
//!
//! The live `@media` plane is deliberately lossy and ephemeral. These two
//! producers are its reliable complement: a **still** (one JPEG frame) and a
//! **clip** (a bounded H.264 recording in an MP4), requested on
//! `@rpc/parallax/artifact/request` and delivered on `@blob/artifact` with
//! the progress, status, cancel and TTL every artifact gets for free from
//! `zensight-sensor-core`.
//!
//! **One-shot pipelines, no tee.** parallax 0.9 has no fan-out element, and
//! `docs/streams.md` explains why the live tiers do not share a source
//! either: a preview must keep its cadence whether or not an encoder runs.
//! So a still or a clip opens its *own* pipeline — the same graph
//! [`pipeline::build_preview`] / [`pipeline::build_video`] builds for a
//! live tier, torn down when the artifact is done — which is free for a
//! test source and an RTSP camera and collides only with an exclusive
//! device: a V4L2 stream that is already open is **refused**, never waited
//! for (waiting would hold the kind's busy slot for up to `idle_timeout_secs`
//! with nothing to show).
//!
//! **The producers own their limits**, as netring's capture does: the
//! `parallax.artifacts.still` and `.clip` blocks carry the byte cap, the
//! cooldown and the TTL the channel enforces, plus each kind's own knobs.
//!
//! **Why `Mp4Mux` and not `Mp4FileSink`.** The sink is an executor element —
//! it is only drivable inside a running pipeline. Keeping the live graph's
//! `AppSink` and driving the muxer by hand from the pulled access units is
//! what gives the clip the same byte budget and cancel loop netring's
//! capture has, and keeps [`pipeline::build_video`] the one video graph.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parallax::codec::annexb::{NalCodec, annex_b_to_avcc, extract_param_sets, is_entry_point};
use parallax::elements::Pulled;
use parallax::elements::mux::{Mp4Mux, Mp4MuxConfig, Mp4VideoTrackConfig};
use serde::{Deserialize, Serialize};
use zensight_common::config::CommonArtifactLimits;
use zensight_common::rpc::RpcError;
use zensight_common::{ArtifactKind, KindAdvert};
use zensight_sensor_core::artifact::{
    ArtifactProducer, DeliveryKind, ProduceCtx, Produced, ProgressUpdate,
};

use crate::catalog::{Catalog, SourceKind};
use crate::config::{PreviewConfig, VideoConfig};
use crate::pipeline::{self, BuiltPipeline};
use crate::session::SessionHandle;
use crate::stats::StreamStats;

/// The `parallax.artifacts` block: the two producers' own limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParallaxArtifactsConfig {
    #[serde(default)]
    pub still: StillConfig,
    #[serde(default)]
    pub clip: ClipConfig,
}

impl ParallaxArtifactsConfig {
    /// The bounds a request is later clamped to must themselves make sense.
    pub fn validate(&self, video: &VideoConfig) -> anyhow::Result<()> {
        if !(1..=100).contains(&self.still.quality) {
            anyhow::bail!("artifacts.still.quality must be 1..=100");
        }
        if self.clip.enabled && self.clip.max_duration_secs == 0 {
            anyhow::bail!("artifacts.clip.max_duration_secs must be > 0");
        }
        if self.clip.enabled && video.tiers.is_empty() {
            anyhow::bail!("artifacts.clip needs at least one video tier to record");
        }
        Ok(())
    }
}

/// `parallax.artifacts.still`: one JPEG frame of a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StillConfig {
    /// Serve stills at all (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// JPEG quality 1–100 (default: 85 — a still is looked at, a preview is
    /// glanced at).
    #[serde(default = "default_still_quality")]
    pub quality: u8,
    /// Aspect-preserving height cap (default: none — the source size).
    #[serde(default)]
    pub max_height: Option<u32>,
    /// Byte cap on the JPEG (default: 8 MiB); a larger frame fails the
    /// request rather than serving it.
    #[serde(default = "default_still_max_bytes")]
    pub max_bytes: u64,
    /// Min gap between stills, seconds (default: 5).
    #[serde(default = "default_still_cooldown")]
    pub cooldown_secs: u64,
    /// How long a still stays downloadable, seconds (default: 600).
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
    /// zblob transfer chunk (default: 512 KiB).
    #[serde(default = "default_chunk")]
    pub chunk_size: u32,
}

/// `parallax.artifacts.clip`: a bounded H.264 recording in an MP4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipConfig {
    /// Serve clips at all (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Largest clip a request can ask for, seconds (default: 60); a longer
    /// request is clamped, and the clip says so.
    #[serde(default = "default_clip_max_duration")]
    pub max_duration_secs: u32,
    /// Byte cap on the MP4 (default: 64 MiB); the recording stops early and
    /// the clip says so.
    #[serde(default = "default_clip_max_bytes")]
    pub max_bytes: u64,
    /// Min gap between clips, seconds (default: 30).
    #[serde(default = "default_clip_cooldown")]
    pub cooldown_secs: u64,
    /// How long a clip stays downloadable, seconds (default: 600).
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
    /// zblob transfer chunk (default: 512 KiB).
    #[serde(default = "default_chunk")]
    pub chunk_size: u32,
}

fn default_still_quality() -> u8 {
    85
}
fn default_still_max_bytes() -> u64 {
    8 * 1024 * 1024
}
fn default_still_cooldown() -> u64 {
    5
}
fn default_clip_max_duration() -> u32 {
    60
}
fn default_clip_max_bytes() -> u64 {
    64 * 1024 * 1024
}
fn default_clip_cooldown() -> u64 {
    30
}
fn default_ttl() -> u64 {
    600
}
fn default_chunk() -> u32 {
    512 * 1024
}

impl Default for StillConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quality: default_still_quality(),
            max_height: None,
            max_bytes: default_still_max_bytes(),
            cooldown_secs: default_still_cooldown(),
            ttl_secs: default_ttl(),
            chunk_size: default_chunk(),
        }
    }
}

impl Default for ClipConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_duration_secs: default_clip_max_duration(),
            max_bytes: default_clip_max_bytes(),
            cooldown_secs: default_clip_cooldown(),
            ttl_secs: default_ttl(),
            chunk_size: default_chunk(),
        }
    }
}

impl StillConfig {
    /// The bounds the channel enforces and advertises.
    pub fn common(&self) -> CommonArtifactLimits {
        CommonArtifactLimits {
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            cooldown_secs: self.cooldown_secs,
            ttl_secs: self.ttl_secs,
            chunk_size: self.chunk_size,
        }
    }
}

impl ClipConfig {
    /// The bounds the channel enforces and advertises.
    pub fn common(&self) -> CommonArtifactLimits {
        CommonArtifactLimits {
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            cooldown_secs: self.cooldown_secs,
            ttl_secs: self.ttl_secs,
            chunk_size: self.chunk_size,
        }
    }
}

/// The catalogue streams a still or a clip can be taken from: every entry
/// that is not RTSP. An RTSP source needs the async connect + SDP path the
/// session actor runs (`session.rs`), which a one-shot pipeline does not
/// have yet — a follow-up, refused by name in `accepts`.
fn producible_streams(catalog: &Catalog) -> Vec<String> {
    catalog
        .entries()
        .iter()
        .filter(|e| !matches!(e.kind, SourceKind::Rtsp { .. }))
        .map(|e| e.name.clone())
        .collect()
}

/// Validate a `Still` request against the catalogue (pure).
pub fn validate_still(kind: &ArtifactKind, catalog: &Catalog) -> Result<(), RpcError> {
    let ArtifactKind::Still { stream } = kind else {
        return Err(RpcError::invalid_args(
            "still producer given a non-still request",
        ));
    };
    validate_stream(stream, catalog)
}

/// Validate a `Clip` request against the catalogue and the tier ladder
/// (pure). Clamping the duration is [`clamp_clip`]'s job in `produce`.
pub fn validate_clip(
    kind: &ArtifactKind,
    catalog: &Catalog,
    video: &VideoConfig,
) -> Result<(), RpcError> {
    let ArtifactKind::Clip {
        stream,
        duration_secs,
        tier,
    } = kind
    else {
        return Err(RpcError::invalid_args(
            "clip producer given a non-clip request",
        ));
    };
    if *duration_secs == 0 {
        return Err(RpcError::invalid_args("clip duration must be > 0"));
    }
    if let Some(tier) = tier
        && !video.tiers.iter().any(|t| &t.spec.name == tier)
    {
        let offered: Vec<&str> = video.tiers.iter().map(|t| t.spec.name.as_str()).collect();
        return Err(RpcError::invalid_args(format!(
            "tier {tier:?} is not on this sensor's ladder ({})",
            offered.join(", ")
        )));
    }
    validate_stream(stream, catalog)
}

fn validate_stream(stream: &str, catalog: &Catalog) -> Result<(), RpcError> {
    match catalog.get(stream) {
        None => Err(RpcError::not_found(format!(
            "stream {stream:?} is not in the catalogue"
        ))),
        Some(entry) if matches!(entry.kind, SourceKind::Rtsp { .. }) => {
            Err(RpcError::unsupported(format!(
                "stills and clips of RTSP stream {stream:?} are a follow-up: the one-shot \
                 pipeline has no async connect + SDP path yet"
            )))
        }
        Some(_) => Ok(()),
    }
}

/// The effective clip bounds after the configured limits (never trust the
/// request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamped {
    pub duration_secs: u32,
    pub max_bytes: u64,
    /// Whether the request asked for more than the config allows.
    pub clamped: bool,
}

/// Clamp a `Clip` request's duration to the configured max.
pub fn clamp_clip(kind: &ArtifactKind, cfg: &ClipConfig) -> Clamped {
    let asked = match kind {
        ArtifactKind::Clip { duration_secs, .. } => *duration_secs,
        _ => 0,
    };
    let duration_secs = asked.min(cfg.max_duration_secs).max(1);
    Clamped {
        duration_secs,
        max_bytes: cfg.max_bytes,
        clamped: asked > cfg.max_duration_secs,
    }
}

/// An exclusive device (V4L2) serves one pipeline at a time: a stream that
/// is already open for a viewer cannot also feed a one-shot pipeline, and
/// waiting for it to close would hold the kind's busy slot with nothing to
/// show. Test sources are shareable, so this never fires for them; RTSP is
/// refused earlier, at `accepts`.
pub fn refuse_if_exclusive_and_open(
    kind: &SourceKind,
    open: &HashSet<String>,
    stream: &str,
) -> anyhow::Result<()> {
    if kind.is_exclusive() && open.contains(stream) {
        anyhow::bail!(
            "stream {stream} is open on an exclusive device; close it first, then ask again"
        );
    }
    Ok(())
}

fn created_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Start a built pipeline, run `body` against its sink, then stop it —
/// whatever `body` returned. The stop is asked first and awaited second,
/// which is the clean-teardown order `pipeline.rs`'s tests pin (#709).
async fn with_running<T>(
    mut built: BuiltPipeline,
    body: impl AsyncFnOnce(&parallax::elements::AppSinkHandle, u32, u32) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let handle = pipeline::executor()
        .start(&mut built.pipeline)
        .map_err(|e| anyhow::anyhow!("start pipeline: {e}"))?;
    let sink = built.sink.clone();
    let result = body(&sink, built.width, built.height).await;
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle.wait()).await;
    result
}

/// One JPEG frame of a stream.
pub struct StillProducer {
    common: CommonArtifactLimits,
    cfg: StillConfig,
    catalog: Arc<Catalog>,
    session: SessionHandle,
    source: String,
}

impl StillProducer {
    pub fn new(
        cfg: &StillConfig,
        catalog: Arc<Catalog>,
        session: SessionHandle,
        source: impl Into<String>,
    ) -> Self {
        Self {
            common: cfg.common(),
            cfg: cfg.clone(),
            catalog,
            session,
            source: source.into(),
        }
    }
}

/// How long a still waits for its first frame before giving up: a test
/// source answers in milliseconds, a camera within a second or two.
const STILL_FIRST_FRAME: Duration = Duration::from_secs(10);

#[async_trait]
impl ArtifactProducer for StillProducer {
    fn kind(&self) -> &'static str {
        "still"
    }
    fn common(&self) -> &CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Blob
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::Still {
            streams: producible_streams(&self.catalog),
        }
    }
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), RpcError> {
        validate_still(kind, &self.catalog)
    }

    async fn produce(&self, kind: ArtifactKind, ctx: ProduceCtx) -> anyhow::Result<Produced> {
        let ArtifactKind::Still { stream } = kind else {
            anyhow::bail!("still producer given a non-still request");
        };
        let entry = self
            .catalog
            .get(&stream)
            .ok_or_else(|| anyhow::anyhow!("stream {stream} left the catalogue"))?;
        refuse_if_exclusive_and_open(&entry.kind, &self.session.open_streams().await, &stream)?;
        let preview = PreviewConfig {
            fps: 5,
            quality: self.cfg.quality,
            max_height: self.cfg.max_height,
        };
        let built =
            pipeline::build_preview(&entry.kind, &preview, &Arc::new(StreamStats::default()))?;
        let max_bytes = self.cfg.max_bytes;
        let cancel = ctx.cancel.clone();
        let data = with_running(built, async |sink, _w, _h| {
            let deadline = tokio::time::Instant::now() + STILL_FIRST_FRAME;
            loop {
                if cancel.is_cancelled() {
                    anyhow::bail!("cancelled");
                }
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "no frame from {stream} within {}s",
                        STILL_FIRST_FRAME.as_secs()
                    );
                }
                match sink.pull_buffer_timeout(Duration::from_millis(500)).await {
                    Pulled::Buffer(buf) => {
                        let bytes = buf.as_bytes();
                        if bytes.len() as u64 > max_bytes {
                            anyhow::bail!(
                                "the frame is {} bytes, over the configured {} byte cap",
                                bytes.len(),
                                max_bytes
                            );
                        }
                        return Ok(bytes.to_vec());
                    }
                    Pulled::Ended(_) => anyhow::bail!("the pipeline ended before a frame"),
                    Pulled::Empty | Pulled::Flushing => {}
                }
            }
        })
        .await?;
        let _ = ctx.progress.send(ProgressUpdate {
            detail: Some(format!("{} bytes", data.len())),
            progress: Some(1.0),
        });
        Ok(Produced::Bytes {
            data,
            filename: format!("still-{}-{stream}-{}.jpg", self.source, created_ms()),
        })
    }
}

/// A bounded H.264 recording of a stream, in an MP4.
pub struct ClipProducer {
    common: CommonArtifactLimits,
    cfg: ClipConfig,
    video: VideoConfig,
    catalog: Arc<Catalog>,
    session: SessionHandle,
    source: String,
}

impl ClipProducer {
    pub fn new(
        cfg: &ClipConfig,
        video: &VideoConfig,
        catalog: Arc<Catalog>,
        session: SessionHandle,
        source: impl Into<String>,
    ) -> Self {
        Self {
            common: cfg.common(),
            cfg: cfg.clone(),
            video: video.clone(),
            catalog,
            session,
            source: source.into(),
        }
    }
}

/// The outcome of one record loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipOutcome {
    /// Access units written.
    pub frames: u64,
    /// Bytes handed to the muxer (the file is a little larger: boxes).
    pub written: u64,
    /// True if the byte cap stopped the clip before its duration elapsed.
    pub truncated: bool,
}

/// Pull access units from a running video pipeline into an MP4 at `path`
/// until the duration, the byte cap, or cancellation — netring's
/// `capture_to_file` shape. The muxer opens on the first entry point (an
/// IDR with its parameter sets), so a clip always starts decodable.
async fn record_clip(
    sink: &parallax::elements::AppSinkHandle,
    width: u32,
    height: u32,
    path: &std::path::Path,
    fps: u32,
    clamped: &Clamped,
    ctx: &ProduceCtx,
) -> anyhow::Result<ClipOutcome> {
    use std::io::Write;

    let dur = u64::from(clamped.duration_secs.max(1));
    let cap = clamped.max_bytes;
    let fps = fps.max(1);
    let frame_ms = 1000 / fps;

    let start = tokio::time::Instant::now();
    let deadline = start + Duration::from_secs(dur);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_progress = 0u64;

    let mut mux: Option<(Mp4Mux<std::io::BufWriter<std::fs::File>>, u32)> = None;
    let mut frames = 0u64;
    let mut written = 0u64;
    let mut truncated = false;

    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break,
            pulled = sink.pull_buffer_timeout(Duration::from_millis(250)) => match pulled {
                Pulled::Buffer(buf) => {
                    let data = buf.as_bytes();
                    let keyframe = buf.metadata().is_keyframe();
                    if mux.is_none() {
                        // Skip until a decoder could start here.
                        if !is_entry_point(data, NalCodec::H264) {
                            continue;
                        }
                        let (Some(sps), Some(pps)) = extract_param_sets(data) else {
                            continue;
                        };
                        let file = std::fs::File::create(path)?;
                        let mut m = Mp4Mux::new(std::io::BufWriter::new(file), Mp4MuxConfig::h264())
                            .map_err(|e| anyhow::anyhow!("mp4 open: {e}"))?;
                        let track = m
                            .add_video_track(Mp4VideoTrackConfig::h264(
                                width as u16,
                                height as u16,
                                &sps,
                                &pps,
                            ))
                            .map_err(|e| anyhow::anyhow!("mp4 track: {e}"))?;
                        mux = Some((m, track));
                    }
                    let (m, track) = mux.as_mut().expect("opened above");
                    let avcc = annex_b_to_avcc(data);
                    m.write_video_sample(*track, &avcc, frames * u64::from(frame_ms), frame_ms, keyframe)
                        .map_err(|e| anyhow::anyhow!("mp4 write: {e}"))?;
                    frames += 1;
                    written += avcc.len() as u64;
                    if written >= cap {
                        truncated = true;
                        break;
                    }
                }
                Pulled::Ended(_) => break,
                Pulled::Empty | Pulled::Flushing => {}
            },
            _ = tick.tick() => {
                if ctx.cancel.is_cancelled() {
                    anyhow::bail!("cancelled");
                }
                let elapsed_s = start.elapsed().as_secs();
                if elapsed_s != last_progress {
                    last_progress = elapsed_s;
                    let _ = ctx.progress.send(ProgressUpdate {
                        detail: Some(format!(
                            "recording {elapsed_s}s/{dur}s · {:.1} MiB · {frames} frames",
                            written as f64 / (1024.0 * 1024.0),
                        )),
                        progress: Some((elapsed_s as f32 / dur as f32).min(1.0)),
                    });
                }
            }
        }
    }

    let Some((m, _)) = mux else {
        anyhow::bail!("no decodable frame from the stream within {dur}s");
    };
    let mut writer = m.finish().map_err(|e| anyhow::anyhow!("mp4 finish: {e}"))?;
    writer.flush()?;
    Ok(ClipOutcome {
        frames,
        written,
        truncated,
    })
}

#[async_trait]
impl ArtifactProducer for ClipProducer {
    fn kind(&self) -> &'static str {
        "clip"
    }
    fn common(&self) -> &CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Blob
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::Clip {
            streams: producible_streams(&self.catalog),
            max_duration_secs: self.cfg.max_duration_secs,
            tiers: self
                .video
                .tiers
                .iter()
                .map(|t| t.spec.name.clone())
                .collect(),
        }
    }
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), RpcError> {
        validate_clip(kind, &self.catalog, &self.video)
    }

    async fn produce(&self, kind: ArtifactKind, ctx: ProduceCtx) -> anyhow::Result<Produced> {
        let clamped = clamp_clip(&kind, &self.cfg);
        let ArtifactKind::Clip { stream, tier, .. } = kind else {
            anyhow::bail!("clip producer given a non-clip request");
        };
        let entry = self
            .catalog
            .get(&stream)
            .ok_or_else(|| anyhow::anyhow!("stream {stream} left the catalogue"))?;
        refuse_if_exclusive_and_open(&entry.kind, &self.session.open_streams().await, &stream)?;
        let tier_name = tier.unwrap_or_else(|| self.video.default_tier.clone());
        let tier_cfg = self
            .video
            .tiers
            .iter()
            .find(|t| t.spec.name == tier_name)
            .ok_or_else(|| anyhow::anyhow!("tier {tier_name} is not on the ladder"))?;
        let params = pipeline::tier_video_params(&self.video, tier_cfg);
        let built = pipeline::build_video(&entry.kind, &params, &Arc::new(StreamStats::default()))?;
        let path = ctx.workdir.join(format!(
            "clip-{}-{stream}-{tier_name}-{}.mp4",
            self.source,
            created_ms()
        ));
        let outcome = with_running(built, async |sink, w, h| {
            record_clip(sink, w, h, &path, params.fps, &clamped, &ctx).await
        })
        .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        let mut summary = format!(
            "recorded {} frames · {:.1} MiB",
            outcome.frames,
            outcome.written as f64 / (1024.0 * 1024.0)
        );
        if outcome.truncated {
            summary.push_str(" · truncated at the byte cap");
        }
        if clamped.clamped {
            summary.push_str(&format!(" · clamped to {}s", clamped.duration_secs));
        }
        if outcome.truncated || clamped.clamped {
            ctx.note(summary.clone());
        }
        let _ = ctx.progress.send(ProgressUpdate {
            detail: Some(summary),
            progress: Some(1.0),
        });
        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "clip.mp4".to_string());
        Ok(Produced::File { path, filename })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ParallaxConfig;
    use tokio::sync::watch;
    use zblob::CancelToken;

    fn cfg() -> ParallaxConfig {
        json5::from_str(
            r#"{ enumerate_v4l2: false,
                 test_sources: [{ name: "test0", pattern: "smpte", width: 320, height: 240, fps: 10 }],
                 rtsp: [{ name: "door", url: "rtsp://10.0.0.7/s" }],
                 video: { tiers: [{ name: "low", max_height: 120, fps: 10, bitrate_kbps: 300 }],
                          default_tier: "low" } }"#,
        )
        .unwrap()
    }

    fn produce_ctx(dir: &tempfile::TempDir) -> (ProduceCtx, CancelToken) {
        let cancel = CancelToken::new();
        let (progress, _rx) = watch::channel(ProgressUpdate::default());
        (
            ProduceCtx {
                workdir: dir.path().to_path_buf(),
                cancel: cancel.clone(),
                progress,
                note: Arc::default(),
            },
            cancel,
        )
    }

    /// A session actor with nothing open, so the exclusivity check has an
    /// answer; the test source never needs the actor otherwise.
    async fn session() -> SessionHandle {
        let config = cfg();
        let catalog = Arc::new(Catalog::build(&config));
        let mut zc = zenoh::Config::default();
        zc.insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        zc.insert_json5("scouting/gossip/enabled", "false").unwrap();
        let session = Arc::new(zenoh::open(zc).await.unwrap());
        crate::session::SessionManager::spawn(
            catalog,
            config,
            "s".to_string(),
            zensight_sensor_core::Publisher::new(
                session,
                "parallax",
                zensight_common::Format::Json,
            ),
            crate::stats::StatsRegistry::default(),
            None,
            None,
        )
    }

    #[test]
    fn clip_clamp_over_limit_reduces_within_untouched() {
        let c = ClipConfig {
            max_duration_secs: 20,
            ..Default::default()
        };
        let over = clamp_clip(
            &ArtifactKind::Clip {
                stream: "test0".into(),
                duration_secs: 90,
                tier: None,
            },
            &c,
        );
        assert_eq!(over.duration_secs, 20);
        assert!(over.clamped);
        let within = clamp_clip(
            &ArtifactKind::Clip {
                stream: "test0".into(),
                duration_secs: 5,
                tier: None,
            },
            &c,
        );
        assert_eq!(within.duration_secs, 5);
        assert!(!within.clamped);
    }

    #[test]
    fn accepts_matrix() {
        let config = cfg();
        let catalog = Catalog::build(&config);
        let still = |s: &str| ArtifactKind::Still { stream: s.into() };
        assert!(validate_still(&still("test0"), &catalog).is_ok());
        assert!(validate_still(&still("nope"), &catalog).is_err());
        assert!(
            validate_still(&still("door"), &catalog).is_err(),
            "RTSP is a follow-up"
        );
        assert!(validate_still(&ArtifactKind::Report {}, &catalog).is_err());
        let clip = |d: u32, t: Option<&str>| ArtifactKind::Clip {
            stream: "test0".into(),
            duration_secs: d,
            tier: t.map(str::to_string),
        };
        assert!(validate_clip(&clip(3, None), &catalog, &config.video).is_ok());
        assert!(validate_clip(&clip(3, Some("low")), &catalog, &config.video).is_ok());
        assert!(validate_clip(&clip(0, None), &catalog, &config.video).is_err());
        assert!(validate_clip(&clip(3, Some("ultra")), &catalog, &config.video).is_err());
    }

    #[test]
    fn exclusive_open_stream_is_refused_test_source_is_not() {
        let open: HashSet<String> = ["video0".to_string()].into_iter().collect();
        let v4l2 = SourceKind::V4l2 {
            device: "/dev/video0".into(),
        };
        assert!(refuse_if_exclusive_and_open(&v4l2, &open, "video0").is_err());
        assert!(refuse_if_exclusive_and_open(&v4l2, &open, "video1").is_ok());
        let test = SourceKind::Test {
            pattern: "smpte".into(),
            width: 320,
            height: 240,
            fps: 10,
        };
        assert!(refuse_if_exclusive_and_open(&test, &open, "video0").is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn still_from_a_test_source_is_a_jpeg_at_the_capped_size() {
        let config = cfg();
        let catalog = Arc::new(Catalog::build(&config));
        let still = StillConfig {
            enabled: true,
            max_height: Some(120),
            ..Default::default()
        };
        let producer = StillProducer::new(&still, catalog, session().await, "s");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _cancel) = produce_ctx(&dir);
        let produced = producer
            .produce(
                ArtifactKind::Still {
                    stream: "test0".into(),
                },
                ctx,
            )
            .await
            .expect("a still");
        let Produced::Bytes { data, filename } = produced else {
            panic!("a still is a blob of bytes");
        };
        assert!(filename.ends_with(".jpg"), "{filename}");
        assert_eq!(&data[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(pipeline::jpeg_sof_dimensions(&data), Some((160, 120)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn still_over_the_byte_cap_fails() {
        let config = cfg();
        let catalog = Arc::new(Catalog::build(&config));
        let still = StillConfig {
            enabled: true,
            max_bytes: 16,
            ..Default::default()
        };
        let producer = StillProducer::new(&still, catalog, session().await, "s");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _cancel) = produce_ctx(&dir);
        let err = producer
            .produce(
                ArtifactKind::Still {
                    stream: "test0".into(),
                },
                ctx,
            )
            .await;
        let Err(err) = err else {
            panic!("a frame over the cap must fail");
        };
        assert!(err.to_string().contains("byte cap"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clip_from_a_test_source_is_a_demuxable_mp4() {
        let config = cfg();
        let catalog = Arc::new(Catalog::build(&config));
        let clip = ClipConfig {
            enabled: true,
            ..Default::default()
        };
        let producer = ClipProducer::new(&clip, &config.video, catalog, session().await, "s");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _cancel) = produce_ctx(&dir);
        let produced = producer
            .produce(
                ArtifactKind::Clip {
                    stream: "test0".into(),
                    duration_secs: 2,
                    tier: None,
                },
                ctx,
            )
            .await
            .expect("a clip");
        let Produced::File { path, filename } = produced else {
            panic!("a clip is a file");
        };
        assert!(filename.ends_with(".mp4"), "{filename}");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[4..8],
            b"ftyp",
            "an MP4 starts with its file-type box"
        );
        let file = std::fs::File::open(&path).unwrap();
        let len = file.metadata().unwrap().len();
        let mut demux = parallax::elements::demux::Mp4Demux::new(file, len).expect("demux");
        let tracks = demux.tracks().to_vec();
        assert_eq!(tracks.len(), 1, "one video track");
        let samples = demux.read_all_samples(tracks[0].id).expect("samples");
        // The encoder's cold start eats the first part of a short clip; a
        // live 10 fps source still leaves several frames in two seconds.
        assert!(samples.len() >= 3, "2s at 10 fps: {}", samples.len());
        assert!(samples[0].is_keyframe, "a clip starts on an entry point");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clip_stops_at_the_byte_cap_and_notes_it() {
        let config = cfg();
        let catalog = Arc::new(Catalog::build(&config));
        // A static test pattern encodes to near-empty delta frames (tens of
        // bytes at 300 kbps), so the cap has to be under the first keyframe
        // to bite at all — and a cap under the first frame still yields a
        // playable one-frame clip, which is the point.
        let clip = ClipConfig {
            enabled: true,
            max_bytes: 200,
            ..Default::default()
        };
        let producer = ClipProducer::new(&clip, &config.video, catalog, session().await, "s");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _cancel) = produce_ctx(&dir);
        let note = ctx.note.clone();
        let started = std::time::Instant::now();
        let produced = producer
            .produce(
                ArtifactKind::Clip {
                    stream: "test0".into(),
                    duration_secs: 30,
                    tier: None,
                },
                ctx,
            )
            .await
            .expect("a truncated clip is still a clip");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the cap stops it early"
        );
        let Produced::File { path, .. } = produced else {
            panic!("a clip is a file");
        };
        let noted = note.lock().unwrap().clone().unwrap_or_default();
        assert!(noted.contains("truncated"), "{noted}");
        let file = std::fs::File::open(&path).unwrap();
        let len = file.metadata().unwrap().len();
        let demux = parallax::elements::demux::Mp4Demux::new(file, len).expect("moov written");
        assert_eq!(demux.tracks().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clip_cancel_stops_and_errs() {
        let config = cfg();
        let catalog = Arc::new(Catalog::build(&config));
        let clip = ClipConfig {
            enabled: true,
            ..Default::default()
        };
        let producer = ClipProducer::new(&clip, &config.video, catalog, session().await, "s");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, cancel) = produce_ctx(&dir);
        cancel.cancel();
        let err = producer
            .produce(
                ArtifactKind::Clip {
                    stream: "test0".into(),
                    duration_secs: 10,
                    tier: None,
                },
                ctx,
            )
            .await;
        let Err(err) = err else {
            panic!("a cancelled clip must fail");
        };
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "no partial left"
        );
    }
}
