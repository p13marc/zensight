//! Health and liveness types for frontend consumption.
//!
//! These types mirror the bridge-framework health types but are designed
//! for deserialization in the frontend without requiring the full framework.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Device availability status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    /// Device is responding normally.
    Online,
    /// Device is not responding.
    Offline,
    /// Device is responding but with errors.
    Degraded,
    /// Device status is unknown (never polled).
    #[default]
    Unknown,
}

impl std::fmt::Display for DeviceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceStatus::Online => write!(f, "online"),
            DeviceStatus::Offline => write!(f, "offline"),
            DeviceStatus::Degraded => write!(f, "degraded"),
            DeviceStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Health snapshot from a bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Bridge name.
    pub bridge: String,
    /// Overall health status.
    pub status: String,
    /// Uptime in seconds.
    pub uptime_secs: u64,
    /// Total devices configured.
    pub devices_total: u64,
    /// Devices currently responding.
    pub devices_responding: u64,
    /// Devices currently failed.
    pub devices_failed: u64,
    /// Last poll duration in milliseconds.
    pub last_poll_duration_ms: u64,
    /// Errors in the last hour.
    pub errors_last_hour: u64,
    /// Total metrics published.
    pub metrics_published: u64,
}

/// Device liveness information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLiveness {
    /// Device identifier.
    pub device: String,
    /// Current status.
    pub status: DeviceStatus,
    /// Last seen timestamp (millis since epoch).
    pub last_seen: i64,
    /// Consecutive failures count.
    pub consecutive_failures: u32,
    /// Last error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Error type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Connection timeout.
    Timeout,
    /// Authentication failed.
    AuthFailed,
    /// Connection refused.
    ConnectionRefused,
    /// Connection reset.
    ConnectionReset,
    /// Parse/decode error.
    ParseError,
    /// Protocol error.
    ProtocolError,
    /// Configuration error.
    ConfigError,
    /// Other/unknown error.
    Other,
}

impl Default for ErrorType {
    fn default() -> Self {
        Self::Other
    }
}

/// Error report from a bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorReport {
    /// Timestamp (millis since epoch).
    pub timestamp: i64,
    /// Device identifier (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Error type classification.
    pub error_type: ErrorType,
    /// Error message.
    pub message: String,
    /// Whether the error is retryable.
    pub retryable: bool,
}

/// Correlation entry from cross-bridge device correlation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationEntry {
    /// Primary IP address (as string for frontend compatibility).
    pub ip: String,
    /// All known hostnames across bridges.
    pub hostnames: Vec<String>,
    /// Bridges that have seen this device.
    pub bridges: Vec<String>,
    /// Source IDs per bridge.
    pub sources: HashMap<String, String>,
    /// Last update timestamp.
    pub last_updated: i64,
}

/// Bridge discovery information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeInfo {
    /// Bridge name (e.g., "snmp", "syslog").
    pub name: String,
    /// Bridge version.
    pub version: String,
    /// Key prefix used by this bridge.
    pub key_prefix: String,
    /// Protocol handled.
    pub protocol: String,
    /// Number of devices being monitored.
    pub device_count: u64,
    /// Bridge status.
    pub status: String,
    /// Last heartbeat timestamp.
    pub last_heartbeat: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_status_default() {
        assert_eq!(DeviceStatus::default(), DeviceStatus::Unknown);
    }

    #[test]
    fn test_device_status_display() {
        assert_eq!(format!("{}", DeviceStatus::Online), "online");
        assert_eq!(format!("{}", DeviceStatus::Offline), "offline");
        assert_eq!(format!("{}", DeviceStatus::Degraded), "degraded");
        assert_eq!(format!("{}", DeviceStatus::Unknown), "unknown");
    }

    #[test]
    fn test_health_snapshot_deserialize() {
        let json = r#"{
            "bridge": "snmp",
            "status": "healthy",
            "uptime_secs": 3600,
            "devices_total": 10,
            "devices_responding": 9,
            "devices_failed": 1,
            "last_poll_duration_ms": 150,
            "errors_last_hour": 5,
            "metrics_published": 1000
        }"#;

        let snapshot: HealthSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.bridge, "snmp");
        assert_eq!(snapshot.devices_total, 10);
    }

    #[test]
    fn test_device_liveness_deserialize() {
        let json = r#"{
            "device": "router01",
            "status": "online",
            "last_seen": 1703500000000,
            "consecutive_failures": 0
        }"#;

        let liveness: DeviceLiveness = serde_json::from_str(json).unwrap();
        assert_eq!(liveness.device, "router01");
        assert_eq!(liveness.status, DeviceStatus::Online);
        assert!(liveness.last_error.is_none());
    }

    #[test]
    fn test_correlation_entry_deserialize() {
        let json = r#"{
            "ip": "10.0.0.1",
            "hostnames": ["router01", "router01.local"],
            "bridges": ["snmp", "syslog"],
            "sources": {"snmp": "router01", "syslog": "router01.local"},
            "last_updated": 1703500000000
        }"#;

        let entry: CorrelationEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.ip, "10.0.0.1");
        assert_eq!(entry.bridges.len(), 2);
    }
}
