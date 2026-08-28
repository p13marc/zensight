//! Configuration for the parallax video sensor.

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use zensight_common::stream::TierSpec;
use zensight_sensor_core::{SensorConfig, SensorError, ZenohConfig};

// Re-export LoggingConfig from the framework (they're compatible).
pub use zensight_sensor_core::LoggingConfig;

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    Parse(#[from] json5::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

/// Complete sensor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallaxSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Video source / stream settings.
    pub parallax: ParallaxConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// On-demand artifact channel (`@rpc/parallax/artifact/*`) limits — report + snapshot.
    /// Every kind disabled by default.
    #[serde(default)]
    pub artifacts: zensight_sensor_core::ArtifactLimits,
}

/// Video source and stream configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallaxConfig {
    /// Source id (hostname) to use in key expressions.
    /// Use "auto" to detect the local hostname automatically (default).
    #[serde(default = "default_source")]
    pub source: String,

    /// Enumerate local V4L2 cameras (`/dev/video*`) into the stream catalogue
    /// (default: true). Headless hosts simply contribute nothing.
    #[serde(default = "default_true")]
    pub enumerate_v4l2: bool,

    /// Remote RTSP cameras to advertise.
    #[serde(default)]
    pub rtsp: Vec<RtspSourceConfig>,

    /// Synthetic test-pattern sources (`VideoTestSrc`). Same catalogue /
    /// command / encode / egress path as real cameras — doubles as demo mode
    /// and lets the sensor stream on any machine.
    #[serde(default)]
    pub test_sources: Vec<TestSourceConfig>,

    /// JPEG preview profile (low-fps thumbnails for the GUI tile grid).
    #[serde(default)]
    pub preview: PreviewConfig,

    /// H.264 video profile (full-rate live view).
    #[serde(default)]
    pub video: VideoConfig,

    /// Tear an open profile down after this many seconds with no viewers and
    /// no explicit opens (crash backstop for GUIs that die without CloseStream).
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,

    /// Interval between per-stream stats telemetry points (fps/kbps/drops).
    #[serde(default = "default_stats_interval")]
    pub stats_interval_secs: u64,
}

impl Default for ParallaxConfig {
    fn default() -> Self {
        Self {
            source: default_source(),
            enumerate_v4l2: true,
            rtsp: Vec::new(),
            test_sources: Vec::new(),
            preview: PreviewConfig::default(),
            video: VideoConfig::default(),
            idle_timeout_secs: default_idle_timeout(),
            stats_interval_secs: default_stats_interval(),
        }
    }
}

/// One remote RTSP camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtspSourceConfig {
    /// Stream identifier (the `<stream>` key chunk) — must be unique.
    pub name: String,
    /// RTSP URL (e.g. `rtsp://cam.local:554/stream1`).
    pub url: String,
    /// Optional credentials.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Optional human-readable description (camera position, …).
    #[serde(default)]
    pub description: Option<String>,
}

/// One synthetic test-pattern source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSourceConfig {
    /// Stream identifier (the `<stream>` key chunk) — must be unique.
    pub name: String,
    /// Test pattern: smpte / checkerboard / ball / gradient / snow / solid /
    /// black / white (default: smpte).
    #[serde(default = "default_pattern")]
    pub pattern: String,
    /// Frame width in pixels (default: 320).
    #[serde(default = "default_test_width")]
    pub width: u32,
    /// Frame height in pixels (default: 240).
    #[serde(default = "default_test_height")]
    pub height: u32,
    /// Frame rate (default: 15).
    #[serde(default = "default_test_fps")]
    pub fps: u32,
}

/// JPEG preview profile settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewConfig {
    /// Preview frame rate (default: 2 fps — thumbnails, not video).
    #[serde(default = "default_preview_fps")]
    pub fps: u32,
    /// JPEG quality 1–100 (default: 75).
    #[serde(default = "default_preview_quality")]
    pub quality: u8,
    /// Aspect-preserving height cap for the preview, in pixels (default: 360).
    /// A 1080p camera's thumbnail is a 1080p JPEG otherwise — ~30× the pixels a
    /// tile needs (#501). `None` = source size.
    #[serde(default = "default_preview_max_height")]
    pub max_height: Option<u32>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            fps: default_preview_fps(),
            quality: default_preview_quality(),
            max_height: default_preview_max_height(),
        }
    }
}

/// H.264 profile. Mirrors `parallax::elements::codec::Profile`, which carries no
/// serde derives — so the config vocabulary is ours, not the codec crate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum H264Profile {
    Baseline,
    Main,
    High,
}

/// Encoder CPU/quality trade. Mirrors `parallax::elements::codec::Complexity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderComplexity {
    Low,
    Medium,
    High,
}

/// What the encoder is being asked to encode. Mirrors
/// `parallax::elements::codec::UsageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderUsage {
    CameraRealtime,
    ScreenRealtime,
    CameraNonRealtime,
    ScreenNonRealtime,
}

/// Encoder knobs that shape *how* a tier reaches its advertised numbers (#509).
///
/// Deliberately **sensor-local**: unlike the resolution/framerate/bitrate in
/// [`TierSpec`], none of this is on the wire. `TierSpec` rides the catalogue
/// inside `StreamDescriptor`, which is a derived entry in the fleet-wide
/// `SchemaSet` every producer serves on `@rpc/<producer>/describe` (RFC 08 §7),
/// and a viewer picks a tier by the three numbers it can act on — never by
/// entropy coder. The sensor owns the numbers; the wire carries the name.
///
/// Every field is `None` = inherit `video.encoder`; an unset `video.encoder`
/// field means the corresponding parallax builder is **never called**, so an
/// unset knob is OpenH264's own default by construction rather than by our copy
/// of it. Everything ships unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderTuning {
    /// H.264 profile. Unset lets OpenH264 choose — see `docs/streams.md` for
    /// why pinning one needs a decode test first (the GUI decodes with
    /// OpenH264 too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<H264Profile>,
    /// CPU spent per frame. `low` is the answer to a firing `encoder_overrun`:
    /// cheaper than dropping resolution, and invisible to the receiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<EncoderComplexity>,
    /// Camera vs screen, realtime vs not. A property of the *source* rather
    /// than the tier — set it on `video.encoder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_type: Option<EncoderUsage>,
    /// Cap on each emitted NAL unit, in bytes (`None` = one slice per frame).
    /// See `docs/streams.md`: this buys nothing on today's whole-access-unit
    /// egress, and is here for a downstream RTP/WebRTC payloader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_slice_len: Option<u32>,
    /// Target quantiser (0..=51). Under `RateControlMode::Bitrate` with frame
    /// skipping on, the rate controller works in a ±4 band around it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qp: Option<u8>,
    /// Per-tier keyframe interval; `None` = `video.gop_frames`. A lossy low
    /// tier wants a short GOP (fast recovery, fast late-join); a high tier
    /// wants a long one (efficiency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gop_frames: Option<u32>,
}

impl EncoderTuning {
    /// Resolve one tier's tuning against the shared defaults: `self` wins,
    /// `base` fills the gaps, field by field.
    pub fn over(self, base: Self) -> Self {
        Self {
            profile: self.profile.or(base.profile),
            complexity: self.complexity.or(base.complexity),
            usage_type: self.usage_type.or(base.usage_type),
            max_slice_len: self.max_slice_len.or(base.max_slice_len),
            qp: self.qp.or(base.qp),
            gop_frames: self.gop_frames.or(base.gop_frames),
        }
    }
}

/// One rung of the ladder: the wire [`TierSpec`] a viewer picks by, flattened
/// alongside the sensor-local encoder shaping that never leaves this host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    #[serde(flatten)]
    pub spec: TierSpec,
    #[serde(default)]
    pub encoder: EncoderTuning,
}

/// H.264 video settings: a shared GOP plus the **tier ladder** — the sensor owns
/// the numbers, the wire and the `<tier>` key carry the name (#498). Each tier is
/// published concurrently on its own `@media/<stream>/video/h264/<tier>` key, so
/// two viewers on different links each subscribe to the tier their link can take
/// without fighting over one encoder (#494).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Keyframe (GOP) interval in frames, shared across tiers (default: 60).
    /// A tier's `encoder.gop_frames` overrides it.
    #[serde(default = "default_gop_frames")]
    pub gop_frames: u32,
    /// Encoder shaping shared by every tier; each tier's own `encoder` block
    /// overrides it field by field (#509). Ships entirely unset.
    #[serde(default)]
    pub encoder: EncoderTuning,
    /// The bandwidth tiers this sensor offers (default: low/medium/high).
    #[serde(default = "default_tiers")]
    pub tiers: Vec<TierConfig>,
    /// Which tier an `OpenStream` with no explicit tier resolves to.
    #[serde(default = "default_default_tier")]
    pub default_tier: String,
}

impl VideoConfig {
    /// The wire view of the ladder — what the catalogue advertises in
    /// `StreamDescriptor::tiers`. The encoder shaping stays behind.
    pub fn ladder(&self) -> Vec<TierSpec> {
        self.tiers.iter().map(|t| t.spec.clone()).collect()
    }

    /// One tier's tuning, resolved against the shared `video.encoder` block.
    pub fn tuning_for(&self, tier: &TierConfig) -> EncoderTuning {
        tier.encoder.over(self.encoder)
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            gop_frames: default_gop_frames(),
            encoder: EncoderTuning::default(),
            tiers: default_tiers(),
            default_tier: default_default_tier(),
        }
    }
}

fn default_source() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

fn default_pattern() -> String {
    "smpte".to_string()
}

fn default_test_width() -> u32 {
    320
}

fn default_test_height() -> u32 {
    240
}

fn default_test_fps() -> u32 {
    15
}

fn default_preview_fps() -> u32 {
    2
}

fn default_preview_quality() -> u8 {
    75
}

fn default_gop_frames() -> u32 {
    60
}

fn default_preview_max_height() -> Option<u32> {
    Some(360)
}

/// The default bandwidth ladder (#498). Bandwidth is ~linear in pixel count at
/// constant quality, so each rung down is a large, deliberate saving for a
/// constrained link.
///
/// Every rung ships with its encoder shaping **unset** (#509): the knobs exist
/// so an operator can reach for them, not so this file can guess. See
/// `docs/streams.md` for what each is for and when it pays.
fn default_tiers() -> Vec<TierConfig> {
    fn rung(name: &str, max_height: Option<u32>, fps: u32, bitrate_kbps: u32) -> TierConfig {
        TierConfig {
            spec: TierSpec {
                name: name.into(),
                max_height,
                fps,
                bitrate_kbps,
            },
            encoder: EncoderTuning::default(),
        }
    }
    vec![
        rung("low", Some(240), 10, 400),
        rung("medium", Some(480), 20, 1200),
        rung("high", None, 30, 4000),
    ]
}

fn default_default_tier() -> String {
    "medium".to_string()
}

fn default_idle_timeout() -> u64 {
    30
}

fn default_stats_interval() -> u64 {
    5
}

/// Numeric bounds on encoder shaping (#509). parallax clamps `qp` silently at
/// `.min(51)`; rejecting at load beats accepting a value that means something
/// other than what it says.
fn validate_tuning(what: &str, t: EncoderTuning) -> Result<(), ConfigError> {
    if let Some(n) = t.max_slice_len
        && !(200..=65_535).contains(&n)
    {
        return Err(ConfigError::Validation(format!(
            "{what}: encoder.max_slice_len must be 200..=65535 bytes \
             (below ~200 the slice header dominates the slice)"
        )));
    }
    if let Some(q) = t.qp
        && q > 51
    {
        return Err(ConfigError::Validation(format!(
            "{what}: encoder.qp must be 0..=51"
        )));
    }
    if t.gop_frames == Some(0) {
        return Err(ConfigError::Validation(format!(
            "{what}: encoder.gop_frames must be > 0"
        )));
    }
    Ok(())
}

impl ParallaxSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: ParallaxSensorConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let p = &self.parallax;

        // Configured stream names must be unique (V4L2 enumeration derives its
        // own names at runtime and disambiguates itself).
        let mut names = std::collections::HashSet::new();
        for name in p
            .rtsp
            .iter()
            .map(|r| &r.name)
            .chain(p.test_sources.iter().map(|t| &t.name))
        {
            if name.is_empty() {
                return Err(ConfigError::Validation(
                    "stream names must not be empty".to_string(),
                ));
            }
            if name.contains('/') || name.contains('*') {
                return Err(ConfigError::Validation(format!(
                    "stream name {name:?} must be a single key chunk (no '/' or '*')"
                )));
            }
            if !names.insert(name) {
                return Err(ConfigError::Validation(format!(
                    "duplicate stream name {name:?}"
                )));
            }
        }

        if p.preview.fps == 0 {
            return Err(ConfigError::Validation(
                "preview.fps must be > 0".to_string(),
            ));
        }
        if p.preview.quality == 0 || p.preview.quality > 100 {
            return Err(ConfigError::Validation(
                "preview.quality must be in 1..=100".to_string(),
            ));
        }
        for t in &p.test_sources {
            if t.fps == 0 {
                return Err(ConfigError::Validation(format!(
                    "test source {:?}: fps must be > 0",
                    t.name
                )));
            }
            if t.width == 0 || t.height == 0 {
                return Err(ConfigError::Validation(format!(
                    "test source {:?}: width/height must be > 0",
                    t.name
                )));
            }
        }
        if let Some(mh) = p.preview.max_height
            && mh < 2
        {
            return Err(ConfigError::Validation(
                "preview.max_height must be >= 2".to_string(),
            ));
        }
        if p.video.gop_frames == 0 {
            return Err(ConfigError::Validation(
                "video.gop_frames must be > 0".to_string(),
            ));
        }
        // The tier ladder (#498): non-empty, uniquely named single-chunk tiers,
        // each with a real fps/bitrate, and a default_tier that names one of them.
        if p.video.tiers.is_empty() {
            return Err(ConfigError::Validation(
                "video.tiers must not be empty".to_string(),
            ));
        }
        let mut tier_names = std::collections::HashSet::new();
        for t in &p.video.tiers {
            let name = &t.spec.name;
            if name.is_empty() || name.contains('/') || name.contains('*') {
                return Err(ConfigError::Validation(format!(
                    "tier name {name:?} must be a single key chunk (no '/' or '*')"
                )));
            }
            if !tier_names.insert(name) {
                return Err(ConfigError::Validation(format!(
                    "duplicate tier name {name:?}"
                )));
            }
            if t.spec.fps == 0 {
                return Err(ConfigError::Validation(format!(
                    "tier {name:?}: fps must be > 0"
                )));
            }
            if t.spec.bitrate_kbps == 0 {
                return Err(ConfigError::Validation(format!(
                    "tier {name:?}: bitrate_kbps must be > 0"
                )));
            }
            if let Some(mh) = t.spec.max_height
                && mh < 2
            {
                return Err(ConfigError::Validation(format!(
                    "tier {name:?}: max_height must be >= 2"
                )));
            }
            // Encoder shaping (#509), validated on the *resolved* tuning so a
            // bad shared default is caught even when no tier overrides it. Bad
            // enum spellings are already a parse error naming the field.
            validate_tuning(&format!("tier {name:?}"), p.video.tuning_for(t))?;
        }
        validate_tuning("video.encoder", p.video.encoder)?;
        if !p
            .video
            .tiers
            .iter()
            .any(|t| t.spec.name == p.video.default_tier)
        {
            return Err(ConfigError::Validation(format!(
                "video.default_tier {:?} names no tier in the ladder",
                p.video.default_tier
            )));
        }
        if p.idle_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "idle_timeout_secs must be > 0".to_string(),
            ));
        }
        if p.stats_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "stats_interval_secs must be > 0".to_string(),
            ));
        }

        Ok(())
    }

    /// Get the source id to use, resolving "auto" to the local hostname.
    pub fn resolved_source(&self) -> String {
        if self.parallax.source == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.parallax.source.clone()
        }
    }
}

/// Implement SensorConfig trait for framework integration.
impl SensorConfig for ParallaxSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &str {
        "parallax"
    }

    fn artifact_limits(&self) -> zensight_sensor_core::ArtifactLimits {
        self.artifacts.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        Self::validate(self).map_err(|e| SensorError::validation(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            parallax: {}
        }"#;
        let config: ParallaxSensorConfig = json5::from_str(json).unwrap();
        config.validate().unwrap();
        assert_eq!(config.parallax.source, "auto");
        assert!(config.parallax.enumerate_v4l2);
        assert!(config.parallax.rtsp.is_empty());
        assert!(config.parallax.test_sources.is_empty());
        assert_eq!(config.parallax.preview.fps, 2);
        assert_eq!(config.parallax.preview.quality, 75);
        assert_eq!(config.parallax.video.gop_frames, 60);
        // Default tier ladder: low / medium / high, defaulting to medium.
        let tier_names: Vec<&str> = config
            .parallax
            .video
            .tiers
            .iter()
            .map(|t| t.spec.name.as_str())
            .collect();
        assert_eq!(tier_names, vec!["low", "medium", "high"]);
        assert_eq!(config.parallax.video.default_tier, "medium");
        assert_eq!(config.parallax.idle_timeout_secs, 30);
        assert_eq!(config.parallax.stats_interval_secs, 5);
    }

    #[test]
    fn parse_full_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            parallax: {
                source: "gateway01",
                enumerate_v4l2: false,
                rtsp: [
                    { name: "door", url: "rtsp://cam.local/stream1",
                      username: "viewer", password: "hunter2",
                      description: "front door" },
                ],
                test_sources: [
                    { name: "test0", pattern: "ball", width: 640, height: 360, fps: 10 },
                ],
                preview: { fps: 4, quality: 60 },
                video: {
                    gop_frames: 30,
                    default_tier: "low",
                    tiers: [
                        { name: "low",  max_height: 240, fps: 10, bitrate_kbps: 500 },
                        { name: "full", max_height: null, fps: 30, bitrate_kbps: 4000 },
                    ],
                },
                idle_timeout_secs: 10,
                stats_interval_secs: 2,
            }
        }"#;
        let config: ParallaxSensorConfig = json5::from_str(json).unwrap();
        config.validate().unwrap();
        assert_eq!(config.resolved_source(), "gateway01");
        assert!(!config.parallax.enumerate_v4l2);
        assert_eq!(config.parallax.rtsp[0].name, "door");
        assert_eq!(config.parallax.rtsp[0].username.as_deref(), Some("viewer"));
        let t = &config.parallax.test_sources[0];
        assert_eq!((t.width, t.height, t.fps), (640, 360, 10));
        assert_eq!(t.pattern, "ball");
        assert_eq!(config.parallax.preview.fps, 4);
        assert_eq!(config.parallax.video.gop_frames, 30);
        assert_eq!(config.parallax.video.default_tier, "low");
        assert_eq!(config.parallax.video.tiers.len(), 2);
        assert_eq!(config.parallax.video.tiers[1].spec.bitrate_kbps, 4000);
    }

    #[test]
    fn validate_rejects_duplicate_names() {
        let json = r#"{
            zenoh: { mode: "peer" },
            parallax: {
                rtsp: [ { name: "cam0", url: "rtsp://a/1" } ],
                test_sources: [ { name: "cam0" } ],
            }
        }"#;
        let config: ParallaxSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }

    /// The precedence rule the whole tuning design rests on: a tier's own
    /// `encoder` block wins field by field, `video.encoder` fills the gaps, and
    /// a field neither sets stays `None` — which is what makes "unset means
    /// OpenH264's default" true by construction rather than by our copy of it.
    #[test]
    fn tier_encoder_overrides_the_shared_defaults() {
        let config: ParallaxSensorConfig = json5::from_str(
            r#"{
                zenoh: { mode: "peer" },
                parallax: {
                    video: {
                        encoder: { complexity: "medium", qp: 26, profile: "main" },
                        default_tier: "low",
                        tiers: [
                            { name: "low", fps: 10, bitrate_kbps: 400,
                              encoder: { complexity: "low", max_slice_len: 1200 } },
                            { name: "high", fps: 30, bitrate_kbps: 4000 },
                        ],
                    },
                },
            }"#,
        )
        .expect("parse");
        config.validate().expect("valid");

        let video = &config.parallax.video;
        let low = video.tuning_for(&video.tiers[0]);
        assert_eq!(low.complexity, Some(EncoderComplexity::Low), "tier wins");
        assert_eq!(low.profile, Some(H264Profile::Main), "shared fills the gap");
        assert_eq!(low.qp, Some(26), "shared fills the gap");
        assert_eq!(low.max_slice_len, Some(1200), "tier-only field");
        assert_eq!(low.usage_type, None, "neither set it — parallax decides");

        let high = video.tuning_for(&video.tiers[1]);
        assert_eq!(high.complexity, Some(EncoderComplexity::Medium));
        assert_eq!(
            high.max_slice_len, None,
            "not inherited from a sibling tier"
        );

        // The wire ladder carries the three numbers a viewer picks by, and
        // none of the shaping (#509).
        let ladder = video.ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].name, "low");
        assert_eq!(ladder[0].bitrate_kbps, 400);
    }

    /// The shipped example must load — it is the file `just parallax` and every
    /// deployment start from, and nothing else in CI opens it. (Precedent:
    /// `zensight-sensor-logs` guards `configs/logs.json5` the same way.)
    #[test]
    fn shipped_config_loads_and_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/parallax.json5");
        let config = ParallaxSensorConfig::load_from_file(path).expect("configs/parallax.json5");
        let video = &config.parallax.video;
        assert_eq!(
            video.ladder().len(),
            3,
            "the shipped ladder is low / medium / high"
        );
        // #509's shaping knobs ship commented out; if one is ever uncommented
        // here, that is a deliberate default and this assertion should move.
        assert_eq!(
            video.encoder,
            EncoderTuning::default(),
            "configs/parallax.json5 must ship with no encoder shaping set"
        );
    }

    /// Everything ships unset, so a default build's encoder config is exactly
    /// what it was before #509.
    #[test]
    fn shipped_tiers_set_no_encoder_shaping() {
        let video = VideoConfig::default();
        assert_eq!(video.encoder, EncoderTuning::default());
        for tier in &video.tiers {
            assert_eq!(
                video.tuning_for(tier),
                EncoderTuning::default(),
                "tier {:?} must ship with no shaping",
                tier.spec.name
            );
        }
    }

    #[test]
    fn validate_rejects_bad_encoder_shaping() {
        for bad in [
            // qp is clamped silently by parallax; reject rather than accept a
            // value that means something other than what it says.
            r#"{ zenoh: { mode: "peer" }, parallax: { video: { tiers: [
                 { name: "low", fps: 10, bitrate_kbps: 400, encoder: { qp: 52 } } ],
                 default_tier: "low" } } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { video: { tiers: [
                 { name: "low", fps: 10, bitrate_kbps: 400, encoder: { max_slice_len: 64 } } ],
                 default_tier: "low" } } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { video: { tiers: [
                 { name: "low", fps: 10, bitrate_kbps: 400, encoder: { gop_frames: 0 } } ],
                 default_tier: "low" } } }"#,
            // A bad shared default is caught even when no tier overrides it.
            r#"{ zenoh: { mode: "peer" }, parallax: { video: {
                 encoder: { max_slice_len: 99999 } } } }"#,
        ] {
            let config: ParallaxSensorConfig = json5::from_str(bad).expect("parses");
            assert!(
                config.validate().is_err(),
                "should have been rejected: {bad}"
            );
        }

        // An unrecognised enum spelling fails at parse, naming the field.
        assert!(
            json5::from_str::<ParallaxSensorConfig>(
                r#"{ zenoh: { mode: "peer" }, parallax: { video: {
                     encoder: { profile: "ultra" } } } }"#
            )
            .is_err(),
            "profile must be one of baseline/main/high"
        );
    }

    #[test]
    fn validate_rejects_bad_values() {
        for bad in [
            r#"{ zenoh: { mode: "peer" }, parallax: { preview: { fps: 0 } } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { preview: { quality: 101 } } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { test_sources: [ { name: "t", fps: 0 } ] } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { test_sources: [ { name: "a/b" } ] } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { video: { gop_frames: 0 } } }"#,
            r#"{ zenoh: { mode: "peer" }, parallax: { idle_timeout_secs: 0 } }"#,
        ] {
            let config: ParallaxSensorConfig = json5::from_str(bad).unwrap();
            assert!(config.validate().is_err(), "should reject: {bad}");
        }
    }
}
