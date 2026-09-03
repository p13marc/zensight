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

/// Manages the per-edge plain declared publishers (#917).
///
/// Deliberately the same shape as [`EntityPublisher`] — a cached publisher per
/// key, `delete()` as the tombstone, `QosClass::Entity` — because an edge has
/// the same lifecycle as an entity: materialized fleet state that must arrive,
/// seeded for late joiners by a queryable rather than a publisher cache.
struct EdgePublisher {
    session: Arc<Session>,
    format: Format,
    publishers: HashMap<String, Publisher<'static>>,
}

impl EdgePublisher {
    fn new(session: Arc<Session>, format: Format) -> Self {
        Self {
            session,
            format,
            publishers: HashMap::new(),
        }
    }

    async fn publisher_for(&mut self, edge_id: &str) -> anyhow::Result<&Publisher<'static>> {
        if !self.publishers.contains_key(edge_id) {
            let key = zensight_common::keyexpr::edge_key(edge_id);
            let q = zensight_common::QosClass::Entity;
            let pubr = self
                .session
                .declare_publisher(key.clone())
                .congestion_control(q.congestion_control())
                .priority(q.priority())
                .express(q.express())
                .reliability(q.reliability())
                .await
                .map_err(|e| anyhow::anyhow!("failed to declare edge publisher {key}: {e}"))?;
            self.publishers.insert(edge_id.to_string(), pubr);
        }
        Ok(self.publishers.get(edge_id).unwrap())
    }

    async fn upsert(&mut self, edge: &zensight_common::relation::Edge) -> anyhow::Result<()> {
        let payload = encode(edge, self.format).map_err(|e| anyhow::anyhow!("encode edge: {e}"))?;
        let encoding = self.format.encoding();
        let pubr = self.publisher_for(&edge.edge_id).await?;
        pubr.put(payload)
            .encoding(encoding)
            .await
            .map_err(|e| anyhow::anyhow!("put edge {}: {e}", edge.edge_id))?;
        Ok(())
    }

    async fn tombstone(&mut self, edge_id: &str) -> anyhow::Result<()> {
        let pubr = self.publisher_for(edge_id).await?;
        pubr.delete()
            .await
            .map_err(|e| anyhow::anyhow!("delete edge {edge_id}: {e}"))?;
        self.publishers.remove(edge_id); // drop -> undeclare
        Ok(())
    }
}

/// Manages the per-incident declared publishers (#923).
///
/// The same shape as [`EdgePublisher`], and for the same reason: an incident
/// is materialized fleet state with an entity's lifecycle — it must arrive, it
/// is seeded for late joiners by a queryable rather than a publisher cache,
/// and it is tombstoned rather than left to age out.
struct IncidentPublisher {
    session: Arc<Session>,
    format: Format,
    publishers: HashMap<String, Publisher<'static>>,
}

impl IncidentPublisher {
    fn new(session: Arc<Session>, format: Format) -> Self {
        Self {
            session,
            format,
            publishers: HashMap::new(),
        }
    }

    async fn publisher_for(&mut self, incident_id: &str) -> anyhow::Result<&Publisher<'static>> {
        if !self.publishers.contains_key(incident_id) {
            let key = zensight_common::keyexpr::incident_key(incident_id);
            let q = zensight_common::QosClass::Entity;
            let pubr = self
                .session
                .declare_publisher(key.clone())
                .congestion_control(q.congestion_control())
                .priority(q.priority())
                .express(q.express())
                .reliability(q.reliability())
                .await
                .map_err(|e| anyhow::anyhow!("failed to declare incident publisher {key}: {e}"))?;
            self.publishers.insert(incident_id.to_string(), pubr);
        }
        Ok(self.publishers.get(incident_id).unwrap())
    }

    async fn upsert(
        &mut self,
        incident: &zensight_common::incident::Incident,
    ) -> anyhow::Result<()> {
        let payload =
            encode(incident, self.format).map_err(|e| anyhow::anyhow!("encode incident: {e}"))?;
        let encoding = self.format.encoding();
        let pubr = self.publisher_for(&incident.id).await?;
        pubr.put(payload)
            .encoding(encoding)
            .await
            .map_err(|e| anyhow::anyhow!("put incident {}: {e}", incident.id))?;
        Ok(())
    }

    async fn tombstone(&mut self, incident_id: &str) -> anyhow::Result<()> {
        let pubr = self.publisher_for(incident_id).await?;
        pubr.delete()
            .await
            .map_err(|e| anyhow::anyhow!("tombstone incident {incident_id}: {e}"))?;
        Ok(())
    }
}

/// Run the incident publisher: drain `op_rx`, apply each op, until shutdown.
pub async fn run_incidents(
    session: Arc<Session>,
    format: Format,
    mut op_rx: mpsc::Receiver<crate::incidents::IncidentOp>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut publisher = IncidentPublisher::new(session, format);
    info!("incident publisher ready");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            op = op_rx.recv() => {
                match op {
                    Some(crate::incidents::IncidentOp::Upsert(incident)) => {
                        if let Err(e) = publisher.upsert(&incident).await {
                            warn!(error = %e, "incident upsert failed");
                        }
                    }
                    Some(crate::incidents::IncidentOp::Tombstone(id)) => {
                        if let Err(e) = publisher.tombstone(&id).await {
                            warn!(error = %e, "incident tombstone failed");
                        }
                    }
                    None => break,
                }
            }
        }
    }
    debug!("incident publisher stopped");
    Ok(())
}

/// Run the edge publisher: drain `op_rx`, apply each op, until shutdown.
pub async fn run_edges(
    session: Arc<Session>,
    format: Format,
    mut op_rx: mpsc::Receiver<crate::edges::EdgeOp>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut publisher = EdgePublisher::new(session, format);
    info!("edge publisher ready");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            op = op_rx.recv() => {
                match op {
                    Some(crate::edges::EdgeOp::Upsert(edge)) => {
                        if let Err(e) = publisher.upsert(&edge).await {
                            warn!(error = %e, "edge upsert failed");
                        }
                    }
                    Some(crate::edges::EdgeOp::Tombstone(id)) => {
                        if let Err(e) = publisher.tombstone(&id).await {
                            warn!(error = %e, "edge tombstone failed");
                        }
                    }
                    None => break,
                }
            }
        }
    }
    debug!("edge publisher stopped");
    Ok(())
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
/// Publish an acknowledgement (#924).
///
/// A one-shot declared publisher rather than a cached one, exactly as
/// `publish_assertion` is: acks are written by an operator pressing a button,
/// not on a loop, and a cache keyed by alert ref would grow with the fleet's
/// history for no gain.
pub async fn publish_ack(
    session: &Session,
    format: Format,
    ack: &zensight_common::ack::AlertAck,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::ack_key(&ack.alert_ref);
    put_catalog_doc(session, format, &key, ack).await?;
    info!(key = %key, by = %ack.by, "published acknowledgement");
    Ok(())
}

/// Tombstone an acknowledgement.
pub async fn retire_ack(
    session: &Session,
    alert_ref: &zensight_common::alert::AlertRef,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::ack_key(alert_ref);
    delete_catalog_doc(session, &key).await?;
    info!(key = %key, "retired acknowledgement");
    Ok(())
}

/// Publish a suppression window (#924).
pub async fn publish_silence(
    session: &Session,
    format: Format,
    silence: &zensight_common::silence::Silence,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::silence_key(&silence.id);
    put_catalog_doc(session, format, &key, silence).await?;
    info!(key = %key, by = %silence.by, "published silence");
    Ok(())
}

/// Tombstone a suppression window.
pub async fn retire_silence(session: &Session, id: &str) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::silence_key(id);
    delete_catalog_doc(session, &key).await?;
    info!(key = %key, "retired silence");
    Ok(())
}

async fn put_catalog_doc<T: serde::Serialize>(
    session: &Session,
    format: Format,
    key: &str,
    doc: &T,
) -> anyhow::Result<()> {
    let payload = encode(doc, format).map_err(|e| anyhow::anyhow!("encode {key}: {e}"))?;
    let q = zensight_common::QosClass::Entity;
    let pubr = session
        .declare_publisher(key.to_string())
        .congestion_control(q.congestion_control())
        .priority(q.priority())
        .express(q.express())
        .reliability(q.reliability())
        .await
        .map_err(|e| anyhow::anyhow!("declare publisher {key}: {e}"))?;
    pubr.put(payload)
        .encoding(format.encoding())
        .await
        .map_err(|e| anyhow::anyhow!("put {key}: {e}"))?;
    Ok(())
}

async fn delete_catalog_doc(session: &Session, key: &str) -> anyhow::Result<()> {
    let pubr = session
        .declare_publisher(key.to_string())
        .await
        .map_err(|e| anyhow::anyhow!("declare publisher {key}: {e}"))?;
    pubr.delete()
        .await
        .map_err(|e| anyhow::anyhow!("delete {key}: {e}"))?;
    Ok(())
}

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
