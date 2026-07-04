//! Recompute engine.
//!
//! [`CorrelatorState`] is the testable core: it owns the evidence + name stores
//! and the last-published entity set, applies incoming evidence, and on
//! `recompute` runs the pure merge, injects names/status, diffs against the last
//! published set and returns the [`EntityOp`]s (upserts + tombstones) to publish.
//! [`Engine`] is the async driver: it debounces recomputes, drives the periodic
//! re-emit, and forwards ops to the publisher's channel.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep_until};
use tracing::{debug, info};
use zensight_common::{
    HostEntity, HostEvidence, NameObservation, NameVal, current_timestamp_millis,
};

use crate::config::CorrelatorConfig;
use crate::merge;
use crate::store::{EvidenceStore, MAX_NAMES_PER_IP, NameStore};

/// Cap on the number of names attached inline to one entity.
const ENTITY_NAMES_CAP: usize = 32;

/// One decoded input to the engine, produced by the subscribers.
#[derive(Debug, Clone)]
pub enum EvidenceMsg {
    /// A host-identity claim (`_meta/evidence/host/<sensor>/<source>`).
    Host(HostEvidence),
    /// A passive-DNS name observation (`_meta/evidence/names/<sensor>/<ip>`).
    Name(NameObservation),
    /// A device-liveness update; `source` is the device id, `status` the
    /// reported status string.
    Liveness { source: String, status: String },
}

/// A change to publish on the entity keyspace.
///
/// `Upsert` boxes its entity: a `HostEntity` is much larger than a tombstone's
/// `String`, and this op flows through a channel (avoids a fat enum).
#[derive(Debug, Clone, PartialEq)]
pub enum EntityOp {
    /// Publish (create or update) this entity.
    Upsert(Box<HostEntity>),
    /// Tombstone (delete) the entity id.
    Tombstone(String),
}

/// A previously-published entity plus its content hash (last_updated excluded),
/// used to detect real changes vs. pure liveness re-emits.
struct EntityRecord {
    hash: u64,
    entity: HostEntity,
}

/// The testable correlation core: stores + last-published set + diff logic.
pub struct CorrelatorState {
    config: CorrelatorConfig,
    evidence: EvidenceStore,
    names: NameStore,
    /// Latest device-liveness status per device (== evidence `source`).
    liveness: HashMap<String, String>,
    /// Entity id → last published record.
    last: HashMap<String, EntityRecord>,
}

impl CorrelatorState {
    /// Create an empty state.
    pub fn new(config: CorrelatorConfig) -> Self {
        Self {
            config,
            evidence: EvidenceStore::default(),
            names: NameStore::default(),
            liveness: HashMap::new(),
            last: HashMap::new(),
        }
    }

    /// Apply one incoming message to the stores.
    pub fn apply(&mut self, msg: EvidenceMsg) {
        match msg {
            EvidenceMsg::Host(ev) => self.evidence.upsert(ev),
            EvidenceMsg::Name(obs) => self.names.upsert(obs),
            EvidenceMsg::Liveness { source, status } => {
                self.liveness.insert(source, status);
            }
        }
    }

    /// Recompute the entity set at `now_ms` and return the ops to publish.
    ///
    /// Sweeps the stores, runs the pure merge over TTL-live evidence, injects
    /// names + status, handles entity-id upgrades (old id → `aliases` +
    /// tombstone), then diffs against the last published set.
    pub fn recompute(&mut self, now_ms: i64) -> Vec<EntityOp> {
        let ttl_ms = self.config.evidence_ttl_secs as i64 * 1000;
        self.evidence.sweep(now_ms, ttl_ms);
        self.names.sweep(now_ms, ttl_ms);

        let live = self.evidence.live(now_ms, ttl_ms);
        let mut entities = merge::correlate(&live, &self.config.rules);

        for e in &mut entities {
            self.inject_names(e);
            self.assign_status(e);
            e.last_updated = now_ms;
        }

        self.apply_upgrades(&mut entities);

        // Diff against last published.
        let new_ids: std::collections::HashSet<String> =
            entities.iter().map(|e| e.entity_id.clone()).collect();
        let mut ops = Vec::new();
        let mut next_last: HashMap<String, EntityRecord> = HashMap::new();

        for e in entities {
            let hash = content_hash(&e);
            let changed = self.last.get(&e.entity_id).map(|r| r.hash) != Some(hash);
            if changed {
                ops.push(EntityOp::Upsert(Box::new(e.clone())));
            }
            next_last.insert(e.entity_id.clone(), EntityRecord { hash, entity: e });
        }

        // Tombstone entity ids that vanished (retired or subsumed into an alias).
        for old_id in self.last.keys() {
            if !new_ids.contains(old_id) {
                ops.push(EntityOp::Tombstone(old_id.clone()));
            }
        }

        self.last = next_last;
        ops
    }

    /// Re-publish every current entity with a refreshed `last_updated` (liveness
    /// + late-restart recovery). Content is unchanged, so the stored hash stays.
    pub fn reemit(&mut self, now_ms: i64) -> Vec<EntityOp> {
        let mut ops = Vec::with_capacity(self.last.len());
        for rec in self.last.values_mut() {
            rec.entity.last_updated = now_ms;
            ops.push(EntityOp::Upsert(Box::new(rec.entity.clone())));
        }
        ops
    }

    /// Top-N accumulated names for an arbitrary IP (serves the names queryable).
    pub fn names_for_ip(&self, ip: &str, n: usize) -> Vec<NameVal> {
        self.names.top_n(ip, n)
    }

    /// The current published entity set (serves the entities queryable).
    pub fn current_entities(&self) -> Vec<HostEntity> {
        let mut v: Vec<HostEntity> = self.last.values().map(|r| r.entity.clone()).collect();
        v.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        v
    }

    /// Attach the accumulated passive-DNS names for the entity's IPs, ranked
    /// most-recent-first (deterministic on ties), capped at [`ENTITY_NAMES_CAP`].
    fn inject_names(&self, e: &mut HostEntity) {
        let mut acc: HashMap<(String, String), i64> = HashMap::new();
        for ip in &e.ips {
            for nv in self.names.top_n(ip, MAX_NAMES_PER_IP) {
                let slot = acc.entry((nv.name, nv.provenance)).or_insert(nv.last_seen);
                *slot = (*slot).max(nv.last_seen);
            }
        }
        let mut names: Vec<NameVal> = acc
            .into_iter()
            .map(|((name, provenance), last_seen)| NameVal {
                name,
                provenance,
                last_seen,
            })
            .collect();
        names.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.provenance.cmp(&b.provenance))
        });
        names.truncate(ENTITY_NAMES_CAP);
        e.names = names;
    }

    /// Roll device-liveness up onto the entity (worst-of-members). Skipped when
    /// `status_from_liveness` is disabled or no member has a liveness record.
    fn assign_status(&self, e: &mut HostEntity) {
        if !self.config.status_from_liveness {
            return;
        }
        let mut worst: Option<&str> = None;
        for m in &e.members {
            if let Some(s) = self.liveness.get(&m.source)
                && worst.is_none_or(|w| status_rank(s) > status_rank(w))
            {
                worst = Some(s);
            }
        }
        e.status = worst.map(|s| s.to_string());
    }

    /// Record entity-id lineage: an old id that shares ≥1 member with a new
    /// entity but is not itself a current id was upgraded/merged into that new
    /// entity — put the old id in the new entity's `aliases` (it is tombstoned
    /// by the diff since it is no longer a current id).
    fn apply_upgrades(&self, entities: &mut [HostEntity]) {
        let new_ids: std::collections::HashSet<&str> =
            entities.iter().map(|e| e.entity_id.as_str()).collect();
        // Snapshot old (id -> member set) that are no longer current ids.
        let superseded: Vec<(String, std::collections::HashSet<(String, String)>)> = self
            .last
            .iter()
            .filter(|(id, _)| !new_ids.contains(id.as_str()))
            .map(|(id, rec)| (id.clone(), member_set(&rec.entity)))
            .collect();

        for e in entities.iter_mut() {
            let members = member_set(e);
            for (old_id, old_members) in &superseded {
                if old_id != &e.entity_id && !members.is_disjoint(old_members) {
                    e.aliases.push(old_id.clone());
                }
            }
            e.aliases.sort();
            e.aliases.dedup();
        }
    }
}

/// The set of `(sensor, source)` members of an entity.
fn member_set(e: &HostEntity) -> std::collections::HashSet<(String, String)> {
    e.members
        .iter()
        .map(|m| (m.sensor.clone(), m.source.clone()))
        .collect()
}

/// Worst-of ranking for status rollup: offline > degraded > online > unknown.
fn status_rank(status: &str) -> u8 {
    match status {
        "offline" => 3,
        "degraded" => 2,
        "online" => 1,
        _ => 0,
    }
}

/// Content hash of an entity with `last_updated` zeroed — so pure liveness
/// re-emits (which only bump `last_updated`) don't count as changes.
fn content_hash(entity: &HostEntity) -> u64 {
    let mut clone = entity.clone();
    clone.last_updated = 0;
    let bytes = serde_json::to_vec(&clone).unwrap_or_default();
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Async driver around [`CorrelatorState`].
pub struct Engine {
    state: CorrelatorState,
    rx: mpsc::Receiver<EvidenceMsg>,
    out: mpsc::Sender<EntityOp>,
    debounce: Duration,
    reemit: Duration,
}

impl Engine {
    /// Create the engine. `rx` receives decoded evidence; `out` carries the
    /// entity ops to the publisher.
    pub fn new(
        config: CorrelatorConfig,
        rx: mpsc::Receiver<EvidenceMsg>,
        out: mpsc::Sender<EntityOp>,
    ) -> Self {
        let debounce = Duration::from_millis(config.recompute_debounce_ms);
        let reemit = Duration::from_secs(config.reemit_secs);
        Self {
            state: CorrelatorState::new(config),
            rx,
            out,
            debounce,
            reemit,
        }
    }

    /// Run until the shutdown signal fires.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!(
            debounce_ms = self.debounce.as_millis(),
            reemit_secs = self.reemit.as_secs(),
            "correlation engine started"
        );
        let mut reemit = tokio::time::interval(self.reemit);
        reemit.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reemit.tick().await; // consume the immediate first tick

        // `None` = idle (no pending recompute); `Some(deadline)` = debounce armed.
        let mut deadline: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                msg = self.rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.state.apply(msg);
                            deadline = Some(Instant::now() + self.debounce);
                        }
                        None => break, // subscribers gone
                    }
                }
                _ = async { sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                    deadline = None;
                    let ops = self.state.recompute(current_timestamp_millis());
                    debug!(ops = ops.len(), "recompute produced ops");
                    self.forward(ops).await;
                }
                _ = reemit.tick() => {
                    let ops = self.state.reemit(current_timestamp_millis());
                    debug!(ops = ops.len(), "re-emit");
                    self.forward(ops).await;
                }
            }
        }
        info!("correlation engine stopped");
        Ok(())
    }

    async fn forward(&self, ops: Vec<EntityOp>) {
        for op in ops {
            if self.out.send(op).await.is_err() {
                debug!("entity-op channel closed");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::{HostEvidence, NameObservation};

    fn cfg() -> CorrelatorConfig {
        CorrelatorConfig::default()
    }

    fn self_report(sensor: &str, source: &str, host_id: &str) -> HostEvidence {
        HostEvidence {
            sensor: sensor.into(),
            source: source.into(),
            observer: None,
            host_id: Some(host_id.into()),
            boot_id: None,
            hostname: Some(source.into()),
            fqdn: None,
            ips: vec!["10.0.0.5".into()],
            macs: vec![],
            vendor: None,
            platform: None,
            last_updated: 1000,
        }
    }

    fn hid(b: u8) -> String {
        format!("{:02x}", b).repeat(32)
    }

    #[test]
    fn recompute_emits_upsert_then_no_change() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host(self_report("sysinfo", "host1", &hid(1))));
        let ops = s.recompute(2000);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], EntityOp::Upsert(_)));
        // Idempotent: same evidence → no ops second time.
        let ops2 = s.recompute(3000);
        assert!(ops2.is_empty(), "no content change → no ops");
    }

    #[test]
    fn two_sensor_self_report_merges_to_one_entity() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host(self_report("sysinfo", "host1", &hid(2))));
        s.apply(EvidenceMsg::Host(self_report("netlink", "host1", &hid(2))));
        let ops = s.recompute(2000);
        let upserts: Vec<_> = ops
            .iter()
            .filter_map(|o| match o {
                EntityOp::Upsert(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].members.len(), 2);
    }

    #[test]
    fn stale_evidence_tombstones_entity() {
        let mut s = CorrelatorState::new(cfg()); // ttl 900s
        s.apply(EvidenceMsg::Host(self_report("sysinfo", "host1", &hid(3))));
        let _ = s.recompute(2000);
        // Advance well past the TTL: evidence (last_updated 1000) ages out.
        let ops = s.recompute(1000 + 901_000 + 1);
        assert!(ops.iter().any(|o| matches!(o, EntityOp::Tombstone(_))));
        assert!(s.current_entities().is_empty());
    }

    #[test]
    fn name_enrichment_populates_and_ranks() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host(self_report("sysinfo", "host1", &hid(4))));
        s.apply(EvidenceMsg::Name(NameObservation {
            observer: "netring".into(),
            ip: "10.0.0.5".into(),
            name: "old.example.com".into(),
            provenance: "dns_a".into(),
            last_seen: 500,
        }));
        s.apply(EvidenceMsg::Name(NameObservation {
            observer: "netring".into(),
            ip: "10.0.0.5".into(),
            name: "new.example.com".into(),
            provenance: "dns_ptr".into(),
            last_seen: 900,
        }));
        let ops = s.recompute(2000);
        let e = ops
            .iter()
            .find_map(|o| match o {
                EntityOp::Upsert(e) => Some(e),
                _ => None,
            })
            .unwrap();
        assert_eq!(e.names.len(), 2);
        assert_eq!(
            e.names[0].name, "new.example.com",
            "ranked most-recent first"
        );
    }

    #[test]
    fn status_rolls_up_worst_of_members() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host(self_report("sysinfo", "host1", &hid(5))));
        s.apply(EvidenceMsg::Liveness {
            source: "host1".into(),
            status: "degraded".into(),
        });
        let ops = s.recompute(2000);
        let e = ops
            .iter()
            .find_map(|o| match o {
                EntityOp::Upsert(e) => Some(e),
                _ => None,
            })
            .unwrap();
        assert_eq!(e.status.as_deref(), Some("degraded"));
    }

    #[test]
    fn entity_id_upgrade_records_alias_and_tombstones_old() {
        let mut s = CorrelatorState::new(cfg());
        // First: a fqdn-only observed asset → fallback id.
        let mut asset = HostEvidence {
            sensor: "netring".into(),
            source: "aa-bb".into(),
            observer: Some("netring".into()),
            host_id: None,
            boot_id: None,
            hostname: None,
            fqdn: Some("host1.example.com".into()),
            ips: vec!["10.0.0.5".into()],
            macs: vec!["aa:bb:cc:dd:ee:ff".into()],
            vendor: None,
            platform: None,
            last_updated: 1000,
        };
        s.apply(EvidenceMsg::Host(asset.clone()));
        let ops1 = s.recompute(2000);
        let old_id = ops1
            .iter()
            .find_map(|o| match o {
                EntityOp::Upsert(e) => Some(e.entity_id.clone()),
                _ => None,
            })
            .unwrap();
        assert!(old_id.starts_with("h_"));

        // Now a self-report with a host_id arrives sharing the same MAC+IP, so
        // it merges with the asset and the set gains a host_id → new id.
        let selfrep = HostEvidence {
            sensor: "sysinfo".into(),
            source: "host1".into(),
            observer: None,
            host_id: Some(hid(6)),
            boot_id: None,
            hostname: Some("host1".into()),
            fqdn: Some("host1.example.com".into()),
            ips: vec!["10.0.0.5".into()],
            macs: vec!["aa:bb:cc:dd:ee:ff".into()],
            vendor: None,
            platform: None,
            last_updated: 1500,
        };
        asset.last_updated = 1500; // keep asset fresh
        s.apply(EvidenceMsg::Host(asset));
        s.apply(EvidenceMsg::Host(selfrep));
        let ops2 = s.recompute(2500);

        let new_entity = ops2
            .iter()
            .find_map(|o| match o {
                EntityOp::Upsert(e) => Some(e),
                _ => None,
            })
            .unwrap();
        let new_id = format!("h_{}", &hid(6)[..12]);
        assert_eq!(new_entity.entity_id, new_id);
        assert!(
            new_entity.aliases.contains(&old_id),
            "old fallback id must be recorded as an alias"
        );
        assert!(
            ops2.iter()
                .any(|o| matches!(o, EntityOp::Tombstone(id) if id == &old_id)),
            "old id must be tombstoned"
        );
    }
}
