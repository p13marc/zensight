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
    /// Count one delivered publication. Called by every publish path after
    /// the put succeeded — the advanced tier included (#1079) — so
    /// `published_total` is deliveries, not attempts.
    pub fn record_publish(&self, bytes: usize) {
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
    /// `key -> (publisher, the class it was DECLARED with)`.
    ///
    /// The class is kept so a second `ensure` under a different class can be
    /// reported (#1155) instead of silently riding the first one's QoS.
    publishers: RwLock<HashMap<String, (Publisher<'static>, QosClass)>>,
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
    ///
    /// # A key may only have one QoS class (#1155)
    ///
    /// A Zenoh publisher carries its congestion control, priority, reliability
    /// and express flag from the moment it is declared; they cannot be changed
    /// afterwards. So the **first** class a key is published under is the one
    /// every later publication on that key gets, whatever class it asked for.
    ///
    /// This used to return early on `contains_key` alone, which made that
    /// substitution silent and let it happen in the dangerous direction: a key
    /// first published as `Telemetry` (BestEffort, **Drop**) and later as
    /// `Alert` keeps BestEffort/Drop — so the one class that exists to be
    /// undroppable becomes droppable, and nothing says so.
    ///
    /// The publisher is still reused, because tearing one down and
    /// re-declaring it mid-flight would lose whatever is in flight and would
    /// not fix the publications already sent. What changes is that the
    /// mismatch is **reported**: a `warn!` naming both classes, in release as
    /// well as debug, plus a `debug_assert!` so a test hits it hard.
    async fn ensure(&self, key: &str, qos: QosClass) -> Result<()> {
        {
            if let Some((_, declared)) = self.publishers.read().await.get(key) {
                Self::check_class(key, *declared, qos);
                return Ok(());
            }
        }
        let mut publishers = self.publishers.write().await;
        if let Some((_, declared)) = publishers.get(key) {
            Self::check_class(key, *declared, qos);
            return Ok(());
        }
        // Owned String → `KeyExpr<'static>` so the cached publisher is `'static`.
        let publisher = crate::qos::declare_publisher(&self.session, key.to_string(), qos).await?;
        publishers.insert(key.to_string(), (publisher, qos));
        Ok(())
    }

    /// Report a key published under a second QoS class (#1155).
    ///
    /// Separated so the test can reason about the rule without a session.
    fn check_class(key: &str, declared: QosClass, asked: QosClass) {
        if declared == asked {
            return;
        }
        tracing::warn!(
            key = %key,
            declared = ?declared,
            asked = ?asked,
            "key already has a declared publisher under a different QoS class; \
             the publication rides the DECLARED class — a Zenoh publisher's QoS \
             is fixed at declare time and cannot be changed (#1155)"
        );
        debug_assert!(
            false,
            "{key} declared as {declared:?} and published as {asked:?}: one key, one class"
        );
    }

    /// Publish `payload` on `key` via its declared publisher.
    pub async fn put(&self, key: &str, payload: Vec<u8>, qos: QosClass) -> Result<()> {
        crate::metric_guard::check_telemetry_key(key);
        self.ensure(key, qos).await?;
        let bytes = payload.len();
        let publishers = self.publishers.read().await;
        publishers
            .get(key)
            .expect("publisher just ensured")
            .0
            .put(payload)
            .await?;
        self.counters.record_publish(bytes);
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
        let bytes = payload.len();
        let publishers = self.publishers.read().await;
        publishers
            .get(key)
            .expect("publisher just ensured")
            .0
            .put(payload)
            .encoding(encoding)
            .await?;
        self.counters.record_publish(bytes);
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
            .0
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #1155: a Zenoh publisher's QoS is fixed at declare time, so the first
    /// class a key is published under is the one every later publication gets.
    /// Asking for a second class is a bug in the caller, and it used to be
    /// silent.
    ///
    /// The dangerous direction is this one: a key first seen as `Telemetry`
    /// (BestEffort, **Drop**) and later published as `Alert` keeps BestEffort
    /// and Drop — so the one class that exists to be undroppable becomes
    /// droppable.
    #[test]
    #[should_panic(expected = "one key, one class")]
    fn a_second_qos_class_on_one_key_is_caught() {
        PublisherRegistry::check_class(
            "v1/h-000000000000/state/snmp/alert/x",
            QosClass::Telemetry,
            QosClass::Alert,
        );
    }

    /// The ordinary case: the same key, the same class, every time.
    #[test]
    fn the_same_class_twice_is_fine() {
        PublisherRegistry::check_class(
            "v1/h-000000000000/telemetry/snmp/cpu",
            QosClass::Telemetry,
            QosClass::Telemetry,
        );
    }
}
