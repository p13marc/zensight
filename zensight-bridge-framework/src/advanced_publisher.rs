//! Advanced publisher with caching and sample miss detection.
//!
//! This module provides an [`AdvancedPublisherRegistry`] that manages
//! zenoh-ext advanced publishers with caching for late-joining subscribers.
//!
//! # Features
//!
//! - **Cache**: Publishers cache the last N samples for each key expression
//! - **Sample miss detection**: Enables subscribers to detect and recover missed samples
//! - **Publisher detection**: Allows subscribers to know when publishers appear/disappear
//!
//! # Example
//!
//! ```ignore
//! use zensight_bridge_framework::AdvancedPublisherRegistry;
//!
//! let registry = AdvancedPublisherRegistry::new(
//!     session.clone(),
//!     "zensight/snmp",
//!     AdvancedPublisherConfig::default(),
//! ).await?;
//!
//! // Publish a telemetry point (publisher is created on first use)
//! registry.publish("router01/cpu", &point).await?;
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use zenoh::Session;
use zenoh_ext::{AdvancedPublisher, AdvancedPublisherBuilderExt, CacheConfig, MissDetectionConfig};

use zensight_common::{Format, TelemetryPoint, encode};

use crate::error::{BridgeError, Result};

/// Configuration for advanced publishers.
#[derive(Debug, Clone)]
pub struct AdvancedPublisherConfig {
    /// Number of samples to cache per key expression.
    /// Default: 10
    pub cache_size: usize,

    /// Enable sample miss detection.
    /// Default: true
    pub miss_detection: bool,

    /// Heartbeat interval for miss detection.
    /// Default: 500ms
    pub heartbeat_interval: Duration,

    /// Enable publisher detection (allows subscribers to detect this publisher).
    /// Default: true
    pub publisher_detection: bool,
}

impl Default for AdvancedPublisherConfig {
    fn default() -> Self {
        Self {
            cache_size: 10,
            miss_detection: true,
            heartbeat_interval: Duration::from_millis(500),
            publisher_detection: true,
        }
    }
}

impl AdvancedPublisherConfig {
    /// Create a minimal config with only caching enabled.
    pub fn cache_only(cache_size: usize) -> Self {
        Self {
            cache_size,
            miss_detection: false,
            heartbeat_interval: Duration::from_millis(500),
            publisher_detection: false,
        }
    }

    /// Create a full-featured config.
    pub fn full(cache_size: usize, heartbeat_ms: u64) -> Self {
        Self {
            cache_size,
            miss_detection: true,
            heartbeat_interval: Duration::from_millis(heartbeat_ms),
            publisher_detection: true,
        }
    }
}

/// Registry for managing advanced publishers.
///
/// Creates and caches [`AdvancedPublisher`] instances for each key expression.
/// Publishers are created lazily on first publish to that key.
pub struct AdvancedPublisherRegistry {
    /// Zenoh session.
    session: Arc<Session>,
    /// Key prefix (e.g., "zensight/snmp").
    key_prefix: String,
    /// Configuration for new publishers.
    config: AdvancedPublisherConfig,
    /// Serialization format.
    format: Format,
    /// Cached publishers by key expression.
    publishers: RwLock<HashMap<String, AdvancedPublisher<'static>>>,
}

impl std::fmt::Debug for AdvancedPublisherRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvancedPublisherRegistry")
            .field("key_prefix", &self.key_prefix)
            .field("config", &self.config)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl AdvancedPublisherRegistry {
    /// Create a new advanced publisher registry.
    pub fn new(
        session: Arc<Session>,
        key_prefix: impl Into<String>,
        format: Format,
        config: AdvancedPublisherConfig,
    ) -> Self {
        Self {
            session,
            key_prefix: key_prefix.into(),
            config,
            format,
            publishers: RwLock::new(HashMap::new()),
        }
    }

    /// Get the key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Get the configuration.
    pub fn config(&self) -> &AdvancedPublisherConfig {
        &self.config
    }

    /// Build a full key expression from a suffix.
    fn build_key(&self, suffix: &str) -> String {
        if suffix.is_empty() {
            self.key_prefix.clone()
        } else {
            format!("{}/{}", self.key_prefix, suffix)
        }
    }

    /// Get or create an advanced publisher for the given key.
    async fn get_or_create_publisher(&self, key: &str) -> Result<()> {
        // Check if publisher already exists
        {
            let publishers = self.publishers.read().await;
            if publishers.contains_key(key) {
                return Ok(());
            }
        }

        // Create new publisher with cache, miss detection, and publisher detection
        let publisher: AdvancedPublisher<'_> = self
            .session
            .declare_publisher(key)
            .cache(CacheConfig::default().max_samples(self.config.cache_size))
            .sample_miss_detection(
                MissDetectionConfig::default().heartbeat(self.config.heartbeat_interval),
            )
            .publisher_detection()
            .await
            .map_err(|e| BridgeError::Publish {
                key: key.to_string(),
                message: format!("Failed to create advanced publisher: {}", e),
            })?;

        // Store the publisher
        // Safety: We're using 'static lifetime because the publisher is stored
        // in the registry and the session is kept alive by Arc
        let publisher: AdvancedPublisher<'static> = unsafe { std::mem::transmute(publisher) };

        let mut publishers = self.publishers.write().await;
        publishers.insert(key.to_string(), publisher);

        tracing::debug!(key = %key, cache_size = %self.config.cache_size, "Created advanced publisher");

        Ok(())
    }

    /// Publish a telemetry point using an advanced publisher.
    ///
    /// The publisher for this key is created on first use and cached.
    pub async fn publish(&self, key_suffix: &str, point: &TelemetryPoint) -> Result<()> {
        let key = self.build_key(key_suffix);

        // Ensure publisher exists
        self.get_or_create_publisher(&key).await?;

        // Encode the payload
        let payload =
            encode(point, self.format).map_err(|e| BridgeError::Serialization(e.to_string()))?;

        // Publish through the cached publisher
        let publishers = self.publishers.read().await;
        if let Some(publisher) = publishers.get(&key) {
            publisher
                .put(payload)
                .await
                .map_err(|e| BridgeError::Publish {
                    key: key.clone(),
                    message: e.to_string(),
                })?;
        }

        Ok(())
    }

    /// Publish a batch of telemetry points.
    ///
    /// Returns statistics about the batch operation.
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

    /// Get the number of active publishers.
    pub async fn publisher_count(&self) -> usize {
        self.publishers.read().await.len()
    }

    /// Clear all cached publishers.
    ///
    /// New publishers will be created on the next publish.
    pub async fn clear(&self) {
        let mut publishers = self.publishers.write().await;
        publishers.clear();
        tracing::debug!("Cleared all advanced publishers");
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
    fn test_config_defaults() {
        let config = AdvancedPublisherConfig::default();
        assert_eq!(config.cache_size, 10);
        assert!(config.miss_detection);
        assert!(config.publisher_detection);
        assert_eq!(config.heartbeat_interval, Duration::from_millis(500));
    }

    #[test]
    fn test_config_cache_only() {
        let config = AdvancedPublisherConfig::cache_only(50);
        assert_eq!(config.cache_size, 50);
        assert!(!config.miss_detection);
        assert!(!config.publisher_detection);
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
