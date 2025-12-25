//! gNMI bridge configuration

use serde::{Deserialize, Serialize};
use zensight_common::ZenohConfig;

/// Top-level configuration for the gNMI bridge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GnmiConfig {
    /// Zenoh connection settings
    pub zenoh: ZenohConfig,

    /// gNMI bridge settings
    pub gnmi: GnmiSettings,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// gNMI-specific settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GnmiSettings {
    /// Key expression prefix for publishing
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Serialization format
    #[serde(default)]
    pub serialization: SerializationFormat,

    /// Target devices to subscribe to
    pub targets: Vec<GnmiTarget>,
}

/// A gNMI target device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GnmiTarget {
    /// Name used in key expressions
    pub name: String,

    /// gRPC endpoint (e.g., "192.168.1.1:9339")
    pub address: String,

    /// Authentication credentials
    #[serde(default)]
    pub credentials: Option<Credentials>,

    /// TLS configuration
    #[serde(default)]
    pub tls: TlsConfig,

    /// Subscription paths
    pub subscriptions: Vec<Subscription>,

    /// gNMI encoding for requests
    #[serde(default)]
    pub encoding: GnmiEncoding,
}

/// Authentication credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Username for authentication
    pub username: String,

    /// Password for authentication
    pub password: String,
}

/// TLS configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS
    #[serde(default)]
    pub enabled: bool,

    /// Skip certificate verification (not recommended for production)
    #[serde(default)]
    pub skip_verify: bool,

    /// Path to CA certificate file
    #[serde(default)]
    pub ca_cert: Option<String>,

    /// Path to client certificate file
    #[serde(default)]
    pub client_cert: Option<String>,

    /// Path to client key file
    #[serde(default)]
    pub client_key: Option<String>,
}

/// A gNMI subscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// XPath or gNMI path to subscribe to
    pub path: String,

    /// Subscription mode
    #[serde(default)]
    pub mode: SubscriptionMode,

    /// Sample interval in milliseconds (for SAMPLE mode)
    #[serde(default = "default_sample_interval")]
    pub sample_interval_ms: u64,

    /// Suppress redundant updates
    #[serde(default)]
    pub suppress_redundant: bool,

    /// Heartbeat interval in milliseconds
    #[serde(default)]
    pub heartbeat_interval_ms: u64,
}

/// Subscription mode
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubscriptionMode {
    /// Stream updates as they occur
    #[default]
    OnChange,

    /// Sample at fixed intervals
    Sample,

    /// Target determines update timing
    TargetDefined,
}

/// gNMI encoding format
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GnmiEncoding {
    /// JSON encoding
    #[default]
    Json,

    /// JSON with IETF formatting
    JsonIetf,

    /// Protocol Buffers
    Proto,

    /// ASCII text
    Ascii,
}

/// Serialization format for Zenoh messages
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SerializationFormat {
    #[default]
    Json,
    Cbor,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level
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

fn default_key_prefix() -> String {
    "zensight/gnmi".to_string()
}

fn default_sample_interval() -> u64 {
    10000 // 10 seconds
}

fn default_log_level() -> String {
    "info".to_string()
}

impl GnmiConfig {
    /// Load configuration from a JSON5 file
    pub fn load_from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = json5::from_str(&content)?;
        Ok(config)
    }
}

impl GnmiEncoding {
    /// Convert to gNMI proto encoding value
    pub fn to_proto(&self) -> i32 {
        match self {
            GnmiEncoding::Json => 0,
            GnmiEncoding::JsonIetf => 4,
            GnmiEncoding::Proto => 2,
            GnmiEncoding::Ascii => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_config() {
        let json = r#"{
            "zenoh": {
                "mode": "peer"
            },
            "gnmi": {
                "key_prefix": "zensight/gnmi",
                "targets": [
                    {
                        "name": "router01",
                        "address": "192.168.1.1:9339",
                        "credentials": {
                            "username": "admin",
                            "password": "admin"
                        },
                        "subscriptions": [
                            {
                                "path": "/interfaces/interface/state/counters",
                                "mode": "SAMPLE",
                                "sample_interval_ms": 5000
                            }
                        ]
                    }
                ]
            }
        }"#;

        let config: GnmiConfig = json5::from_str(json).unwrap();
        assert_eq!(config.gnmi.targets.len(), 1);
        assert_eq!(config.gnmi.targets[0].name, "router01");
        assert_eq!(
            config.gnmi.targets[0].subscriptions[0].mode,
            SubscriptionMode::Sample
        );
    }

    #[test]
    fn test_subscription_modes() {
        let on_change: SubscriptionMode = serde_json::from_str(r#""ON_CHANGE""#).unwrap();
        assert_eq!(on_change, SubscriptionMode::OnChange);

        let sample: SubscriptionMode = serde_json::from_str(r#""SAMPLE""#).unwrap();
        assert_eq!(sample, SubscriptionMode::Sample);
    }

    #[test]
    fn test_encoding_to_proto() {
        assert_eq!(GnmiEncoding::Json.to_proto(), 0);
        assert_eq!(GnmiEncoding::Proto.to_proto(), 2);
        assert_eq!(GnmiEncoding::Ascii.to_proto(), 3);
        assert_eq!(GnmiEncoding::JsonIetf.to_proto(), 4);
    }

    #[test]
    fn test_tls_config_defaults() {
        let tls = TlsConfig::default();
        assert!(!tls.enabled);
        assert!(!tls.skip_verify);
        assert!(tls.ca_cert.is_none());
    }
}
