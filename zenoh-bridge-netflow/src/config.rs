//! NetFlow/IPFIX bridge configuration.

use serde::{Deserialize, Serialize};
use std::path::Path;
use zensight_common::config::ZenohConfig;

/// Complete NetFlow bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetFlowBridgeConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// NetFlow-specific settings.
    pub netflow: NetFlowConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// NetFlow receiver configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetFlowConfig {
    /// Key expression prefix for publishing.
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// UDP listener configurations.
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,

    /// Exporter name mappings (IP -> friendly name).
    #[serde(default)]
    pub exporter_names: std::collections::HashMap<String, String>,

    /// Whether to publish individual flow records.
    #[serde(default = "default_true")]
    pub publish_flows: bool,

    /// Whether to publish aggregated statistics.
    #[serde(default = "default_true")]
    pub publish_stats: bool,

    /// Flow aggregation interval in seconds (0 = no aggregation).
    #[serde(default)]
    pub aggregation_interval_secs: u64,
}

fn default_key_prefix() -> String {
    "zensight/netflow".to_string()
}

fn default_true() -> bool {
    true
}

/// Individual listener configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
    /// Bind address (e.g., "0.0.0.0:2055").
    pub bind: String,

    /// Maximum packet size.
    #[serde(default = "default_max_packet_size")]
    pub max_packet_size: usize,
}

fn default_max_packet_size() -> usize {
    65535
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level.
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

impl NetFlowBridgeConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let config: Self = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.netflow.listeners.is_empty() {
            anyhow::bail!("At least one listener must be configured");
        }

        for (i, listener) in self.netflow.listeners.iter().enumerate() {
            if listener.bind.is_empty() {
                anyhow::bail!("Listener {} has empty bind address", i);
            }

            if !listener.bind.contains(':') {
                anyhow::bail!(
                    "Listener {} bind address must include port (e.g., '0.0.0.0:2055')",
                    i
                );
            }
        }

        Ok(())
    }
}

impl Default for NetFlowConfig {
    fn default() -> Self {
        Self {
            key_prefix: default_key_prefix(),
            listeners: vec![ListenerConfig {
                bind: "0.0.0.0:2055".to_string(),
                max_packet_size: default_max_packet_size(),
            }],
            exporter_names: std::collections::HashMap::new(),
            publish_flows: true,
            publish_stats: true,
            aggregation_interval_secs: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            netflow: {
                listeners: [
                    { bind: "0.0.0.0:2055" }
                ]
            }
        }"#;

        let config: NetFlowBridgeConfig = json5::from_str(json).unwrap();
        assert_eq!(config.netflow.key_prefix, "zensight/netflow");
        assert_eq!(config.netflow.listeners.len(), 1);
        assert!(config.netflow.publish_flows);
        assert!(config.netflow.publish_stats);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            netflow: {
                key_prefix: "custom/netflow",
                listeners: [
                    { bind: "0.0.0.0:2055", max_packet_size: 9000 },
                    { bind: "0.0.0.0:4739" }
                ],
                exporter_names: {
                    "192.168.1.1": "core-router",
                    "192.168.1.2": "edge-switch"
                },
                publish_flows: true,
                publish_stats: true,
                aggregation_interval_secs: 60
            },
            logging: {
                level: "debug"
            }
        }"#;

        let config: NetFlowBridgeConfig = json5::from_str(json).unwrap();
        assert_eq!(config.netflow.key_prefix, "custom/netflow");
        assert_eq!(config.netflow.listeners.len(), 2);
        assert_eq!(config.netflow.listeners[0].max_packet_size, 9000);
        assert_eq!(
            config.netflow.exporter_names.get("192.168.1.1"),
            Some(&"core-router".to_string())
        );
        assert_eq!(config.netflow.aggregation_interval_secs, 60);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_validate_empty_listeners() {
        let json = r#"{
            zenoh: { mode: "peer" },
            netflow: {
                listeners: []
            }
        }"#;

        let config: NetFlowBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_missing_port() {
        let json = r#"{
            zenoh: { mode: "peer" },
            netflow: {
                listeners: [
                    { bind: "0.0.0.0" }
                ]
            }
        }"#;

        let config: NetFlowBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }
}
