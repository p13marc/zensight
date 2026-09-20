//! One contract over the two publish tiers (#1155).
//!
//! A sensor publishes through two backends that grew apart: the baseline
//! [`PublisherRegistry`] (declared publishers, per-call QoS class, the
//! registry guard on every put) and the [`AdvancedPublisherRegistry`]
//! (zenoh-ext advanced publishers with a per-key cache for late joiners, one
//! class per registry). They disagreed on things a caller cannot see: the
//! advanced tier ran the registry guard only when *it* built the key, so a
//! full-key put, a serializable document and a tombstone skipped it; it
//! minted a fresh counter set by default, so one forgotten `with_counters`
//! made `published_total` orders of magnitude low (#1079, and `RelationSet`
//! had exactly that); and it never recorded the class a key was declared
//! with, so the one-key-one-class rule (#1155) was reported on one tier and
//! silent on the other.
//!
//! [`Publish`] is the contract both now keep: **the guard on every put and
//! every delete, deliveries counted after the put succeeds into a counter set
//! the caller supplies, one class per key reported on mismatch, and one
//! observer seam** — so the threshold evaluator is installed through the
//! same call whichever tier a sensor publishes through.
//!
//! [`PublisherRegistry`]: zensight_common::PublisherRegistry
//! [`AdvancedPublisherRegistry`]: crate::AdvancedPublisherRegistry

use std::sync::Arc;

use async_trait::async_trait;
use zensight_common::point_observer::PointObserver;
use zensight_common::{Format, PublishCounters, QosClass, TelemetryPoint};

use crate::error::{Result, SensorError};

/// The publish contract both tiers keep — see the module doc.
///
/// `key` is always base-relative and full (#466): the session namespace adds
/// the base, and the registry guard refuses a key that is not v1 (#1153).
#[async_trait]
pub trait Publish: Send + Sync {
    /// Put already-encoded bytes on `key`, stamping `encoding` (RFC 08 §7),
    /// under `qos` — reported if the key was declared under another class.
    async fn put_encoded(
        &self,
        key: &str,
        payload: Vec<u8>,
        qos: QosClass,
        encoding: zenoh::bytes::Encoding,
    ) -> Result<()>;

    /// Observe → encode → put. **The threshold seam** (#930) on both tiers:
    /// the last place the point is still a `TelemetryPoint` and not bytes.
    async fn put_point(
        &self,
        key: &str,
        point: &TelemetryPoint,
        qos: QosClass,
        format: Format,
    ) -> Result<()>;

    /// Tombstone `key` through the publisher that wrote it. Guarded like a
    /// put.
    async fn delete(&self, key: &str, qos: QosClass) -> Result<()>;

    /// Install the point observer. Idempotent-by-first-call on both tiers.
    fn set_observer(&self, observer: Arc<dyn PointObserver>);

    /// The counter set this backend delivers into.
    fn counters(&self) -> Arc<PublishCounters>;
}

fn publish_err(key: &str, e: impl std::fmt::Display) -> SensorError {
    SensorError::Publish {
        key: key.to_string(),
        message: e.to_string(),
    }
}

#[async_trait]
impl Publish for zensight_common::PublisherRegistry {
    async fn put_encoded(
        &self,
        key: &str,
        payload: Vec<u8>,
        qos: QosClass,
        encoding: zenoh::bytes::Encoding,
    ) -> Result<()> {
        zensight_common::PublisherRegistry::put_encoded(self, key, payload, qos, encoding)
            .await
            .map_err(|e| publish_err(key, e))
    }

    async fn put_point(
        &self,
        key: &str,
        point: &TelemetryPoint,
        qos: QosClass,
        format: Format,
    ) -> Result<()> {
        zensight_common::PublisherRegistry::put_point(self, key, point, qos, format)
            .await
            .map_err(|e| publish_err(key, e))
    }

    async fn delete(&self, key: &str, qos: QosClass) -> Result<()> {
        zensight_common::PublisherRegistry::delete(self, key, qos)
            .await
            .map_err(|e| publish_err(key, e))
    }

    fn set_observer(&self, observer: Arc<dyn PointObserver>) {
        zensight_common::PublisherRegistry::set_observer(self, observer);
    }

    fn counters(&self) -> Arc<PublishCounters> {
        zensight_common::PublisherRegistry::counters(self)
    }
}

#[async_trait]
impl Publish for crate::AdvancedPublisherRegistry {
    async fn put_encoded(
        &self,
        key: &str,
        payload: Vec<u8>,
        qos: QosClass,
        encoding: zenoh::bytes::Encoding,
    ) -> Result<()> {
        self.put_raw_as(key, payload, qos, encoding).await
    }

    async fn put_point(
        &self,
        key: &str,
        point: &TelemetryPoint,
        qos: QosClass,
        format: Format,
    ) -> Result<()> {
        self.observe(key, point);
        let payload = zensight_common::encode(point, format)
            .map_err(|e| SensorError::Serialization(e.to_string()))?;
        self.put_raw_as(key, payload, qos, format.encoding()).await
    }

    async fn delete(&self, key: &str, qos: QosClass) -> Result<()> {
        self.tombstone_as(key, qos).await
    }

    fn set_observer(&self, observer: Arc<dyn PointObserver>) {
        crate::AdvancedPublisherRegistry::set_observer(self, observer);
    }

    fn counters(&self) -> Arc<PublishCounters> {
        crate::AdvancedPublisherRegistry::counters(self)
    }
}

/// The runner's own [`Publisher`](crate::Publisher) is the baseline tier
/// behind a prefix; through the trait it is that tier.
#[async_trait]
impl Publish for crate::Publisher {
    async fn put_encoded(
        &self,
        key: &str,
        payload: Vec<u8>,
        qos: QosClass,
        encoding: zenoh::bytes::Encoding,
    ) -> Result<()> {
        self.publish_raw(key, payload, qos, encoding).await
    }

    async fn put_point(
        &self,
        key: &str,
        point: &TelemetryPoint,
        qos: QosClass,
        format: Format,
    ) -> Result<()> {
        Publish::put_point(self.registry(), key, point, qos, format).await
    }

    async fn delete(&self, key: &str, qos: QosClass) -> Result<()> {
        crate::Publisher::delete(self, key, qos).await
    }

    fn set_observer(&self, observer: Arc<dyn PointObserver>) {
        crate::Publisher::set_observer(self, observer);
    }

    fn counters(&self) -> Arc<PublishCounters> {
        crate::Publisher::counters(self)
    }
}
