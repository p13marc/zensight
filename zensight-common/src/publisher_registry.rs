//! A registry of **declared** plain Zenoh publishers, keyed by key expression.
//!
//! Every publication in ZenSight goes through a *declared* publisher rather than a
//! one-shot `session.put()`: declaring interns the key expression to a numeric id
//! and primes the routers' routing tables, so the full key string isn't re-resolved
//! and re-sent on every message — a real saving on a low-bandwidth hop. Publishers
//! are declared lazily on first use (with the key's [`QosClass`]) and cached, so a
//! dynamic key space (per-`alert_key`, per-device) still amortizes to one declared
//! publisher per key.
//!
//! This is the plain counterpart to the zenoh-ext `AdvancedPublisherRegistry` in
//! `zensight-sensor-core`: no cache/recovery/heartbeat, just declaration + QoS.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use zenoh::Session;
use zenoh::pubsub::Publisher;

use crate::error::Result;
use crate::qos::QosClass;

/// Caches one declared [`Publisher`] per key expression.
pub struct PublisherRegistry {
    session: Arc<Session>,
    publishers: RwLock<HashMap<String, Publisher<'static>>>,
}

impl std::fmt::Debug for PublisherRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherRegistry").finish_non_exhaustive()
    }
}

impl PublisherRegistry {
    /// Create a registry over a shared session.
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            publishers: RwLock::new(HashMap::new()),
        }
    }

    /// Declare (once) and cache the publisher for `key` with the class's QoS.
    async fn ensure(&self, key: &str, qos: QosClass) -> Result<()> {
        {
            if self.publishers.read().await.contains_key(key) {
                return Ok(());
            }
        }
        let mut publishers = self.publishers.write().await;
        if publishers.contains_key(key) {
            return Ok(());
        }
        // Owned String → `KeyExpr<'static>` so the cached publisher is `'static`.
        let publisher = self
            .session
            .declare_publisher(key.to_string())
            .congestion_control(qos.congestion_control())
            .priority(qos.priority())
            .express(qos.express())
            .reliability(qos.reliability())
            .await?;
        publishers.insert(key.to_string(), publisher);
        Ok(())
    }

    /// Publish `payload` on `key` via its declared publisher.
    pub async fn put(&self, key: &str, payload: Vec<u8>, qos: QosClass) -> Result<()> {
        crate::metric_guard::check_telemetry_key(key);
        self.ensure(key, qos).await?;
        let publishers = self.publishers.read().await;
        publishers
            .get(key)
            .expect("publisher just ensured")
            .put(payload)
            .await?;
        Ok(())
    }

    /// Publish a serialized value on `key` (encode with `format`, then [`Self::put`]).
    pub async fn put_serializable<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
        format: crate::serialization::Format,
        qos: QosClass,
    ) -> Result<()> {
        let payload = crate::serialization::encode(value, format)?;
        self.put(key, payload, qos).await
    }

    /// Delete (tombstone) `key` via its declared publisher.
    pub async fn delete(&self, key: &str, qos: QosClass) -> Result<()> {
        self.ensure(key, qos).await?;
        let publishers = self.publishers.read().await;
        publishers
            .get(key)
            .expect("publisher just ensured")
            .delete()
            .await?;
        Ok(())
    }

    /// Number of currently-declared publishers (test/introspection).
    pub async fn len(&self) -> usize {
        self.publishers.read().await.len()
    }

    /// Whether no publisher has been declared yet.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}
