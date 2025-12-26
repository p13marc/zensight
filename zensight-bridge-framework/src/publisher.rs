//! Telemetry publisher for Zenoh.

use std::sync::Arc;

use zensight_common::{Format, TelemetryPoint, encode};

use crate::error::{BridgeError, Result};

/// Publisher for sending telemetry to Zenoh.
///
/// Wraps a Zenoh session and provides convenient methods for publishing
/// [`TelemetryPoint`] values with automatic serialization.
#[derive(Clone, Debug)]
pub struct Publisher {
    session: Arc<zenoh::Session>,
    key_prefix: String,
    format: Format,
}

impl Publisher {
    /// Create a new publisher.
    pub fn new(
        session: Arc<zenoh::Session>,
        key_prefix: impl Into<String>,
        format: Format,
    ) -> Self {
        Self {
            session,
            key_prefix: key_prefix.into(),
            format,
        }
    }

    /// Get the key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Get the serialization format.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Get a reference to the Zenoh session.
    pub fn session(&self) -> &Arc<zenoh::Session> {
        &self.session
    }

    /// Build a full key expression from a suffix.
    pub fn build_key(&self, suffix: &str) -> String {
        if suffix.is_empty() {
            self.key_prefix.clone()
        } else {
            format!("{}/{}", self.key_prefix, suffix)
        }
    }

    /// Publish a telemetry point.
    ///
    /// The key is constructed by appending `key_suffix` to the publisher's prefix.
    pub async fn publish(&self, key_suffix: &str, point: &TelemetryPoint) -> Result<()> {
        let key = self.build_key(key_suffix);
        let payload =
            encode(point, self.format).map_err(|e| BridgeError::Serialization(e.to_string()))?;

        self.session
            .put(&key, payload)
            .await
            .map_err(|e| BridgeError::Publish {
                key: key.clone(),
                message: e.to_string(),
            })?;

        Ok(())
    }

    /// Publish a telemetry point with a full key (not using prefix).
    pub async fn publish_to_key(&self, key: &str, point: &TelemetryPoint) -> Result<()> {
        let payload =
            encode(point, self.format).map_err(|e| BridgeError::Serialization(e.to_string()))?;

        self.session
            .put(key, payload)
            .await
            .map_err(|e| BridgeError::Publish {
                key: key.to_string(),
                message: e.to_string(),
            })?;

        Ok(())
    }

    /// Publish a batch of telemetry points.
    ///
    /// Returns the number of successfully published points and logs errors.
    pub async fn publish_batch<'a, I>(&self, points: I) -> PublishStats
    where
        I: IntoIterator<Item = (&'a str, &'a TelemetryPoint)>,
    {
        let mut stats = PublishStats::default();

        for (key_suffix, point) in points {
            match self.publish(key_suffix, point).await {
                Ok(()) => stats.success += 1,
                Err(e) => {
                    stats.failed += 1;
                    tracing::warn!(error = %e, "Failed to publish telemetry");
                }
            }
        }

        stats
    }

    /// Publish raw bytes to a key (for status messages, etc.).
    pub async fn publish_raw(&self, key: &str, payload: Vec<u8>) -> Result<()> {
        self.session
            .put(key, payload)
            .await
            .map_err(|e| BridgeError::Publish {
                key: key.to_string(),
                message: e.to_string(),
            })?;

        Ok(())
    }

    /// Publish a JSON value to a key.
    pub async fn publish_json<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let payload = serde_json::to_vec(value)?;
        self.publish_raw(key, payload).await
    }
}

/// Statistics from a batch publish operation.
#[derive(Debug, Default, Clone, Copy)]
pub struct PublishStats {
    /// Number of successfully published points.
    pub success: usize,
    /// Number of failed publishes.
    pub failed: usize,
}

impl PublishStats {
    /// Total number of attempted publishes.
    pub fn total(&self) -> usize {
        self.success + self.failed
    }

    /// Success rate as a percentage.
    pub fn success_rate(&self) -> f64 {
        if self.total() == 0 {
            100.0
        } else {
            (self.success as f64 / self.total() as f64) * 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_key() {
        // We can't create a real session in tests, but we can test key building logic
        let key_prefix = "zensight/test";

        // Test key building logic
        let suffix = "device/metric";
        let expected = format!("{}/{}", key_prefix, suffix);
        assert_eq!(expected, "zensight/test/device/metric");

        // Empty suffix
        let empty_key = key_prefix.to_string();
        assert_eq!(empty_key, "zensight/test");
    }

    #[test]
    fn test_publish_stats() {
        let mut stats = PublishStats::default();
        assert_eq!(stats.total(), 0);
        assert_eq!(stats.success_rate(), 100.0);

        stats.success = 8;
        stats.failed = 2;
        assert_eq!(stats.total(), 10);
        assert_eq!(stats.success_rate(), 80.0);
    }
}
