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
//! use zensight_sensor_core::AdvancedPublisherRegistry;
//!
//! // The prefix is the v1 telemetry prefix from `V1Context::telemetry_prefix()`
//! // (`zensight/v1/<origin>/telemetry/<producer>`).
//! let registry = AdvancedPublisherRegistry::new(
//!     session.clone(),
//!     ctx.telemetry_prefix(),
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

use zensight_common::{Format, QosClass, TelemetryPoint, encode};

use crate::error::{Result, SensorError};

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
    /// Default: 5s (relaxed from 500ms — periodic telemetry is superseded by the next
    /// sample, so a fast heartbeat mostly adds background traffic on a low-bandwidth link).
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
            heartbeat_interval: Duration::from_secs(5),
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
    /// The v1 telemetry prefix (`zensight/v1/<origin>/telemetry/<producer>`).
    telemetry_prefix: String,
    /// Configuration for new publishers.
    config: AdvancedPublisherConfig,
    /// Serialization format.
    format: Format,
    /// QoS class applied to every publisher declared by this registry
    /// (default [`QosClass::Telemetry`]; override with [`Self::with_qos`] for
    /// must-arrive feeds like evidence).
    qos: QosClass,
    /// Cached publishers by key expression, each beside the class it was
    /// **declared** with — so a put under another class is reported (#1155),
    /// on this tier as on the baseline one.
    publishers: RwLock<HashMap<String, (AdvancedPublisher<'static>, QosClass)>>,
    /// Watches every point published here (#930).
    ///
    /// This registry is a **second** publish path, independent of
    /// `PublisherRegistry` — and the one netlink, netring, snmp and logs
    /// actually use for the bulk of their telemetry. A threshold evaluator
    /// installed only on the other one would have missed most of the fleet's
    /// points while looking like it saw them all.
    observer: std::sync::OnceLock<Arc<dyn zensight_common::point_observer::PointObserver>>,
    /// Publish counters (#1079). **Supplied at construction, never fresh by
    /// default** (#1155): a sensor passes its baseline publisher's set so the
    /// health doc's `published_total` counts this tier too — for netlink,
    /// netring, snmp and logs this is where the bulk of the telemetry goes,
    /// and one forgotten `with_counters` made `published_total` orders of
    /// magnitude low. `RelationSet` had exactly that omission.
    counters: Arc<zensight_common::PublishCounters>,
}

impl std::fmt::Debug for AdvancedPublisherRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvancedPublisherRegistry")
            .field("telemetry_prefix", &self.telemetry_prefix)
            .field("config", &self.config)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl AdvancedPublisherRegistry {
    /// Create a new advanced publisher registry.
    ///
    /// `counters` is normally the baseline
    /// [`Publisher::counters`](crate::Publisher::counters) of the same sensor,
    /// so every tier's deliveries land in the one number the health doc
    /// publishes (#1079). It is an argument rather than a builder step so a
    /// registry cannot be built counting into nothing (#1155).
    pub fn new(
        session: Arc<Session>,
        telemetry_prefix: impl Into<String>,
        format: Format,
        config: AdvancedPublisherConfig,
        counters: Arc<zensight_common::PublishCounters>,
    ) -> Self {
        Self {
            session,
            telemetry_prefix: telemetry_prefix.into(),
            config,
            format,
            qos: QosClass::Telemetry,
            publishers: RwLock::new(HashMap::new()),
            observer: std::sync::OnceLock::new(),
            counters,
        }
    }

    /// This registry's publish counters.
    pub fn counters(&self) -> Arc<zensight_common::PublishCounters> {
        self.counters.clone()
    }

    /// Override the QoS class for publishers declared by this registry (default
    /// [`QosClass::Telemetry`]). Use for must-arrive feeds, e.g.
    /// `AdvancedPublisherRegistry::new(..).with_qos(QosClass::Evidence)`.
    pub fn with_qos(mut self, qos: QosClass) -> Self {
        self.qos = qos;
        self
    }

    /// Get the key prefix.
    pub fn telemetry_prefix(&self) -> &str {
        &self.telemetry_prefix
    }

    /// Get the configuration.
    pub fn config(&self) -> &AdvancedPublisherConfig {
        &self.config
    }

    /// Build a full key expression from a suffix.
    ///
    /// The registry-conformance guard (RFC 08 §5) runs on the put, not here
    /// (#1155): this tier declares its own publishers rather than going
    /// through [`zensight_common::PublisherRegistry`], and guarding only the
    /// keys *it* built left `publish_to_key`, `publish_serializable` and
    /// `tombstone` unchecked.
    fn build_key(&self, suffix: &str) -> String {
        if suffix.is_empty() {
            self.telemetry_prefix.clone()
        } else {
            format!("{}/{}", self.telemetry_prefix, suffix)
        }
    }

    /// Get or create an advanced publisher for the given key.
    async fn get_or_create_publisher(&self, key: &str) -> Result<()> {
        // Take write lock upfront to avoid TOCTOU race between read-check and write-insert
        let mut publishers = self.publishers.write().await;
        if publishers.contains_key(key) {
            return Ok(());
        }

        // Pass owned String to declare_publisher so the resulting KeyExpr uses the
        // Owned variant (KeyExprInner::Owned), making the publisher genuinely 'static.
        // This avoids needing unsafe transmute since String -> KeyExpr<'_> produces
        // KeyExpr<'static> via TryFrom<String>.
        let owned_key = key.to_string();
        // Build conditionally so `cache_only` configs are genuinely cache-only: apply
        // miss-detection (and its heartbeat) and publisher-detection only when enabled.
        // Previously these were attached unconditionally, so every publisher — including
        // cache-only identity/evidence publishers — emitted a heartbeat per key, a constant
        // background stream a low-bandwidth link cannot shed.
        let mut builder = self
            .session
            .declare_publisher(owned_key)
            .congestion_control(self.qos.congestion_control())
            .priority(self.qos.priority())
            .express(self.qos.express())
            .reliability(self.qos.reliability())
            .cache(CacheConfig::default().max_samples(self.config.cache_size));
        if self.config.miss_detection {
            builder = builder.sample_miss_detection(
                MissDetectionConfig::default().heartbeat(self.config.heartbeat_interval),
            );
        }
        if self.config.publisher_detection {
            builder = builder.publisher_detection();
        }
        let publisher: AdvancedPublisher<'static> =
            builder.await.map_err(|e| SensorError::Publish {
                key: key.to_string(),
                message: format!("Failed to create advanced publisher: {}", e),
            })?;

        publishers.insert(key.to_string(), (publisher, self.qos));

        tracing::debug!(key = %key, cache_size = %self.config.cache_size, "Created advanced publisher");

        Ok(())
    }

    /// Publish a telemetry point using an advanced publisher.
    ///
    /// The publisher for this key is created on first use and cached.
    pub async fn publish(&self, key_suffix: &str, point: &TelemetryPoint) -> Result<()> {
        let key = self.build_key(key_suffix);
        self.publish_to_key(&key, point).await
    }

    /// Install a point observer (#930). Idempotent-by-first-call.
    pub fn set_observer(&self, observer: Arc<dyn zensight_common::point_observer::PointObserver>) {
        let _ = self.observer.set(observer);
    }

    /// Publish a telemetry point to a full key (bypassing the prefix), via an
    /// advanced publisher created on first use for that key.
    pub async fn publish_to_key(&self, key: &str, point: &TelemetryPoint) -> Result<()> {
        self.observe(key, point);
        let payload =
            encode(point, self.format).map_err(|e| SensorError::Serialization(e.to_string()))?;
        self.put_raw_as(key, payload, self.qos, self.format.encoding())
            .await
    }

    /// The threshold seam (#930): every point this tier publishes passes the
    /// installed observer while it is still a `TelemetryPoint`.
    pub(crate) fn observe(&self, key: &str, point: &TelemetryPoint) {
        if let Some(observer) = self.observer.get() {
            observer.observe_point(key, point);
        }
    }

    /// Tombstone a full key through its cached advanced publisher.
    ///
    /// The publisher matters: a `Delete` sent through the session would not
    /// enter this key's advanced-publisher cache, so a late joiner replaying
    /// the cache would receive the last `Put` and never learn the document was
    /// retired. Retiring through the same publisher that wrote it is what
    /// makes the tombstone as durable as the value it retires.
    pub async fn tombstone(&self, key: &str) -> Result<()> {
        self.tombstone_as(key, self.qos).await
    }

    /// [`Self::tombstone`] under an explicit class — reported, not applied,
    /// when the key was declared under another (#1155). Guarded like a put.
    pub(crate) async fn tombstone_as(&self, key: &str, asked: QosClass) -> Result<()> {
        zensight_common::metric_guard::check_telemetry_key(key);
        {
            let publishers = self.publishers.read().await;
            if let Some((publisher, declared)) = publishers.get(key) {
                zensight_common::PublisherRegistry::check_class(key, *declared, asked);
                return publisher.delete().await.map_err(|e| SensorError::Publish {
                    key: key.to_string(),
                    message: e.to_string(),
                });
            }
        }
        self.get_or_create_publisher(key).await?;
        let publishers = self.publishers.read().await;
        match publishers.get(key) {
            Some((publisher, declared)) => {
                zensight_common::PublisherRegistry::check_class(key, *declared, asked);
                publisher.delete().await.map_err(|e| SensorError::Publish {
                    key: key.to_string(),
                    message: e.to_string(),
                })
            }
            None => Err(SensorError::Publish {
                key: key.to_string(),
                message: "publisher vanished between create and use".into(),
            }),
        }
    }

    /// Publish any serializable document to a full key (bypassing the prefix),
    /// via an advanced publisher created on first use for that key.
    ///
    /// Used for non-telemetry control-plane docs (`SensorInfo`, `HostEvidence`,
    /// ... — identity envelope, #301) that want the same cached-publisher
    /// late-joiner semantics as telemetry. Encoded with the registry's format.
    pub async fn publish_serializable<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<()> {
        let payload =
            encode(value, self.format).map_err(|e| SensorError::Serialization(e.to_string()))?;
        self.put_raw_as(key, payload, self.qos, self.format.encoding())
            .await
    }

    /// Shared put path. Fast path: one read-lock hit on the cached publisher
    /// (the steady state — every publish used to take the write lock first).
    /// Miss: create, then put under a fresh read lock. Every sample carries an
    /// [`Encoding`](zenoh::bytes::Encoding) (RFC 08 §7: metadata beats
    /// sniffing). The registry guard runs here, on **every** put (#1155), and
    /// a key declared under another class than `asked` is reported.
    pub(crate) async fn put_raw_as(
        &self,
        key: &str,
        payload: Vec<u8>,
        asked: QosClass,
        encoding: zenoh::bytes::Encoding,
    ) -> Result<()> {
        zensight_common::metric_guard::check_telemetry_key(key);
        let bytes = payload.len();
        {
            let publishers = self.publishers.read().await;
            if let Some((publisher, declared)) = publishers.get(key) {
                zensight_common::PublisherRegistry::check_class(key, *declared, asked);
                publisher
                    .put(payload)
                    .encoding(encoding)
                    .await
                    .map_err(|e| SensorError::Publish {
                        key: key.to_string(),
                        message: e.to_string(),
                    })?;
                self.counters.record_publish(bytes);
                return Ok(());
            }
        }
        self.get_or_create_publisher(key).await?;
        let publishers = self.publishers.read().await;
        // A publisher that was just created and is not in the map is a bug,
        // not a success: `tombstone` already treats it as one, and a put that
        // returns `Ok(())` without publishing is the one outcome a caller
        // cannot detect (#1079).
        let Some((publisher, declared)) = publishers.get(key) else {
            return Err(SensorError::Publish {
                key: key.to_string(),
                message: "publisher missing after creation".to_string(),
            });
        };
        zensight_common::PublisherRegistry::check_class(key, *declared, asked);
        publisher
            .put(payload)
            .encoding(encoding)
            .await
            .map_err(|e| SensorError::Publish {
                key: key.to_string(),
                message: e.to_string(),
            })?;
        self.counters.record_publish(bytes);
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

// One `PublishStats`, not two: the batch outcome is the same shape on both
// tiers, and this module used to carry a byte-for-byte copy (#1155).
pub use crate::publisher::PublishStats;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = AdvancedPublisherConfig::default();
        assert_eq!(config.cache_size, 10);
        assert!(config.miss_detection);
        assert!(config.publisher_detection);
        assert_eq!(config.heartbeat_interval, Duration::from_secs(5));
    }

    #[test]
    fn test_config_cache_only() {
        // cache_only must disable miss-detection AND publisher-detection so
        // `get_or_create_publisher` attaches neither the sample-miss listener nor the
        // heartbeat — the builder now honors these flags (previously ignored).
        let config = AdvancedPublisherConfig::cache_only(50);
        assert_eq!(config.cache_size, 50);
        assert!(!config.miss_detection);
        assert!(!config.publisher_detection);
    }
}
