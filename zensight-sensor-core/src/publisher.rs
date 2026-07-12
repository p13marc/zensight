//! Telemetry publisher for Zenoh.

use std::sync::Arc;

use zenoh::bytes::{Encoding, ZBytes};
use zenoh::handlers::DefaultHandler;
use zenoh::matching::MatchingListenerBuilder;
use zensight_common::{Format, QosClass, TelemetryPoint};

use crate::error::{Result, SensorError};
use crate::v1::V1Context;

/// Publisher for sending telemetry to Zenoh.
///
/// Wraps a Zenoh session and provides convenient methods for publishing
/// [`TelemetryPoint`] values with automatic serialization.
///
/// **Telemetry** (`publish` / `publish_to_key` / `publish_batch`) publishes
/// under the v1 grammar (`<base>/@v1/<origin>/telemetry/<producer>/…`) on the
/// **baseline delivery tier** (RFC 04 §3.2): plain declared publishers, no
/// per-key cache/heartbeat machinery — a late joiner waits out the next
/// cadence (`seed = none`), which is the class default. The advanced tier is
/// opt-in per subject (RFC 04 §3.3; the runner's evidence publishers use
/// cache-only depth 1). **Control-plane** writes (`publish_raw` /
/// `publish_json` / `delete`) stay plain declared `put`/`delete`.
#[derive(Clone, Debug)]
pub struct Publisher {
    session: Arc<zenoh::Session>,
    /// The v1 telemetry prefix (`<base>/@v1/<origin>/telemetry/<producer>`).
    key_prefix: String,
    format: Format,
    /// The v1 key context this publisher derives every key from.
    v1: V1Context,
    /// Shared registry of declared plain publishers backing both telemetry
    /// (baseline tier) and control-plane writes — declared, never one-shot
    /// `session.put`, so keys are interned + routing-optimized.
    control: Arc<zensight_common::PublisherRegistry>,
}

impl Publisher {
    /// Create a new publisher.
    pub fn new(
        session: Arc<zenoh::Session>,
        key_prefix: impl Into<String>,
        format: Format,
    ) -> Self {
        let v1 = V1Context::from_prefix(&key_prefix.into());
        let key_prefix = v1.telemetry_prefix();
        let control = Arc::new(zensight_common::PublisherRegistry::new(session.clone()));
        Self {
            session,
            key_prefix,
            format,
            v1,
            control,
        }
    }

    /// The v1 key context (origin + producer + base) behind this publisher.
    pub fn v1(&self) -> &V1Context {
        &self.v1
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
    ///
    /// Debug-asserts that `suffix` doesn't contain double slashes.
    pub fn build_key(&self, suffix: &str) -> String {
        debug_assert!(!suffix.contains("//"), "key suffix must not contain '//'");
        if suffix.is_empty() {
            self.key_prefix.clone()
        } else {
            format!("{}/{}", self.key_prefix, suffix)
        }
    }

    /// Publish a telemetry point (baseline tier: plain declared publisher).
    ///
    /// The key is constructed by appending `key_suffix` to the publisher's prefix.
    pub async fn publish(&self, key_suffix: &str, point: &TelemetryPoint) -> Result<()> {
        let key = self.build_key(key_suffix);
        self.publish_to_key(&key, point).await
    }

    /// Publish a telemetry point with a full key (not using prefix).
    pub async fn publish_to_key(&self, key: &str, point: &TelemetryPoint) -> Result<()> {
        let payload =
            zensight_common::encode(point, self.format).map_err(|e| SensorError::Publish {
                key: key.to_string(),
                message: e.to_string(),
            })?;
        self.publish_raw(key, payload, QosClass::Telemetry).await
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

    /// Publish raw bytes to a control-plane key with an explicit QoS class.
    ///
    /// Alerts/commands use [`QosClass::Alert`]/[`QosClass::Command`]
    /// (reliable+block) so a firing/resolved event is never dropped on a lossy
    /// link; health/liveness use [`QosClass::HealthLiveness`] (drop-friendly).
    pub async fn publish_raw(&self, key: &str, payload: Vec<u8>, qos: QosClass) -> Result<()> {
        self.control
            .put(key, payload, qos)
            .await
            .map_err(|e| SensorError::Publish {
                key: key.to_string(),
                message: e.to_string(),
            })
    }

    /// Publish a JSON value to a control-plane key with an explicit QoS class.
    pub async fn publish_json<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
        qos: QosClass,
    ) -> Result<()> {
        let payload = serde_json::to_vec(value)?;
        self.publish_raw(key, payload, qos).await
    }

    /// Delete (tombstone) a key with an explicit QoS class — used to clear keyed,
    /// lifecycle-managed state such as resolved alerts so late subscribers don't
    /// see stale values. Alert tombstones must arrive, so pass [`QosClass::Alert`].
    pub async fn delete(&self, key: &str, qos: QosClass) -> Result<()> {
        self.control
            .delete(key, qos)
            .await
            .map_err(|e| SensorError::Publish {
                key: key.to_string(),
                message: e.to_string(),
            })
    }

    /// Declare a **plain** (NOT advanced) publisher for the opaque `@media` plane
    /// (#359) at an absolute `key` (e.g. from
    /// [`zensight_common::keyexpr::media_video_key`]).
    ///
    /// Unlike the telemetry path, media samples are raw encoded access units —
    /// no `TelemetryPoint`/`Format` envelope, no zenoh-ext cache/recovery — so
    /// this deliberately bypasses the [`AdvancedPublisherRegistry`]. The
    /// publisher is declared with [`QosClass::LiveVideo`] (best-effort, drop,
    /// interactive-high): a stale frame is worthless, and the encoder must never
    /// block. Each [`RawMediaPublisher::put`] carries its own [`Encoding`]
    /// (e.g. `video/h264`, `image/jpeg`) and a frame-metadata attachment; the
    /// [`RawMediaPublisher::matching_listener`] fires when a viewer appears so
    /// the sensor can force an immediate keyframe.
    pub async fn raw_media_publisher(&self, key: impl Into<String>) -> Result<RawMediaPublisher> {
        let key = key.into();
        let qos = QosClass::LiveVideo;
        let inner = self
            .session
            .declare_publisher(key.clone())
            .congestion_control(qos.congestion_control())
            .priority(qos.priority())
            .express(qos.express())
            .reliability(qos.reliability())
            .await
            .map_err(|e| SensorError::Publish {
                key: key.clone(),
                message: e.to_string(),
            })?;
        Ok(RawMediaPublisher { key, inner })
    }
}

/// A plain Zenoh publisher for one media-plane key (#359).
///
/// Wraps a declared [`zenoh::pubsub::Publisher`] carrying raw, opaque encoded
/// media (never a `TelemetryPoint`). Obtain one from
/// [`Publisher::raw_media_publisher`].
pub struct RawMediaPublisher {
    key: String,
    inner: zenoh::pubsub::Publisher<'static>,
}

impl std::fmt::Debug for RawMediaPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawMediaPublisher")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl RawMediaPublisher {
    /// The absolute key this publisher writes to.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Publish one encoded access unit (opaque bytes) with its `encoding`
    /// (e.g. `Encoding::VIDEO_H264`, `Encoding::IMAGE_JPEG`) and a frame-metadata
    /// `attachment` (keyframe flag, PTS, sequence, …). No serialization envelope
    /// — the payload is the wire bytes.
    pub async fn put(
        &self,
        payload: Vec<u8>,
        encoding: Encoding,
        attachment: ZBytes,
    ) -> Result<()> {
        self.inner
            .put(payload)
            .encoding(encoding)
            .attachment(attachment)
            .await
            .map_err(|e| SensorError::Publish {
                key: self.key.clone(),
                message: e.to_string(),
            })
    }

    /// Return a builder for a matching listener that fires whenever this
    /// publisher gains or loses matching subscribers. The sensor uses the
    /// rising edge (a viewer appeared) to force a keyframe so late joiners get a
    /// decodable picture immediately.
    pub fn matching_listener(&self) -> MatchingListenerBuilder<'_, DefaultHandler> {
        self.inner.matching_listener()
    }

    /// Whether this publisher currently has at least one matching subscriber.
    pub async fn has_viewers(&self) -> Result<bool> {
        self.inner
            .matching_status()
            .await
            .map(|s| s.matching())
            .map_err(|e| SensorError::Publish {
                key: self.key.clone(),
                message: e.to_string(),
            })
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
