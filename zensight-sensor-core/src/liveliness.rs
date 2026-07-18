//! Liveliness tokens for presence detection.
//!
//! This module provides Zenoh liveliness token management for sensors and devices.
//! Liveliness tokens allow the frontend to instantly detect when sensors or devices
//! come online or go offline.
//!
//! # Key Expressions (v1, RFC 04 §5)
//!
//! The manager takes the sensor's [`V1Context`]; token keys mirror the state
//! grammar (origin-scoped, so two hosts never collide):
//!
//! - Sensor liveliness: `<base>/v1/<origin>/state/<producer>/alive`
//! - Device liveliness: `<base>/v1/<origin>/state/<producer>/device/<device>/alive`
//!
//! # Example
//!
//! ```ignore
//! use zensight_sensor_core::LivelinessManager;
//!
//! let manager = LivelinessManager::new(session.clone(), publisher.v1().clone()).await?;
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
use crate::v1::V1Context;

/// Manages liveliness tokens for a sensor and its devices.
///
/// The sensor token is declared on creation and automatically undeclared on drop.
/// Device tokens can be declared/undeclared as devices come online/offline.
#[derive(Debug)]
pub struct LivelinessManager {
    /// Zenoh session.
    session: Arc<Session>,
    /// The v1 key context (origin + producer) the token keys derive from.
    ctx: V1Context,
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
    /// The sensor liveliness token is declared immediately at
    /// `<base>/v1/<origin>/state/<producer>/alive` (RFC 04 §5).
    pub async fn new(session: Arc<Session>, ctx: V1Context) -> Result<Self> {
        let sensor_key = ctx.alive_key();

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
            ctx,
            sensor_token,
            device_tokens: RwLock::new(HashMap::new()),
        })
    }

    /// Declare a device as alive.
    ///
    /// Creates a liveliness token at
    /// `<base>/v1/<origin>/state/<producer>/device/<device>/alive`
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

        let device_key = self.ctx.device_alive_key(device_id);

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
        // Token keys mirror the v1 state grammar; origin-scoped so two hosts
        // running the same producer never collide (RFC 04 §5).
        let ctx = crate::v1::V1Context::for_producer(&zensight_common::PROFILE, "snmp");
        let sensor_key = ctx.alive_key();
        assert!(sensor_key.starts_with("v1/h-"), "{sensor_key}");
        assert!(sensor_key.ends_with("/state/snmp/alive"), "{sensor_key}");

        let device_key = ctx.device_alive_key("router01");
        assert!(
            device_key.ends_with("/state/snmp/device/router01/alive"),
            "{device_key}"
        );
    }
}
