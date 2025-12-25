//! Syslog bridge configuration.

use serde::{Deserialize, Serialize};
use std::path::Path;
use zensight_common::config::ZenohConfig;

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_bridge_framework::LoggingConfig;

/// Complete syslog bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogBridgeConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Syslog-specific settings.
    pub syslog: SyslogConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Syslog receiver configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogConfig {
    /// Key expression prefix for publishing.
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Listener configurations.
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,

    /// Hostname overrides for source identification.
    #[serde(default)]
    pub hostname_aliases: std::collections::HashMap<String, String>,

    /// Whether to include raw message in labels.
    #[serde(default)]
    pub include_raw_message: bool,
}

fn default_key_prefix() -> String {
    "zensight/syslog".to_string()
}

/// Individual listener configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
    /// Protocol: "udp" or "tcp".
    pub protocol: ListenerProtocol,

    /// Bind address (e.g., "0.0.0.0:514").
    pub bind: String,

    /// Maximum message size in bytes (UDP only).
    #[serde(default = "default_max_message_size")]
    pub max_message_size: usize,

    /// TCP-specific: maximum concurrent connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// TCP-specific: connection timeout in seconds.
    #[serde(default = "default_connection_timeout_secs")]
    pub connection_timeout_secs: u64,
}

fn default_max_message_size() -> usize {
    65535
}

fn default_max_connections() -> usize {
    1000
}

fn default_connection_timeout_secs() -> u64 {
    300
}

/// Listener protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerProtocol {
    Udp,
    Tcp,
}

impl std::fmt::Display for ListenerProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Udp => write!(f, "udp"),
            Self::Tcp => write!(f, "tcp"),
        }
    }
}

impl SyslogBridgeConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let config: Self = json5::from_str(&content)?;
        config.validate_config()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate_config(&self) -> anyhow::Result<()> {
        if self.syslog.listeners.is_empty() {
            anyhow::bail!("At least one listener must be configured");
        }

        for (i, listener) in self.syslog.listeners.iter().enumerate() {
            if listener.bind.is_empty() {
                anyhow::bail!("Listener {} has empty bind address", i);
            }

            // Validate bind address format
            if !listener.bind.contains(':') {
                anyhow::bail!(
                    "Listener {} bind address must include port (e.g., '0.0.0.0:514')",
                    i
                );
            }
        }

        Ok(())
    }
}

impl zensight_bridge_framework::BridgeConfig for SyslogBridgeConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn key_prefix(&self) -> &str {
        &self.syslog.key_prefix
    }

    fn validate(&self) -> zensight_bridge_framework::Result<()> {
        self.validate_config()
            .map_err(|e| zensight_bridge_framework::BridgeError::config(e.to_string()))
    }
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            key_prefix: default_key_prefix(),
            listeners: vec![ListenerConfig {
                protocol: ListenerProtocol::Udp,
                bind: "0.0.0.0:514".to_string(),
                max_message_size: default_max_message_size(),
                max_connections: default_max_connections(),
                connection_timeout_secs: default_connection_timeout_secs(),
            }],
            hostname_aliases: std::collections::HashMap::new(),
            include_raw_message: false,
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
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514" }
                ]
            }
        }"#;

        let config: SyslogBridgeConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.key_prefix, "zensight/syslog");
        assert_eq!(config.syslog.listeners.len(), 1);
        assert_eq!(config.syslog.listeners[0].protocol, ListenerProtocol::Udp);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            syslog: {
                key_prefix: "custom/syslog",
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514", max_message_size: 8192 },
                    { protocol: "tcp", bind: "0.0.0.0:514", max_connections: 500 }
                ],
                hostname_aliases: {
                    "192.168.1.1": "router01"
                },
                include_raw_message: true
            },
            logging: {
                level: "debug"
            }
        }"#;

        let config: SyslogBridgeConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.key_prefix, "custom/syslog");
        assert_eq!(config.syslog.listeners.len(), 2);
        assert_eq!(config.syslog.listeners[0].max_message_size, 8192);
        assert_eq!(config.syslog.listeners[1].max_connections, 500);
        assert_eq!(
            config.syslog.hostname_aliases.get("192.168.1.1"),
            Some(&"router01".to_string())
        );
        assert!(config.syslog.include_raw_message);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_validate_empty_listeners() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: []
            }
        }"#;

        let config: SyslogBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_validate_missing_port() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0" }
                ]
            }
        }"#;

        let config: SyslogBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }
}
