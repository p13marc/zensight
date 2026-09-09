//! gNMI sensor configuration

use serde::{Deserialize, Serialize};
use zensight_common::ZenohConfig;

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_sensor_core::LoggingConfig;

/// Top-level configuration for the gNMI sensor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GnmiConfig {
    /// Zenoh connection settings
    pub zenoh: ZenohConfig,

    /// gNMI sensor settings
    pub gnmi: GnmiSettings,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Declared resource envelope (#811/#1091). `resources.budget_rss_mb` is
    /// carried into the health doc's `self_stats.budget_bytes` and graded by
    /// the runner's `sensor-budget` rule at 80 % — declared, not enforced
    /// (#812 is the shed ladder). Absent reads as *undeclared*, never as
    /// unlimited-and-fine.
    #[serde(default)]
    pub resources: zensight_sensor_core::ResourcesConfig,

    /// On-demand artifact channel (`@rpc/gnmi/artifact/*`) limits — report + snapshot.
    /// Every kind disabled by default.
    #[serde(default)]
    pub artifacts: zensight_sensor_core::ArtifactLimits,

    /// `@desired` reconcile settings (#931): the kill switch and refresh
    /// cadence. File config on purpose — the mechanism that could misbehave
    /// must be disarmable from outside itself.
    #[serde(default)]
    pub desired: zensight_common::desired::DesiredConfig,

    /// Operator-authored threshold rules over this sensor's own telemetry
    /// (#931). **Empty by default** — this build ships no threshold that
    /// fires. Also authorable fleet-wide on `@desired` and per-host over
    /// `@rpc/gnmi/thresholds/set`; `state/gnmi/applied/thresholds`
    /// says which of the three is in force.
    #[serde(default)]
    pub thresholds: zensight_common::threshold::ThresholdsConfig,
}

/// gNMI-specific settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GnmiSettings {
    /// Override the agent-host source id (default: the local hostname).
    #[serde(default)]
    pub source: Option<String>,

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

    /// Path substrings that name a **counter**, in addition to the default
    /// `/counters/` (#1077).
    ///
    /// A gNMI value carries no kind: OpenConfig types `state/counters/in-octets`
    /// and `cpu/utilization/state/instant` both as `uint64`, and the subscriber
    /// used to publish *every* `UintVal` as a `Counter`. A backend then applies
    /// `rate()` to CPU utilisation, and every legitimate decrease looks like a
    /// reset. Only the path says which is which, and only for the vendor trees
    /// this list is for — OpenConfig's own convention is covered by the default.
    #[serde(default)]
    pub counter_paths: Vec<String>,

    /// Path substrings that name a **gauge**, overriding `counter_paths` and
    /// the `/counters/` default where a vendor puts a level under one.
    #[serde(default)]
    pub gauge_paths: Vec<String>,

    /// How far a device's own timestamp may sit from this host's clock before
    /// the receive time is used instead, in seconds (#1077). `0` disables the
    /// clamp and trusts the device unconditionally, which is what every build
    /// before this one did.
    ///
    /// A switch that has not reached NTP after a reload — the common case —
    /// publishes points months out; a `timestamp: 0`, which the spec defines as
    /// "unset", published every point at the epoch.
    #[serde(default = "default_max_clock_skew_secs")]
    pub max_clock_skew_secs: u64,
}

/// Five minutes: comfortably past any plausible NTP offset on a device that
/// *has* synchronised, and far short of the months a device that has not is out
/// by.
fn default_max_clock_skew_secs() -> u64 {
    300
}

impl GnmiTarget {
    /// What kind of value a path carries (#1077).
    ///
    /// `/counters/` is OpenConfig's own convention — `state/counters/in-octets`
    /// and friends — and is the only default. Everything else is a **gauge**,
    /// because that is the answer that costs least when it is wrong: a level
    /// mistaken for a counter makes `rate()` produce garbage and every decrease
    /// look like a reset, while a counter mistaken for a level is still exactly
    /// the number the device sent, just typed as the thing it also is.
    pub fn kind_for_path(&self, path: &str) -> ValueKind {
        if self.gauge_paths.iter().any(|p| path.contains(p.as_str())) {
            return ValueKind::Gauge;
        }
        if path.contains("/counters/")
            || path.starts_with("counters/")
            || self.counter_paths.iter().any(|p| path.contains(p.as_str()))
        {
            return ValueKind::Counter;
        }
        ValueKind::Gauge
    }
}

/// What a numeric gNMI leaf is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// Monotonic until whatever counts it restarts.
    Counter,
    /// A level: it may fall, and falling means it fell.
    Gauge,
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
    Json,
    /// Default: compact binary; every consumer decodes via `decode_auto`.
    #[default]
    Cbor,
}

impl From<SerializationFormat> for zensight_common::serialization::Format {
    /// This crate predates the shared [`Format`] and kept its own two-variant
    /// copy. The publish path needs the shared one (#931: `put_point` encodes
    /// *after* the threshold evaluator has seen the point), so the two are
    /// bridged here rather than duplicating the encode.
    fn from(f: SerializationFormat) -> Self {
        match f {
            SerializationFormat::Json => zensight_common::serialization::Format::Json,
            SerializationFormat::Cbor => zensight_common::serialization::Format::Cbor,
        }
    }
}

impl GnmiSettings {
    /// The agent host's unified source id: the `source` override, else the hostname.
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string())
        })
    }
}

fn default_sample_interval() -> u64 {
    10000 // 10 seconds
}

impl GnmiConfig {
    /// Load configuration from a JSON5 file
    pub fn load_from_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = json5::from_str(&content)?;
        Ok(config)
    }
}

impl zensight_sensor_core::SensorConfig for GnmiConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &str {
        "gnmi"
    }

    fn resources(&self) -> &zensight_sensor_core::ResourcesConfig {
        &self.resources
    }

    fn desired(&self) -> zensight_common::desired::DesiredConfig {
        self.desired.clone()
    }

    fn thresholds(&self) -> zensight_common::threshold::ThresholdsConfig {
        self.thresholds.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        if self.gnmi.targets.is_empty() {
            return Err(zensight_sensor_core::SensorError::config(
                "At least one target must be configured",
            ));
        }
        for target in &self.gnmi.targets {
            if target.name.is_empty() {
                return Err(zensight_sensor_core::SensorError::config(
                    "Target name cannot be empty",
                ));
            }
            if target.address.is_empty() {
                return Err(zensight_sensor_core::SensorError::config(format!(
                    "Target '{}' has no address",
                    target.name
                )));
            }
        }
        Ok(())
    }

    fn artifact_limits(&self) -> zensight_sensor_core::ArtifactLimits {
        self.artifacts.clone()
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

/// The shipped example config must load (#845): it ships in the release
/// tarball and the container image, and nothing else in CI ever parsed it —
/// so a renamed field silently reverted to its serde default in production
/// (`gen-configs.sh` documents the identical hazard for the demo profile).
/// Precedent: parallax/logs guard their shipped configs the same way.
#[cfg(test)]
mod shipped_config {
    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/gnmi.json5");
        let _config =
            crate::config::GnmiConfig::load_from_file(path).expect("configs/gnmi.json5 must load");
    }
}
