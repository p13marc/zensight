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

/// H.264 video settings: a shared GOP plus the **tier ladder** — the sensor owns
/// the numbers, the wire and the `<tier>` key carry the name (#498). Each tier is
/// published concurrently on its own `@media/<stream>/video/h264/<tier>` key, so
/// two viewers on different links each subscribe to the tier their link can take
/// without fighting over one encoder (#494).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Keyframe (GOP) interval in frames, shared across tiers (default: 60).
    #[serde(default = "default_gop_frames")]
    pub gop_frames: u32,
    /// The bandwidth tiers this sensor offers (default: low/medium/high).
    #[serde(default = "default_tiers")]
    pub tiers: Vec<TierSpec>,
    /// Which tier an `OpenStream` with no explicit tier resolves to.
    #[serde(default = "default_default_tier")]
    pub default_tier: String,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            gop_frames: default_gop_frames(),
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
fn default_tiers() -> Vec<TierSpec> {
    vec![
        TierSpec {
            name: "low".into(),
            max_height: Some(240),
            fps: 10,
            bitrate_kbps: 400,
        },
        TierSpec {
            name: "medium".into(),
            max_height: Some(480),
            fps: 20,
            bitrate_kbps: 1200,
        },
        TierSpec {
            name: "high".into(),
            max_height: None,
            fps: 30,
            bitrate_kbps: 4000,
        },
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
            if t.name.is_empty() || t.name.contains('/') || t.name.contains('*') {
                return Err(ConfigError::Validation(format!(
                    "tier name {:?} must be a single key chunk (no '/' or '*')",
                    t.name
                )));
            }
            if !tier_names.insert(&t.name) {
                return Err(ConfigError::Validation(format!(
                    "duplicate tier name {:?}",
                    t.name
                )));
            }
            if t.fps == 0 {
                return Err(ConfigError::Validation(format!(
                    "tier {:?}: fps must be > 0",
                    t.name
                )));
            }
            if t.bitrate_kbps == 0 {
                return Err(ConfigError::Validation(format!(
                    "tier {:?}: bitrate_kbps must be > 0",
                    t.name
                )));
            }
            if let Some(mh) = t.max_height
                && mh < 2
            {
                return Err(ConfigError::Validation(format!(
                    "tier {:?}: max_height must be >= 2",
                    t.name
                )));
            }
        }
        if !p.video.tiers.iter().any(|t| t.name == p.video.default_tier) {
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
            .map(|t| t.name.as_str())
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
        assert_eq!(config.parallax.video.tiers[1].bitrate_kbps, 4000);
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
