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

/// Common logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error".
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

/// Base configuration shared by all bridges.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for BaseConfig {
    fn default() -> Self {
        Self {
            zenoh: ZenohConfig::default(),
            serialization: Format::default(),
            logging: LoggingConfig::default(),
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
    }
}
