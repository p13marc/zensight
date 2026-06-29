use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};
use crate::serialization::Format;

/// Common Zenoh connection configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZenohConfig {
    /// Zenoh mode: "client", "peer", or "router".
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Endpoints to connect to (for client mode).
    #[serde(default)]
    pub connect: Vec<String>,

    /// Endpoints to listen on (for peer/router mode).
    #[serde(default)]
    pub listen: Vec<String>,
}

fn default_mode() -> String {
    "peer".to_string()
}

impl Default for ZenohConfig {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            connect: Vec::new(),
            listen: Vec::new(),
        }
    }
}

impl ZenohConfig {
    /// Apply `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` environment overrides.
    ///
    /// `CONNECT`/`LISTEN` are comma-separated endpoint lists. Unset variables
    /// leave the field untouched. This lets a launcher (e.g. `just run`) pin
    /// explicit local endpoints so the GUI and sensors connect reliably without
    /// depending on multicast peer discovery (which is unreliable on hosts with
    /// a VPN or multiple interfaces, e.g. tailscale/docker).
    pub fn with_env_overrides(self) -> Self {
        self.with_overrides_from(|k| std::env::var(k).ok())
    }

    /// Testable core of [`with_env_overrides`]: `get` resolves a variable name.
    fn with_overrides_from(mut self, get: impl Fn(&str) -> Option<String>) -> Self {
        if let Some(mode) = get("ZENSIGHT_ZENOH_MODE") {
            self.mode = mode;
        }
        let parse = |v: String| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        };
        if let Some(c) = get("ZENSIGHT_ZENOH_CONNECT") {
            self.connect = parse(c);
        }
        if let Some(l) = get("ZENSIGHT_ZENOH_LISTEN") {
            self.listen = parse(l);
        }
        self
    }
}

#[cfg(test)]
mod zenoh_env_tests {
    use super::*;
    use std::collections::HashMap;

    fn over(base: ZenohConfig, vars: &[(&str, &str)]) -> ZenohConfig {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        base.with_overrides_from(|k| map.get(k).cloned())
    }

    #[test]
    fn no_vars_leaves_config_unchanged() {
        let base = ZenohConfig {
            mode: "peer".into(),
            connect: vec!["tcp/a:1".into()],
            listen: vec![],
        };
        assert_eq!(over(base.clone(), &[]), base);
    }

    #[test]
    fn connect_and_listen_override_parse_csv() {
        let out = over(
            ZenohConfig::default(),
            &[
                ("ZENSIGHT_ZENOH_CONNECT", "tcp/127.0.0.1:7447, tcp/h:2 ,"),
                ("ZENSIGHT_ZENOH_LISTEN", "tcp/0.0.0.0:7448"),
                ("ZENSIGHT_ZENOH_MODE", "client"),
            ],
        );
        assert_eq!(out.mode, "client");
        assert_eq!(out.connect, vec!["tcp/127.0.0.1:7447", "tcp/h:2"]); // trimmed, empties dropped
        assert_eq!(out.listen, vec!["tcp/0.0.0.0:7448"]);
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable text format (default).
    #[default]
    Text,
    /// Structured JSON format.
    Json,
}

/// Common logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error".
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log output format: "text" or "json".
    #[serde(default)]
    pub format: LogFormat,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

/// Base configuration shared by all sensors.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// Serialization format for telemetry.
    #[serde(default)]
    pub serialization: Format,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Limits and policy for on-demand debug-report generation (`@/report`).
///
/// Disabled by default — a sensor opts in by setting `enabled: true` in its
/// config and overriding `SensorConfig::report_limits`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportLimits {
    /// Whether the sensor serves debug-report requests at all.
    #[serde(default)]
    pub enabled: bool,
    /// Hard cap on the generated bundle size; generation fails past this (R7).
    #[serde(default = "default_report_max_bytes")]
    pub max_bytes: u64,
    /// Minimum gap between successive generations, seconds (rate-limit, R7).
    #[serde(default = "default_report_cooldown")]
    pub cooldown_secs: u64,
    /// How long a generated bundle (and its resume window) stays available.
    #[serde(default = "default_report_ttl")]
    pub ttl_secs: u64,
    /// Chunk size used by the blob transfer (clamped to 256 KiB–1 MiB).
    #[serde(default = "default_report_chunk_size")]
    pub chunk_size: u32,
    /// Extra config field-name patterns to redact, on top of the built-in
    /// denylist (case-insensitive substring match).
    #[serde(default)]
    pub redact_extra: Vec<String>,
}

fn default_report_max_bytes() -> u64 {
    64 * 1024 * 1024
}
fn default_report_cooldown() -> u64 {
    30
}
fn default_report_ttl() -> u64 {
    600
}
fn default_report_chunk_size() -> u32 {
    512 * 1024
}

impl Default for ReportLimits {
    fn default() -> Self {
        ReportLimits {
            enabled: false,
            max_bytes: default_report_max_bytes(),
            cooldown_secs: default_report_cooldown(),
            ttl_secs: default_report_ttl(),
            chunk_size: default_report_chunk_size(),
            redact_extra: Vec::new(),
        }
    }
}

/// Load a configuration file in JSON5 format.
pub fn load_config<T: for<'de> Deserialize<'de>>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!(
            "Failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;

    json5::from_str(&content).map_err(|e| {
        Error::Config(format!(
            "Failed to parse config file '{}': {}",
            path.display(),
            e
        ))
    })
}

/// Load a configuration from a JSON5 string.
pub fn parse_config<T: for<'de> Deserialize<'de>>(content: &str) -> Result<T> {
    json5::from_str(content).map_err(|e| Error::Config(format!("Failed to parse config: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_base_config() {
        let json5 = r#"
        {
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"],
            },
            serialization: "cbor",
            logging: {
                level: "debug",
            },
        }
        "#;

        let config: BaseConfig = parse_config(json5).unwrap();

        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.zenoh.connect, vec!["tcp/localhost:7447"]);
        assert_eq!(config.serialization, Format::Cbor);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_default_config() {
        let json5 = "{}";
        let config: BaseConfig = parse_config(json5).unwrap();

        assert_eq!(config.zenoh.mode, "peer");
        assert!(config.zenoh.connect.is_empty());
        assert_eq!(config.serialization, Format::Json);
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, LogFormat::Text);
    }

    #[test]
    fn test_json_logging_format() {
        let json5 = r#"
        {
            logging: {
                level: "debug",
                format: "json",
            },
        }
        "#;

        let config: BaseConfig = parse_config(json5).unwrap();

        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, LogFormat::Json);
    }
}
