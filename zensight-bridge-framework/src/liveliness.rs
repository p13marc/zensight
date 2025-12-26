//! Liveliness tokens for presence detection.
//!
//! This module provides Zenoh liveliness token management for bridges and devices.
//! Liveliness tokens allow the frontend to instantly detect when bridges or devices
//! come online or go offline.
//!
//! # Key Expressions
//!
//! - Bridge liveliness: `zensight/<protocol>/@/alive`
//! - Device liveliness: `zensight/<protocol>/@/devices/<device_id>/alive`
//!
//! # Example
//!
//! ```ignore
//! use zensight_bridge_framework::LivelinessManager;
//!
//! let manager = LivelinessManager::new(session.clone(), "zensight/snmp").await?;
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

use crate::error::{BridgeError, Result};

/// Manages liveliness tokens for a bridge and its devices.
///
/// The bridge token is declared on creation and automatically undeclared on drop.
/// Device tokens can be declared/undeclared as devices come online/offline.
#[derive(Debug)]
pub struct LivelinessManager {
    /// Zenoh session.
    session: Arc<Session>,
    /// Key prefix (e.g., "zensight/snmp").
    key_prefix: String,
    /// Bridge-level liveliness token.
    /// Kept alive for the lifetime of the manager.
    #[allow(dead_code)]
    bridge_token: LivelinessToken,
    /// Per-device liveliness tokens.
    device_tokens: RwLock<HashMap<String, LivelinessToken>>,
}

impl LivelinessManager {
    /// Create a new liveliness manager and declare the bridge as alive.
    ///
    /// The bridge liveliness token is declared immediately at:
    /// `<key_prefix>/@/alive`
    ///
    /// For example: `zensight/snmp/@/alive`
    pub async fn new(session: Arc<Session>, key_prefix: impl Into<String>) -> Result<Self> {
        let key_prefix = key_prefix.into();
        let bridge_key = format!("{}/@/alive", key_prefix);

        let bridge_token = session
            .liveliness()
            .declare_token(&bridge_key)
            .await
            .map_err(|e| {
                BridgeError::liveliness(format!("Failed to declare bridge token: {}", e))
            })?;

        tracing::info!(key = %bridge_key, "Bridge liveliness token declared");

        Ok(Self {
            session,
            key_prefix,
            bridge_token,
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

        let device_key = format!("{}/@/devices/{}/alive", self.key_prefix, device_id);

        let token = self
            .session
            .liveliness()
            .declare_token(&device_key)
            .await
            .map_err(|e| {
                BridgeError::liveliness(format!(
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
    use super::*;

    // Note: These tests require a Zenoh session which we can't easily mock.
    // Integration tests should cover liveliness functionality.

    #[test]
    fn test_key_format() {
        // Test the key format logic
        let prefix = "zensight/snmp";
        let bridge_key = format!("{}/@/alive", prefix);
        assert_eq!(bridge_key, "zensight/snmp/@/alive");

        let device_key = format!("{}/@/devices/{}/alive", prefix, "router01");
        assert_eq!(device_key, "zensight/snmp/@/devices/router01/alive");
    }
}
