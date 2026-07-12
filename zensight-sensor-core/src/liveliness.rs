//! Liveliness tokens for presence detection.
//!
//! This module provides Zenoh liveliness token management for sensors and devices.
//! Liveliness tokens allow the frontend to instantly detect when sensors or devices
//! come online or go offline.
//!
//! # Key Expressions
//!
//! The manager takes the sensor instance's host-scoped control prefix
//! (`zensight/<protocol>/<source>`), so two hosts running the same protocol
//! hold distinct tokens:
//!
//! - Sensor liveliness: `zensight/<protocol>/<source>/@/alive`
//! - Device liveliness: `zensight/<protocol>/<source>/@/devices/<device_id>/alive`
//!
//! # Example
//!
//! ```ignore
//! use zensight_sensor_core::LivelinessManager;
//!
//! let manager = LivelinessManager::new(session.clone(), "zensight/snmp/poller01").await?;
//!
//! // Declare device as alive
//! manager.declare_device_alive("router01").await?;
//!
//! // Device went offline
//! manager.undeclare_device("router01").await;
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use zenoh::Session;
use zenoh::liveliness::LivelinessToken;

use crate::error::{Result, SensorError};

/// Manages liveliness tokens for a sensor and its devices.
///
/// The sensor token is declared on creation and automatically undeclared on drop.
/// Device tokens can be declared/undeclared as devices come online/offline.
#[derive(Debug)]
pub struct LivelinessManager {
    /// Zenoh session.
    session: Arc<Session>,
    /// Host-scoped control prefix (e.g., "zensight/snmp/poller01").
    key_prefix: String,
    /// Sensor-level liveliness token.
    /// Kept alive for the lifetime of the manager.
    #[allow(dead_code)]
    sensor_token: LivelinessToken,
    /// Per-device liveliness tokens.
    device_tokens: RwLock<HashMap<String, LivelinessToken>>,
}

impl LivelinessManager {
    /// Create a new liveliness manager and declare the sensor as alive.
    ///
    /// The sensor liveliness token is declared immediately at:
    /// `<key_prefix>/@/alive`
    ///
    /// For example: `zensight/snmp/poller01/@/alive`
    pub async fn new(session: Arc<Session>, key_prefix: impl Into<String>) -> Result<Self> {
        let key_prefix = key_prefix.into();
        let sensor_key = crate::keys::alive_key(&key_prefix);

        let sensor_token = session
            .liveliness()
            .declare_token(&sensor_key)
            .await
            .map_err(|e| {
                SensorError::liveliness(format!("Failed to declare sensor token: {}", e))
            })?;

        tracing::info!(key = %sensor_key, "Sensor liveliness token declared");

        Ok(Self {
            session,
            key_prefix,
            sensor_token,
            device_tokens: RwLock::new(HashMap::new()),
        })
    }

    /// Declare a device as alive.
    ///
    /// Creates a liveliness token at:
    /// `<key_prefix>/@/devices/<device_id>/alive`
    ///
    /// If the device already has a token, this is a no-op.
    pub async fn declare_device_alive(&self, device_id: &str) -> Result<()> {
        // Check if already declared
        {
            let tokens = self.device_tokens.read().await;
            if tokens.contains_key(device_id) {
                return Ok(());
            }
        }

        let device_key = crate::keys::device_alive_key(&self.key_prefix, device_id);

        let token = self
            .session
            .liveliness()
            .declare_token(&device_key)
            .await
            .map_err(|e| {
                SensorError::liveliness(format!(
                    "Failed to declare device token for {}: {}",
                    device_id, e
                ))
            })?;

        tracing::debug!(device = %device_id, key = %device_key, "Device liveliness token declared");

        let mut tokens = self.device_tokens.write().await;
        tokens.insert(device_id.to_string(), token);

        Ok(())
    }

    /// Undeclare a device (mark as offline).
    ///
    /// Removes the liveliness token for the device. The frontend will
    /// receive a DELETE notification.
    pub async fn undeclare_device(&self, device_id: &str) {
        let mut tokens = self.device_tokens.write().await;
        if let Some(token) = tokens.remove(device_id) {
            // Token is dropped here, which undeclares it
            drop(token);
            tracing::debug!(device = %device_id, "Device liveliness token undeclared");
        }
    }

    /// Check if a device is currently declared as alive.
    pub async fn is_device_alive(&self, device_id: &str) -> bool {
        let tokens = self.device_tokens.read().await;
        tokens.contains_key(device_id)
    }

    /// Get the list of devices currently declared as alive.
    pub async fn alive_devices(&self) -> Vec<String> {
        let tokens = self.device_tokens.read().await;
        tokens.keys().cloned().collect()
    }

    /// Get the key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Undeclare all device tokens.
    ///
    /// Called automatically on drop, but can be called explicitly for cleanup.
    pub async fn undeclare_all_devices(&self) {
        let mut tokens = self.device_tokens.write().await;
        let count = tokens.len();
        tokens.clear();
        if count > 0 {
            tracing::debug!(count = count, "All device liveliness tokens undeclared");
        }
    }
}

#[cfg(test)]
mod tests {
    // Note: These tests require a Zenoh session which we can't easily mock.
    // Integration tests should cover liveliness functionality.

    #[test]
    fn test_key_format() {
        // The prefix is the host-scoped instance prefix, so tokens from two
        // hosts running the same protocol never collide.
        let prefix = "zensight/snmp/poller01";
        let sensor_key = format!("{}/@/alive", prefix);
        assert_eq!(sensor_key, "zensight/snmp/poller01/@/alive");

        let device_key = format!("{}/@/devices/{}/alive", prefix, "router01");
        assert_eq!(
            device_key,
            "zensight/snmp/poller01/@/devices/router01/alive"
        );
    }
}
