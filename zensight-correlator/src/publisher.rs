//! Entity publisher.
//!
//! Drains the engine's [`EntityOp`] stream and materializes the entity keyspace:
//! one cached plain [`Publisher`] per `entity_key(id)`, and a tombstone (`delete`)
//! on retire. Late joiners are seeded by the `entities_query_key()` queryable (see
//! `query.rs`) — the sole consumer (the frontend) subscribes plainly and seeds from
//! that queryable — so no AdvancedPublisher cache/recovery is needed here.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zenoh::Session;
use zenoh::pubsub::Publisher;
use zensight_common::serialization::Format;
use zensight_common::{
    AliasRecord, HostEntity, OperatorAssertion, alias_key, assertion_key, encode, entity_key,
};

use crate::engine::EntityOp;

/// Manages the per-entity plain declared publishers.
struct EntityPublisher {
    session: Arc<Session>,
    format: Format,
    publishers: HashMap<String, Publisher<'static>>,
    /// old_id → entity_id alias records already published (skip re-puts on
    /// pure re-emits; LWW makes an occasional duplicate harmless anyway).
    aliases_published: HashMap<String, String>,
}

impl EntityPublisher {
    fn new(session: Arc<Session>, format: Format) -> Self {
        Self {
            session,
            format,
            publishers: HashMap::new(),
            aliases_published: HashMap::new(),
        }
    }

    /// Get or create the cached plain publisher for an entity id.
    async fn publisher_for(&mut self, entity_id: &str) -> anyhow::Result<&Publisher<'static>> {
        if !self.publishers.contains_key(entity_id) {
            let key = entity_key(entity_id);
            // Entities are materialized fleet state — must arrive (reliable+block).
            let q = zensight_common::QosClass::Entity;
            let pubr = self
                .session
                .declare_publisher(key.clone())
                .congestion_control(q.congestion_control())
                .priority(q.priority())
                .express(q.express())
                .reliability(q.reliability())
                .await
                .map_err(|e| anyhow::anyhow!("failed to declare entity publisher {key}: {e}"))?;
            self.publishers.insert(entity_id.to_string(), pubr);
        }
        Ok(self.publishers.get(entity_id).unwrap())
    }

    /// Publish (create/update) an entity on its cached key, plus an
    /// [`AliasRecord`] per retired id (RFC 06 §5.4) so consumers holding an
    /// old entity id can re-point after a merge/upgrade.
    async fn upsert(&mut self, entity: &HostEntity) -> anyhow::Result<()> {
        let payload =
            encode(entity, self.format).map_err(|e| anyhow::anyhow!("encode entity: {e}"))?;
        let encoding = self.format.encoding();
        let pubr = self.publisher_for(&entity.entity_id).await?;
        pubr.put(payload)
            .encoding(encoding)
            .await
            .map_err(|e| anyhow::anyhow!("put entity {}: {e}", entity.entity_id))?;
        for old_id in &entity.aliases {
            if self.aliases_published.get(old_id) == Some(&entity.entity_id) {
                continue;
            }
            let record = AliasRecord {
                old_id: old_id.clone(),
                entity_id: entity.entity_id.clone(),
                last_updated: zensight_common::current_timestamp_millis(),
            };
            let payload = encode(&record, self.format)
                .map_err(|e| anyhow::anyhow!("encode alias record: {e}"))?;
            // One-shot LWW docs on rare merges — a session put via the cached
            // entity-publisher machinery is overkill; reuse a plain declared
            // publisher keyed like the entities.
            let key = alias_key(old_id);
            let q = zensight_common::QosClass::Entity;
            let alias_pub = self
                .session
                .declare_publisher(key.clone())
                .congestion_control(q.congestion_control())
                .priority(q.priority())
                .express(q.express())
                .reliability(q.reliability())
                .await
                .map_err(|e| anyhow::anyhow!("declare alias publisher {key}: {e}"))?;
            alias_pub
                .put(payload)
                .encoding(self.format.encoding())
                .await
                .map_err(|e| anyhow::anyhow!("put alias {old_id}: {e}"))?;
            self.aliases_published
                .insert(old_id.clone(), entity.entity_id.clone());
        }
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

/// Publish one operator assertion on its catalog state key (#473).
///
/// Round-trips through the bus rather than only mutating memory: this is the
/// record that makes a restarted correlator (or a second one, or a
/// storage-backed router) see the operator's decision. A declared publisher per
/// call is fine — an operator invokes this by hand, not in a loop.
pub async fn publish_assertion(
    session: &Session,
    format: Format,
    assertion: &OperatorAssertion,
) -> anyhow::Result<()> {
    let key = assertion_key(&assertion.id);
    let payload =
        encode(assertion, format).map_err(|e| anyhow::anyhow!("encode assertion: {e}"))?;
    let q = zensight_common::QosClass::Entity;
    let pubr = session
        .declare_publisher(key.clone())
        .congestion_control(q.congestion_control())
        .priority(q.priority())
        .express(q.express())
        .reliability(q.reliability())
        .await
        .map_err(|e| anyhow::anyhow!("declare assertion publisher {key}: {e}"))?;
    pubr.put(payload)
        .encoding(format.encoding())
        .await
        .map_err(|e| anyhow::anyhow!("put assertion {key}: {e}"))?;
    info!(key = %key, kind = ?assertion.kind, "published operator assertion");
    Ok(())
}

/// Tombstone an assertion key — how an `unlink` retires the `link` it replaces.
pub async fn retire_assertion(session: &Session, id: &str) -> anyhow::Result<()> {
    let key = assertion_key(id);
    let pubr = session
        .declare_publisher(key.clone())
        .await
        .map_err(|e| anyhow::anyhow!("declare assertion publisher {key}: {e}"))?;
    pubr.delete()
        .await
        .map_err(|e| anyhow::anyhow!("delete assertion {key}: {e}"))?;
    info!(key = %key, "retired operator assertion");
    Ok(())
}

#[cfg(test)]
mod tests {
    use zensight_common::entity_key;

    #[test]
    fn entity_publish_key_mapping() {
        assert_eq!(
            entity_key("h-0123456789ab"),
            "v1/@catalog/state/entity/h-0123456789ab"
        );
    }
}
