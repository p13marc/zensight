//! Configuration for the netring sensor.

use serde::{Deserialize, Serialize};
use zensight_sensor_core::{LoggingConfig, SensorConfig, ZenohConfig};

fn default_key_prefix() -> String {
    "zensight/netring".to_string()
}
fn default_sensor_id() -> String {
    "auto".to_string()
}
fn default_bw_period() -> u64 {
    5
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetringSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub netring: NetringConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetringConfig {
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// Sensor identifier used as telemetry `source`. "auto" → hostname.
    #[serde(default = "default_sensor_id")]
    pub sensor_id: String,
    /// Capture interfaces (e.g. ["eth0"]). Ignored when `pcap` is set.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Replay an offline pcap instead of live capture (no privileges needed).
    #[serde(default)]
    pub pcap: Option<String>,
    #[serde(default)]
    pub collect: CollectConfig,
    #[serde(default = "default_bw_period")]
    pub bandwidth_period_secs: u64,
    #[serde(default)]
    pub anomalies: AnomalyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Per-application bandwidth (bytes/sec).
    #[serde(default = "default_true")]
    pub bandwidth: bool,
    /// Flow lifecycle aggregates.
    #[serde(default = "default_true")]
    pub flows: bool,
    /// TCP reset counters (resets + connection refusals).
    #[serde(default = "default_true")]
    pub tcp_resets: bool,
    /// Passive TLS fingerprinting (SNI + JA3/JA4 asset inventory). Needs capture
    /// (CAP_NET_RAW), same as all netring collection.
    #[serde(default = "default_true")]
    pub tls: bool,
    /// Capture self-health (packets/drops/drop_rate per source). Only fires on
    /// LIVE capture — the kernel ring has no drops to report under pcap replay.
    #[serde(default = "default_true")]
    pub capture_stats: bool,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            bandwidth: true,
            flows: true,
            tcp_resets: true,
            tls: true,
            capture_stats: true,
        }
    }
}

/// Anomaly detectors to run (Pillar A).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnomalyConfig {
    /// Port-scan detection (TRW).
    #[serde(default)]
    pub port_scan: bool,
}

impl NetringConfig {
    pub fn resolved_sensor_id(&self) -> String {
        if self.sensor_id == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.sensor_id.clone()
        }
    }
}

impl SensorConfig for NetringSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }
    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }
    fn key_prefix(&self) -> &str {
        &self.netring.key_prefix
    }
    fn validate(&self) -> zensight_sensor_core::Result<()> {
        if self.netring.pcap.is_none() && self.netring.interfaces.is_empty() {
            return Err(zensight_sensor_core::SensorError::config(
                "netring: configure at least one interface, or set `pcap` for replay",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_interface() {
        let cfg: NetringSensorConfig =
            json5::from_str(r#"{ netring: { sensor_id: "s1", interfaces: ["eth0"] } }"#).unwrap();
        assert_eq!(cfg.netring.key_prefix, "zensight/netring");
        assert_eq!(cfg.netring.resolved_sensor_id(), "s1");
        assert!(cfg.netring.collect.bandwidth);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_requires_source() {
        let cfg: NetringSensorConfig = json5::from_str(r#"{ netring: {} }"#).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn pcap_satisfies_validation() {
        let cfg: NetringSensorConfig =
            json5::from_str(r#"{ netring: { pcap: "/tmp/x.pcap" } }"#).unwrap();
        assert!(cfg.validate().is_ok());
    }
}
