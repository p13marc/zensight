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

/// Base configuration shared by all bridges.
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
