//! Configuration for the Rerun visualization adapter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zensight_common::config::{LoggingConfig, ZenohConfig};

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] json5::Error),
    #[error("validation error: {0}")]
    Validation(String),
}

/// Complete adapter configuration (JSON5, same shape family as the exporters).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RerunConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// Rerun sink settings (mode, viewer URL, .rrd path, ids).
    #[serde(default)]
    pub rerun: RerunSinkConfig,

    /// Subscription filtering.
    #[serde(default)]
    pub filters: FilterConfig,

    /// Per-series sampling limits.
    #[serde(default)]
    pub sampling: SamplingConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Open an isolated Zenoh session: scouting (multicast + gossip) disabled,
    /// only explicit `connect`/`listen` endpoints. Prevents a live sensor
    /// fleet on the host from contaminating a demo/recording session.
    #[serde(default)]
    pub isolate: bool,
}

/// Where the adapter sends data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RerunMode {
    /// Stream to a running Rerun viewer/proxy over gRPC.
    #[default]
    Live,
    /// Write a `.rrd` file (headless-friendly; replay later).
    Record,
    /// Both at once (`RecordingStreamBuilder::set_sinks`).
    Both,
}

impl RerunMode {
    /// Stable lowercase name (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            RerunMode::Live => "live",
            RerunMode::Record => "record",
            RerunMode::Both => "both",
        }
    }
}

impl std::str::FromStr for RerunMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "live" => Ok(RerunMode::Live),
            "record" => Ok(RerunMode::Record),
            "both" => Ok(RerunMode::Both),
            other => Err(format!(
                "invalid rerun mode '{other}' (expected live|record|both)"
            )),
        }
    }
}

/// How monotonic counters are rendered (see docs/plans/rerun/02-mapping.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CounterPolicy {
    /// Per-second rate (first sample and resets absorbed silently).
    #[default]
    Rate,
    /// Raw counter value (debugging).
    Raw,
    /// Rate on `<metric>`, raw on `<metric>/raw`.
    Both,
}

/// Rerun sink settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerunSinkConfig {
    /// live | record | both.
    #[serde(default)]
    pub mode: RerunMode,

    /// Viewer/proxy gRPC URL (live/both). Default is the viewer's local proxy;
    /// note the endpoint is unauthenticated and unencrypted — keep it loopback
    /// or on an operator-controlled network (01-capabilities.md §4).
    #[serde(default = "default_viewer_url")]
    pub viewer_url: String,

    /// Output `.rrd` path (required for record/both).
    #[serde(default)]
    pub rrd_path: Option<PathBuf>,

    /// Rerun application id (viewer grouping + blueprint scope).
    #[serde(default = "default_application_id")]
    pub application_id: String,

    /// Explicit recording id. Producers sharing an id merge into one logical
    /// recording; default (None) lets Rerun generate a random per-process id.
    #[serde(default)]
    pub recording_id: Option<String>,

    /// Counter rendering policy.
    #[serde(default)]
    pub counters: CounterPolicy,
}

fn default_viewer_url() -> String {
    "rerun+http://127.0.0.1:9876/proxy".to_string()
}

fn default_application_id() -> String {
    "zensight".to_string()
}

impl Default for RerunSinkConfig {
    fn default() -> Self {
        Self {
            mode: RerunMode::default(),
            viewer_url: default_viewer_url(),
            rrd_path: None,
            application_id: default_application_id(),
            recording_id: None,
            counters: CounterPolicy::default(),
        }
    }
}

/// Subscription filtering (same knobs as the exporters).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Telemetry subscription key expression (default
    /// `zensight/v1/*/telemetry/**`). Narrow
    /// it to tame the firehose at the subscription. The alert/health/entity
    /// subscribers are separate and unaffected.
    #[serde(default)]
    pub key_expr: Option<String>,

    /// Only include these protocols (empty = all).
    #[serde(default)]
    pub include_protocols: Vec<String>,

    /// Exclude these protocols.
    #[serde(default)]
    pub exclude_protocols: Vec<String>,
}

impl FilterConfig {
    /// Whether a telemetry point from `protocol` passes the protocol filters.
    pub fn allows_protocol(&self, protocol: &str) -> bool {
        if self
            .exclude_protocols
            .iter()
            .any(|p| p.eq_ignore_ascii_case(protocol))
        {
            return false;
        }
        self.include_protocols.is_empty()
            || self
                .include_protocols
                .iter()
                .any(|p| p.eq_ignore_ascii_case(protocol))
    }
}

/// Per-series sampling limits (applied before rate conversion).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamplingConfig {
    /// Global per-series ceiling, samples/second (None = unlimited).
    #[serde(default)]
    pub max_hz_per_series: Option<f64>,

    /// Per-prefix overrides matched against the *metric* path — the part
    /// after `zensight/<protocol>/<source>/`, with no protocol/source in it —
    /// longest prefix wins. E.g. `{ "sockets/tcp": 0.2, "iface": 10.0 }`.
    #[serde(default)]
    pub per_prefix: HashMap<String, f64>,
}

impl RerunConfig {
    /// Load a configuration from a JSON5 file and validate it.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: RerunConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate cross-field constraints.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if matches!(self.rerun.mode, RerunMode::Record | RerunMode::Both)
            && self.rerun.rrd_path.is_none()
        {
            return Err(ConfigError::Validation(format!(
                "rerun.mode = \"{}\" requires rerun.rrd_path",
                self.rerun.mode.as_str()
            )));
        }
        if matches!(self.rerun.mode, RerunMode::Live | RerunMode::Both)
            && self.rerun.viewer_url.is_empty()
        {
            return Err(ConfigError::Validation(
                "rerun.viewer_url must not be empty in live/both mode".to_string(),
            ));
        }
        if self.rerun.application_id.is_empty() {
            return Err(ConfigError::Validation(
                "rerun.application_id must not be empty".to_string(),
            ));
        }
        if let Some(hz) = self.sampling.max_hz_per_series
            && hz <= 0.0
        {
            return Err(ConfigError::Validation(
                "sampling.max_hz_per_series must be > 0".to_string(),
            ));
        }
        for (prefix, hz) in &self.sampling.per_prefix {
            if *hz <= 0.0 {
                return Err(ConfigError::Validation(format!(
                    "sampling.per_prefix[\"{prefix}\"] must be > 0"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_live_mode_with_proxy_url() {
        let config: RerunConfig = json5::from_str("{}").unwrap();
        assert_eq!(config.rerun.mode, RerunMode::Live);
        assert_eq!(config.rerun.viewer_url, "rerun+http://127.0.0.1:9876/proxy");
        assert_eq!(config.rerun.application_id, "zensight");
        assert_eq!(config.rerun.counters, CounterPolicy::Rate);
        assert!(config.rerun.rrd_path.is_none());
        assert!(!config.isolate);
        config.validate().unwrap();
    }

    #[test]
    fn record_mode_requires_rrd_path() {
        let config: RerunConfig = json5::from_str(r#"{ rerun: { mode: "record" } }"#).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("rrd_path"), "{err}");

        let config: RerunConfig =
            json5::from_str(r#"{ rerun: { mode: "record", rrd_path: "/tmp/x.rrd" } }"#).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn both_mode_requires_rrd_path_and_url() {
        let config: RerunConfig = json5::from_str(r#"{ rerun: { mode: "both" } }"#).unwrap();
        assert!(config.validate().is_err());

        let config: RerunConfig = json5::from_str(
            r#"{ rerun: { mode: "both", rrd_path: "/tmp/x.rrd", viewer_url: "" } }"#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn full_config_parses() {
        let config: RerunConfig = json5::from_str(
            r#"{
                zenoh: { mode: "peer", connect: ["tcp/127.0.0.1:7447"] },
                rerun: {
                    mode: "both",
                    viewer_url: "rerun+http://10.0.0.2:9876/proxy",
                    rrd_path: "/var/lib/zensight/capture.rrd",
                    application_id: "zensight-lab",
                    recording_id: "incident-42",
                    counters: "both",
                },
                filters: { include_protocols: ["sysinfo", "netlink"] },
                sampling: { max_hz_per_series: 2.0, per_prefix: { "iface": 10.0 } },
                isolate: true,
            }"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.rerun.recording_id.as_deref(), Some("incident-42"));
        assert_eq!(config.rerun.counters, CounterPolicy::Both);
        assert!(config.isolate);
        assert_eq!(config.sampling.per_prefix["iface"], 10.0);
    }

    #[test]
    fn protocol_filters() {
        let f = FilterConfig {
            key_expr: None,
            include_protocols: vec!["sysinfo".into()],
            exclude_protocols: vec!["logs".into()],
        };
        assert!(f.allows_protocol("sysinfo"));
        assert!(f.allows_protocol("SYSINFO"));
        assert!(!f.allows_protocol("netlink")); // not included
        assert!(!f.allows_protocol("logs")); // excluded

        let open = FilterConfig::default();
        assert!(open.allows_protocol("anything"));
    }

    #[test]
    fn sampling_validation_rejects_nonpositive() {
        let config: RerunConfig =
            json5::from_str(r#"{ sampling: { max_hz_per_series: 0 } }"#).unwrap();
        assert!(config.validate().is_err());
        let config: RerunConfig =
            json5::from_str(r#"{ sampling: { per_prefix: { "x": -1 } } }"#).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn mode_from_str() {
        assert_eq!("record".parse::<RerunMode>().unwrap(), RerunMode::Record);
        assert_eq!("BOTH".parse::<RerunMode>().unwrap(), RerunMode::Both);
        assert!("viewer".parse::<RerunMode>().is_err());
    }
}
