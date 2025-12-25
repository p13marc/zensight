//! Bridge status reporting.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::publisher::Publisher;

/// Bridge status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeStatus {
    /// Bridge name (e.g., "snmp", "syslog").
    pub bridge: String,
    /// Bridge version.
    pub version: String,
    /// Current status ("running", "offline", "error").
    pub status: String,
    /// Additional metadata (protocol-specific).
    #[serde(flatten)]
    pub metadata: serde_json::Value,
}

impl BridgeStatus {
    /// Create a new status with "running" state.
    pub fn running(bridge: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            bridge: bridge.into(),
            version: version.into(),
            status: "running".to_string(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Create a status with "offline" state.
    pub fn offline(bridge: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            bridge: bridge.into(),
            version: version.into(),
            status: "offline".to_string(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Create a status with "error" state.
    pub fn error(
        bridge: impl Into<String>,
        version: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            bridge: bridge.into(),
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
    /// Publishes to `{key_prefix}/@/status`.
    pub async fn publish(&self, publisher: &Publisher) -> Result<()> {
        let key = format!("{}/@/status", publisher.key_prefix());
        publisher.publish_json(&key, self).await
    }
}

/// Helper to publish bridge status on startup and shutdown.
pub struct StatusPublisher {
    publisher: Publisher,
    bridge_name: String,
    version: String,
}

impl StatusPublisher {
    /// Create a new status publisher.
    pub fn new(
        publisher: Publisher,
        bridge_name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            publisher,
            bridge_name: bridge_name.into(),
            version: version.into(),
        }
    }

    /// Publish "running" status with optional metadata.
    pub async fn publish_running(&self, metadata: Option<serde_json::Value>) -> Result<()> {
        let mut status = BridgeStatus::running(&self.bridge_name, &self.version);
        if let Some(meta) = metadata {
            status = status.with_metadata(meta);
        }
        status.publish(&self.publisher).await
    }

    /// Publish "offline" status.
    pub async fn publish_offline(&self) -> Result<()> {
        BridgeStatus::offline(&self.bridge_name, &self.version)
            .publish(&self.publisher)
            .await
    }

    /// Publish "error" status.
    pub async fn publish_error(&self, error: impl Into<String>) -> Result<()> {
        BridgeStatus::error(&self.bridge_name, &self.version, error)
            .publish(&self.publisher)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_running() {
        let status = BridgeStatus::running("snmp", "0.1.0");
        assert_eq!(status.bridge, "snmp");
        assert_eq!(status.status, "running");
    }

    #[test]
    fn test_status_with_metadata() {
        let status = BridgeStatus::running("modbus", "0.1.0").with_metadata(serde_json::json!({
            "devices": ["plc01", "plc02"],
            "poll_interval": 30
        }));

        assert_eq!(status.metadata["devices"][0], "plc01");
        assert_eq!(status.metadata["poll_interval"], 30);
    }

    #[test]
    fn test_status_serialization() {
        let status =
            BridgeStatus::running("test", "1.0.0").with_metadata(serde_json::json!({ "count": 5 }));

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"bridge\":\"test\""));
        assert!(json.contains("\"status\":\"running\""));
        assert!(json.contains("\"count\":5"));
    }
}
