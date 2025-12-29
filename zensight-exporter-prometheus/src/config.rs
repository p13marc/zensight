//! Configuration for the Prometheus exporter.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
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
pub struct ExporterConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// Prometheus exporter settings.
    #[serde(default)]
    pub prometheus: PrometheusConfig,

    /// Metric aggregation settings.
    #[serde(default)]
    pub aggregation: AggregationConfig,

    /// Metric filtering settings.
    #[serde(default)]
    pub filters: FilterConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Prometheus HTTP endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrometheusConfig {
    /// Address to listen on (default: "0.0.0.0:9090").
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Path for metrics endpoint (default: "/metrics").
    #[serde(default = "default_path")]
    pub path: String,

    /// Default labels to add to all metrics.
    #[serde(default)]
    pub default_labels: HashMap<String, String>,

    /// Metric name prefix (default: "zensight").
    #[serde(default = "default_prefix")]
    pub prefix: String,
}

fn default_listen() -> String {
    "0.0.0.0:9090".to_string()
}

fn default_path() -> String {
    "/metrics".to_string()
}

fn default_prefix() -> String {
    "zensight".to_string()
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            path: default_path(),
            default_labels: HashMap::new(),
            prefix: default_prefix(),
        }
    }
}

/// Aggregation and staleness configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationConfig {
    /// How long to keep metrics without updates before expiring (seconds).
    #[serde(default = "default_stale_timeout")]
    pub stale_timeout_secs: u64,

    /// Maximum unique time series (memory protection).
    #[serde(default = "default_max_series")]
    pub max_series: usize,

    /// How often to run cleanup of stale metrics (seconds).
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval_secs: u64,
}

fn default_stale_timeout() -> u64 {
    300 // 5 minutes
}

fn default_max_series() -> usize {
    100_000
}

fn default_cleanup_interval() -> u64 {
    60 // 1 minute
}

impl Default for AggregationConfig {
    fn default() -> Self {
        Self {
            stale_timeout_secs: default_stale_timeout(),
            max_series: default_max_series(),
            cleanup_interval_secs: default_cleanup_interval(),
        }
    }
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

    /// Glob patterns for metrics to include (empty = all).
    #[serde(default)]
    pub include_metrics: Vec<String>,

    /// Glob patterns for metrics to exclude.
    #[serde(default)]
    pub exclude_metrics: Vec<String>,

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
        if self.aggregation.stale_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "stale_timeout_secs must be > 0".to_string(),
            ));
        }

        if self.aggregation.max_series == 0 {
            return Err(ConfigError::Validation(
                "max_series must be > 0".to_string(),
            ));
        }

        if self.aggregation.cleanup_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "cleanup_interval_secs must be > 0".to_string(),
            ));
        }

        // Validate listen address format
        if self
            .prometheus
            .listen
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            return Err(ConfigError::Validation(format!(
                "Invalid listen address: {}",
                self.prometheus.listen
            )));
        }

        // Validate path starts with /
        if !self.prometheus.path.starts_with('/') {
            return Err(ConfigError::Validation(
                "Metrics path must start with /".to_string(),
            ));
        }

        Ok(())
    }
}

impl Default for ExporterConfig {
    fn default() -> Self {
        Self {
            zenoh: ZenohConfig::default(),
            prometheus: PrometheusConfig::default(),
            aggregation: AggregationConfig::default(),
            filters: FilterConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = "{}";
        let config = ExporterConfig::parse(json).unwrap();

        assert_eq!(config.prometheus.listen, "0.0.0.0:9090");
        assert_eq!(config.prometheus.path, "/metrics");
        assert_eq!(config.prometheus.prefix, "zensight");
        assert_eq!(config.aggregation.stale_timeout_secs, 300);
        assert_eq!(config.aggregation.max_series, 100_000);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            prometheus: {
                listen: "127.0.0.1:9091",
                path: "/prometheus/metrics",
                prefix: "myapp",
                default_labels: {
                    environment: "production",
                    datacenter: "us-east-1"
                }
            },
            aggregation: {
                stale_timeout_secs: 600,
                max_series: 50000,
                cleanup_interval_secs: 30
            },
            filters: {
                include_protocols: ["snmp", "sysinfo"],
                exclude_metrics: ["**/debug/**"]
            },
            logging: {
                level: "debug",
                format: "json"
            }
        }"#;

        let config = ExporterConfig::parse(json).unwrap();

        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.zenoh.connect, vec!["tcp/localhost:7447"]);
        assert_eq!(config.prometheus.listen, "127.0.0.1:9091");
        assert_eq!(config.prometheus.path, "/prometheus/metrics");
        assert_eq!(config.prometheus.prefix, "myapp");
        assert_eq!(
            config.prometheus.default_labels.get("environment"),
            Some(&"production".to_string())
        );
        assert_eq!(config.aggregation.stale_timeout_secs, 600);
        assert_eq!(config.aggregation.max_series, 50000);
        assert_eq!(config.filters.include_protocols, vec!["snmp", "sysinfo"]);
        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, LogFormat::Json);
    }

    #[test]
    fn test_validate_invalid_listen() {
        let json = r#"{
            prometheus: { listen: "not-an-address" }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid listen address")
        );
    }

    #[test]
    fn test_validate_invalid_path() {
        let json = r#"{
            prometheus: { path: "no-leading-slash" }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must start with /")
        );
    }

    #[test]
    fn test_validate_zero_stale_timeout() {
        let json = r#"{
            aggregation: { stale_timeout_secs: 0 }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_zero_max_series() {
        let json = r#"{
            aggregation: { max_series: 0 }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
    }
}
