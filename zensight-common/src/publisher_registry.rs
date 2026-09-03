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

/// Publish-side self-accounting (#811): totals a sensor's health doc reports
/// so the platform can see its own output. All relaxed atomics — statistics,
/// not synchronization. `published_*` are bumped here on every baseline put;
/// `dropped`/`evicted` have no core-side source and are fed by the sensor
/// that owns the shedding/eviction (count what is wired; the health doc's
/// optional fields already say "absent = not measured").
#[derive(Debug, Default)]
pub struct PublishCounters {
    published_total: std::sync::atomic::AtomicU64,
    published_bytes_total: std::sync::atomic::AtomicU64,
    dropped_total: std::sync::atomic::AtomicU64,
    evicted_total: std::sync::atomic::AtomicU64,
}

impl PublishCounters {
    fn record_publish(&self, bytes: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.published_total.fetch_add(1, Relaxed);
        self.published_bytes_total.fetch_add(bytes as u64, Relaxed);
    }
    /// Count samples the sensor chose not to publish (shed, rate-limited).
    pub fn add_dropped(&self, n: u64) {
        self.dropped_total
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
    /// Count entries evicted from bounded tables.
    pub fn add_evicted(&self, n: u64) {
        self.evicted_total
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn published_total(&self) -> u64 {
        self.published_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn published_bytes_total(&self) -> u64 {
        self.published_bytes_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn evicted_total(&self) -> u64 {
        self.evicted_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Caches one declared [`Publisher`] per key expression.
pub struct PublisherRegistry {
    session: Arc<Session>,
    publishers: RwLock<HashMap<String, Publisher<'static>>>,
    counters: Arc<PublishCounters>,
    /// Watches every point published through [`Self::put_point`] (#930).
    ///
    /// A `OnceLock` rather than a lock: it is installed once at startup and
    /// read on every publish, so the read must cost nothing. A registry with
    /// no observer pays one `Option` check per point.
    observer: std::sync::OnceLock<Arc<dyn crate::point_observer::PointObserver>>,
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
            counters: Arc::new(PublishCounters::default()),
            observer: std::sync::OnceLock::new(),
        }
    }

    /// Install the point observer (#930). Idempotent-by-first-call: a second
    /// install is ignored rather than replacing a live evaluator mid-flight.
    pub fn set_observer(&self, observer: Arc<dyn crate::point_observer::PointObserver>) {
        let _ = self.observer.set(observer);
    }

    /// The registry's publish counters (#811) — shared so the health doc can
    /// read them and the owning sensor can feed `dropped`/`evicted` into the
    /// same accounting.
    pub fn counters(&self) -> Arc<PublishCounters> {
        self.counters.clone()
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
        self.counters.record_publish(payload.len());
        let publishers = self.publishers.read().await;
        publishers
            .get(key)
            .expect("publisher just ensured")
            .put(payload)
            .await?;
        Ok(())
    }

    /// Publish `payload` on `key`, stamping the sample [`Encoding`] so
    /// consumers resolve the payload from metadata rather than sniffing
    /// (RFC 08 §7). Use [`crate::serialization::Format::encoding`] for
    /// format-encoded payloads.
    pub async fn put_encoded(
        &self,
        key: &str,
        payload: Vec<u8>,
        qos: QosClass,
        encoding: zenoh::bytes::Encoding,
    ) -> Result<()> {
        crate::metric_guard::check_telemetry_key(key);
        self.ensure(key, qos).await?;
        self.counters.record_publish(payload.len());
        let publishers = self.publishers.read().await;
        publishers
            .get(key)
            .expect("publisher just ensured")
            .put(payload)
            .encoding(encoding)
            .await?;
        Ok(())
    }

    /// Publish one telemetry point: observe, encode, put (#930).
    ///
    /// **The seam.** Every path that wants a threshold rule evaluated against
    /// its points comes through here, because this is the last place the point
    /// is still a `TelemetryPoint` rather than bytes. Callers that encode
    /// themselves and call [`Self::put`] bypass it — deliberately visible, so
    /// "does this sensor evaluate thresholds?" is answerable by grep.
    pub async fn put_point(
        &self,
        key: &str,
        point: &crate::TelemetryPoint,
        qos: QosClass,
        format: crate::serialization::Format,
    ) -> Result<()> {
        if let Some(observer) = self.observer.get() {
            observer.observe_point(key, point);
        }
        let payload = crate::serialization::encode(point, format)?;
        self.put_encoded(key, payload, qos, format.encoding()).await
    }

    /// Publish a serialized value on `key` (encode with `format`, stamp its
    /// [`Encoding`], then put).
    pub async fn put_serializable<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
        format: crate::serialization::Format,
        qos: QosClass,
    ) -> Result<()> {
        let payload = crate::serialization::encode(value, format)?;
        self.put_encoded(key, payload, qos, format.encoding()).await
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
