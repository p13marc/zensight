//! Entity publisher.
//!
//! Drains the engine's [`EntityOp`] stream and materializes the entity keyspace:
//! one cached [`AdvancedPublisher`] per `entity_key(id)` (cache = 1, so a late
//! joiner seeds the latest doc immediately, mirroring how sensors seed alerts),
//! and a tombstone (`delete`) on retire.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zenoh::Session;
use zenoh_ext::{AdvancedPublisher, AdvancedPublisherBuilderExt, CacheConfig, MissDetectionConfig};
use zensight_common::serialization::Format;
use zensight_common::{HostEntity, encode, entity_key};

use crate::engine::EntityOp;

/// Manages the per-entity cached publishers.
struct EntityPublisher {
    session: Arc<Session>,
    format: Format,
    publishers: HashMap<String, AdvancedPublisher<'static>>,
}

impl EntityPublisher {
    fn new(session: Arc<Session>, format: Format) -> Self {
        Self {
            session,
            format,
            publishers: HashMap::new(),
        }
    }

    /// Get or create the cached publisher for an entity id.
    async fn publisher_for(
        &mut self,
        entity_id: &str,
    ) -> anyhow::Result<&AdvancedPublisher<'static>> {
        if !self.publishers.contains_key(entity_id) {
            let key = entity_key(entity_id);
            let pubr = self
                .session
                .declare_publisher(key.clone())
                .cache(CacheConfig::default().max_samples(1))
                // Sequence-number miss-detection (like the sensors) — without it
                // the cache defaults to `Sequencing::Timestamp`, which fails
                // unless the Zenoh session has timestamping enabled.
                .sample_miss_detection(MissDetectionConfig::default())
                .publisher_detection()
                .await
                .map_err(|e| anyhow::anyhow!("failed to declare entity publisher {key}: {e}"))?;
            self.publishers.insert(entity_id.to_string(), pubr);
        }
        Ok(self.publishers.get(entity_id).unwrap())
    }

    /// Publish (create/update) an entity on its cached key.
    async fn upsert(&mut self, entity: &HostEntity) -> anyhow::Result<()> {
        let payload =
            encode(entity, self.format).map_err(|e| anyhow::anyhow!("encode entity: {e}"))?;
        let pubr = self.publisher_for(&entity.entity_id).await?;
        pubr.put(payload)
            .await
            .map_err(|e| anyhow::anyhow!("put entity {}: {e}", entity.entity_id))?;
        Ok(())
    }

    /// Tombstone an entity id (delete on its key); undeclare the publisher.
    async fn tombstone(&mut self, entity_id: &str) -> anyhow::Result<()> {
        let pubr = self.publisher_for(entity_id).await?;
        pubr.delete()
            .await
            .map_err(|e| anyhow::anyhow!("delete entity {entity_id}: {e}"))?;
        self.publishers.remove(entity_id); // drop → undeclare
        Ok(())
    }
}

/// Run the publisher: drain `op_rx`, apply each op, until shutdown.
pub async fn run(
    session: Arc<Session>,
    format: Format,
    mut op_rx: mpsc::Receiver<EntityOp>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut publisher = EntityPublisher::new(session, format);
    info!("entity publisher ready");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            op = op_rx.recv() => {
                match op {
                    Some(EntityOp::Upsert(entity)) => {
                        if let Err(e) = publisher.upsert(&entity).await {
                            warn!(error = %e, "entity upsert failed");
                        } else {
                            debug!(entity_id = %entity.entity_id, "entity published");
                        }
                    }
                    Some(EntityOp::Tombstone(id)) => {
                        if let Err(e) = publisher.tombstone(&id).await {
                            warn!(error = %e, "entity tombstone failed");
                        } else {
                            debug!(entity_id = %id, "entity tombstoned");
                        }
                    }
                    None => break, // engine gone
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zensight_common::entity_key;

    #[test]
    fn entity_publish_key_mapping() {
        assert_eq!(
            entity_key("h_0123456789ab"),
            "zensight/_meta/entity/host/h_0123456789ab"
        );
    }
}
