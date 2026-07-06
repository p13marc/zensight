//! Sensor status reporting.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::publisher::Publisher;

/// Sensor status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorStatus {
    /// Sensor name (e.g., "snmp", "syslog").
    pub sensor: String,
    /// Sensor version.
    pub version: String,
    /// Current status ("running", "offline", "error").
    pub status: String,
    /// Additional metadata (protocol-specific).
    #[serde(flatten)]
    pub metadata: serde_json::Value,
}

impl SensorStatus {
    /// Create a new status with "running" state.
    pub fn running(sensor: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            sensor: sensor.into(),
            version: version.into(),
            status: "running".to_string(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Create a status with "offline" state.
    pub fn offline(sensor: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            sensor: sensor.into(),
            version: version.into(),
            status: "offline".to_string(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Create a status with "error" state.
    pub fn error(
        sensor: impl Into<String>,
        version: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            sensor: sensor.into(),
            version: version.into(),
            status: "error".to_string(),
            metadata: serde_json::json!({ "error": error.into() }),
        }
    }

    /// Add metadata to the status.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Publish this status to Zenoh.
    ///
    /// Publishes to `{control_prefix}/@/status`, where the control prefix is
    /// the host-scoped instance prefix (`zensight/<protocol>/<source>`) so two
    /// hosts running the same protocol never overwrite each other's status.
    pub async fn publish(&self, publisher: &Publisher, control_prefix: &str) -> Result<()> {
        let key = format!("{}/@/status", control_prefix);
        publisher
            .publish_json(&key, self, zensight_common::QosClass::Command)
            .await
    }
}

/// Helper to publish sensor status on startup and shutdown.
pub struct StatusPublisher {
    publisher: Publisher,
    /// Host-scoped instance prefix (`zensight/<protocol>/<source>`) the
    /// `@/status` key hangs off.
    control_prefix: String,
    sensor_name: String,
    version: String,
}

impl StatusPublisher {
    /// Create a new status publisher.
    pub fn new(
        publisher: Publisher,
        control_prefix: impl Into<String>,
        sensor_name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            publisher,
            control_prefix: control_prefix.into(),
            sensor_name: sensor_name.into(),
            version: version.into(),
        }
    }

    /// Publish "running" status with optional metadata.
    pub async fn publish_running(&self, metadata: Option<serde_json::Value>) -> Result<()> {
        let mut status = SensorStatus::running(&self.sensor_name, &self.version);
        if let Some(meta) = metadata {
            status = status.with_metadata(meta);
        }
        status.publish(&self.publisher, &self.control_prefix).await
    }

    /// Publish "offline" status.
    pub async fn publish_offline(&self) -> Result<()> {
        SensorStatus::offline(&self.sensor_name, &self.version)
            .publish(&self.publisher, &self.control_prefix)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_running() {
        let status = SensorStatus::running("snmp", "0.1.0");
        assert_eq!(status.sensor, "snmp");
        assert_eq!(status.status, "running");
    }

    #[test]
    fn test_status_with_metadata() {
        let status = SensorStatus::running("modbus", "0.1.0").with_metadata(serde_json::json!({
            "devices": ["plc01", "plc02"],
            "poll_interval": 30
        }));

        assert_eq!(status.metadata["devices"][0], "plc01");
        assert_eq!(status.metadata["poll_interval"], 30);
    }

    #[test]
    fn test_status_serialization() {
        let status =
            SensorStatus::running("test", "1.0.0").with_metadata(serde_json::json!({ "count": 5 }));

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"sensor\":\"test\""));
        assert!(json.contains("\"status\":\"running\""));
        assert!(json.contains("\"count\":5"));
    }
}
