//! Configuration for the OpenTelemetry exporter.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use zensight_common::config::ZenohConfig;

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

/// Complete exporter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct ExporterConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// OpenTelemetry exporter settings.
    #[serde(default)]
    pub opentelemetry: OtelConfig,

    /// Metric filtering settings.
    #[serde(default)]
    pub filters: FilterConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// OpenTelemetry OTLP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelConfig {
    /// OTLP endpoint (e.g., "http://localhost:4317" for gRPC).
    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    /// Protocol: "grpc" or "http".
    #[serde(default = "default_protocol")]
    pub protocol: OtlpProtocol,

    /// Headers to include in OTLP requests (e.g., for authentication).
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Export interval in seconds.
    #[serde(default = "default_export_interval")]
    pub export_interval_secs: u64,

    /// Export timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to export metrics.
    #[serde(default = "default_true")]
    pub export_metrics: bool,

    /// Whether to export logs (syslog messages).
    #[serde(default = "default_true")]
    pub export_logs: bool,

    /// Resource attributes to add to all telemetry.
    #[serde(default)]
    pub resource: HashMap<String, String>,

    /// Service name for OTEL resource.
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// Service version for OTEL resource.
    #[serde(default)]
    pub service_version: Option<String>,
}

fn default_endpoint() -> String {
    "http://localhost:4317".to_string()
}

fn default_protocol() -> OtlpProtocol {
    OtlpProtocol::Grpc
}

fn default_export_interval() -> u64 {
    10
}

fn default_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_service_name() -> String {
    "zensight".to_string()
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            protocol: default_protocol(),
            headers: HashMap::new(),
            export_interval_secs: default_export_interval(),
            timeout_secs: default_timeout(),
            export_metrics: true,
            export_logs: true,
            resource: HashMap::new(),
            service_name: default_service_name(),
            service_version: None,
        }
    }
}

impl OtelConfig {
    /// Get export interval as Duration.
    pub fn export_interval(&self) -> Duration {
        Duration::from_secs(self.export_interval_secs)
    }

    /// Get timeout as Duration.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// OTLP protocol selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OtlpProtocol {
    /// gRPC protocol (port 4317).
    #[default]
    Grpc,
    /// HTTP/protobuf protocol (port 4318).
    Http,
}

/// Metric filtering configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Only include these protocols (empty = all).
    #[serde(default)]
    pub include_protocols: Vec<String>,

    /// Exclude these protocols.
    #[serde(default)]
    pub exclude_protocols: Vec<String>,

    /// Only include these sources (empty = all).
    #[serde(default)]
    pub include_sources: Vec<String>,

    /// Exclude these sources.
    #[serde(default)]
    pub exclude_sources: Vec<String>,
}

/// Logging configuration.
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

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

impl ExporterConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: ExporterConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse configuration from a JSON5 string.
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: ExporterConfig = json5::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.opentelemetry.endpoint.is_empty() {
            return Err(ConfigError::Validation(
                "OTLP endpoint cannot be empty".to_string(),
            ));
        }

        if self.opentelemetry.export_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "export_interval_secs must be > 0".to_string(),
            ));
        }

        if self.opentelemetry.timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "timeout_secs must be > 0".to_string(),
            ));
        }

        if !self.opentelemetry.export_metrics && !self.opentelemetry.export_logs {
            return Err(ConfigError::Validation(
                "At least one of export_metrics or export_logs must be enabled".to_string(),
            ));
        }

        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = "{}";
        let config = ExporterConfig::parse(json).unwrap();

        assert_eq!(config.opentelemetry.endpoint, "http://localhost:4317");
        assert_eq!(config.opentelemetry.protocol, OtlpProtocol::Grpc);
        assert_eq!(config.opentelemetry.export_interval_secs, 10);
        assert!(config.opentelemetry.export_metrics);
        assert!(config.opentelemetry.export_logs);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            opentelemetry: {
                endpoint: "http://otel-collector:4317",
                protocol: "grpc",
                export_interval_secs: 30,
                timeout_secs: 60,
                export_metrics: true,
                export_logs: true,
                service_name: "my-zensight",
                service_version: "1.0.0",
                headers: {
                    "Authorization": "Bearer token123"
                },
                resource: {
                    "deployment.environment": "production"
                }
            },
            filters: {
                include_protocols: ["snmp", "sysinfo"],
                exclude_sources: ["test-device"]
            },
            logging: {
                level: "debug",
                format: "json"
            }
        }"#;

        let config = ExporterConfig::parse(json).unwrap();

        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.opentelemetry.endpoint, "http://otel-collector:4317");
        assert_eq!(config.opentelemetry.protocol, OtlpProtocol::Grpc);
        assert_eq!(config.opentelemetry.export_interval_secs, 30);
        assert_eq!(config.opentelemetry.service_name, "my-zensight");
        assert_eq!(
            config.opentelemetry.service_version,
            Some("1.0.0".to_string())
        );
        assert_eq!(
            config.opentelemetry.headers.get("Authorization"),
            Some(&"Bearer token123".to_string())
        );
        assert_eq!(config.filters.include_protocols, vec!["snmp", "sysinfo"]);
        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, LogFormat::Json);
    }

    #[test]
    fn test_validate_empty_endpoint() {
        let json = r#"{
            opentelemetry: { endpoint: "" }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("endpoint"));
    }

    #[test]
    fn test_validate_zero_interval() {
        let json = r#"{
            opentelemetry: { export_interval_secs: 0 }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_no_exports() {
        let json = r#"{
            opentelemetry: {
                export_metrics: false,
                export_logs: false
            }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("At least one of"));
    }

    #[test]
    fn test_http_protocol() {
        let json = r#"{
            opentelemetry: {
                endpoint: "http://localhost:4318",
                protocol: "http"
            }
        }"#;

        let config = ExporterConfig::parse(json).unwrap();
        assert_eq!(config.opentelemetry.protocol, OtlpProtocol::Http);
    }
}
