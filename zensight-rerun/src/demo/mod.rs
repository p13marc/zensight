//! Synthetic demo publishers (the `zensight-rerun-demo` bin).
//!
//! Everything goes through the real bus contract: declared publishers via
//! [`PublisherRegistry`] with the proper [`QosClass`], CBOR encoding, keys
//! from [`KeyExprBuilder`]/keyexpr helpers — never a hand-formatted key,
//! never `session.put`. Demo sessions are ALWAYS isolated (scouting off,
//! explicit connect endpoint): a live sensor fleet must not contaminate a
//! demo recording, and vice versa (docs/plans/rerun/08-multi-process.md §5).

pub mod events;
pub mod incident;
pub mod metrics;

use std::sync::Arc;

use zensight_common::PublisherRegistry;
use zensight_common::alert::Alert;
use zensight_common::config::ZenohConfig;
use zensight_common::entity::HostEntity;
use zensight_common::health::HealthSnapshot;
use zensight_common::keyexpr::{KeyExprBuilder, entity_key, sensor_control_prefix};
use zensight_common::qos::QosClass;
use zensight_common::serialization::Format;
use zensight_common::telemetry::TelemetryPoint;

/// A demo publishing context over one isolated session.
pub struct DemoContext {
    pub session: Arc<zenoh::Session>,
    pub registry: PublisherRegistry,
}

impl DemoContext {
    /// Open an isolated session connected to `endpoint` (e.g. the adapter's
    /// `tcp/127.0.0.1:7449` listener).
    pub async fn connect(endpoint: &str) -> anyhow::Result<Self> {
        let config = ZenohConfig {
            mode: "peer".to_string(),
            connect: vec![endpoint.to_string()],
            listen: vec![],
        };
        let session = Arc::new(crate::session::open_session(&config, true).await?);
        let registry = PublisherRegistry::new(session.clone());
        Ok(Self { session, registry })
    }

    /// Build a context over an existing session (integration tests share one
    /// lone session between publisher and adapter).
    pub fn over(session: Arc<zenoh::Session>) -> Self {
        let registry = PublisherRegistry::new(session.clone());
        Self { session, registry }
    }

    /// Publish one telemetry point on its canonical key.
    pub async fn publish_point(&self, point: &TelemetryPoint) -> anyhow::Result<()> {
        let key = KeyExprBuilder::new(point.protocol).build(&point.source, &point.metric);
        self.registry
            .put_serializable(&key, point, Format::Cbor, QosClass::Telemetry)
            .await?;
        Ok(())
    }

    /// Publish one alert transition on its keyed `@/alerts/<alert_key>` channel.
    pub async fn publish_alert(&self, alert: &Alert) -> anyhow::Result<()> {
        let key = KeyExprBuilder::new(alert.protocol).alert_key_expr(&alert.alert_key());
        self.registry
            .put_serializable(&key, alert, Format::Cbor, QosClass::Alert)
            .await?;
        Ok(())
    }

    /// Publish one health snapshot on the sensor's host-scoped `@/health`
    /// channel (`sensor_control_prefix` + the canonical `@/health` suffix,
    /// the same shape sensor-core publishes on — docs/KEYSPACE.md).
    pub async fn publish_health(&self, snapshot: &HealthSnapshot) -> anyhow::Result<()> {
        let source = snapshot.source.as_deref().unwrap_or("unknown");
        let key = format!(
            "{}/@/health",
            sensor_control_prefix(&snapshot.sensor, source)
        );
        self.registry
            .put_serializable(&key, snapshot, Format::Cbor, QosClass::HealthLiveness)
            .await?;
        Ok(())
    }

    /// Publish one host-entity doc (what a correlator would emit).
    pub async fn publish_entity(&self, entity: &HostEntity) -> anyhow::Result<()> {
        self.registry
            .put_serializable(
                &entity_key(&entity.entity_id),
                entity,
                Format::Cbor,
                QosClass::Entity,
            )
            .await?;
        Ok(())
    }
}
