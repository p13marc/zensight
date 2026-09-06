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
    HostEntity, HostEvidence, NameObservation, NameVal, OperatorAssertion, PdnsRecord,
    current_timestamp_millis,
};

use crate::config::CorrelatorConfig;
use crate::merge;
use crate::store::{EvidenceStore, MAX_NAMES_PER_IP, NameStore};

/// Cap on the number of names attached inline to one entity.
const ENTITY_NAMES_CAP: usize = 32;

/// One decoded input to the engine, produced by the subscribers.
#[derive(Debug, Clone)]
pub enum EvidenceMsg {
    /// A host-identity claim (`state/<sensor>/evidence/{self,device/<d>}`). Boxed:
    /// `HostEvidence` is much larger than the other variants and this message
    /// flows through a channel (keeps the enum from being fat — same reason
    /// [`EntityOp::Upsert`] boxes its entity).
    Host {
        /// The origin the claim was published from — carried through from the
        /// key, because the payload does not have it. Identity does not need
        /// it; the topology graph does (#917).
        origin: String,
        ev: Box<HostEvidence>,
    },
    /// A passive-DNS name observation (`state/<sensor>/evidence/names/<ip-slug>`).
    Name(NameObservation),
    /// A host-evidence tombstone (a `Delete` on
    /// `state/<sensor>/evidence/{self,device/<d>}`): drop that claim now instead of
    /// waiting for it to age out by TTL.
    RemoveHost { sensor: String, source: String },
    /// An operator identity assertion (`@catalog/state/assertion/<id>`, #473).
    ///
    /// The catalog subscribes to its **own** published state here, which looks
    /// circular and is not: it is what keeps the catalog a pure function of the
    /// bus (RFC 06 §5). The assertion a restarted correlator re-seeds from a
    /// storage arrives through exactly this path, so there is one code path, not
    /// a live one and a recovery one.
    Assert(OperatorAssertion),
    /// An assertion was retired (a `Delete` on its key).
    RemoveAssertion { id: String },
    /// A relationship claim (`state/<sensor>/evidence/relation/<id>`, #917).
    ///
    /// Carries the **publishing origin from the key**, which the payload does
    /// not have: a claim says which sensor made it, only the key says which
    /// host that sensor ran on, and an edge's observer set needs both. Boxed
    /// for the same reason as `Host`.
    Relation {
        origin: String,
        ev: Box<zensight_common::relation::RelationshipEvidence>,
    },
    /// A relationship claim was retired (a `Delete` on its key).
    RemoveRelation {
        sensor: String,
        origin: String,
        relation_id: String,
    },
    /// A sensor's alert changed state (`state/<producer>/alert/<key>`, #923).
    ///
    /// Carries the ref built from the KEY — origin and producer are key
    /// chunks, and an `Alert`'s `source` is the polled device for a proxy
    /// sensor (#883), so the payload alone cannot say which host published it.
    /// `alert: None` is a tombstone.
    Alert {
        r: Box<zensight_common::alert::AlertRef>,
        alert: Option<Box<zensight_common::alert::Alert>>,
    },
    /// An operator acknowledgement (`@catalog/state/ack/<alert_ref>`, #922).
    ///
    /// Subscribed from the catalog's **own** published state, exactly as
    /// assertions are: it is what keeps the catalog a pure function of the
    /// bus, and it means a restarted correlator re-seeds acks through the same
    /// path a live one takes rather than through a second recovery path.
    Ack(Box<zensight_common::ack::AlertAck>),
    /// An acknowledgement was retired (a `Delete` on its key).
    RemoveAck(Box<zensight_common::alert::AlertRef>),
    /// A suppression window (`@catalog/state/silence/<id>`, #922).
    Silence(Box<zensight_common::silence::Silence>),
    /// A suppression window was closed (a `Delete` on its key).
    RemoveSilence { id: String },
    /// A sensor's liveliness token appeared or vanished.
    ///
    /// The input to `down`, which is what makes `symptom_of` mean anything: a
    /// machine that stopped answering publishes no alert of its own, so the
    /// only evidence it is the cause is the absence of its token.
    Liveliness { origin: String, alive: bool },
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
    /// Entity id → last published record.
    last: HashMap<String, EntityRecord>,
    /// Operator assertions by id (#473) — an input to the merge, held exactly as
    /// it arrived off the bus.
    assertions: HashMap<String, OperatorAssertion>,
    /// Relationship claims (#917). Deliberately **not** an input to
    /// `merge::correlate`: an edge cannot make two machines the same machine,
    /// and a claim that could would be an identity claim wearing a different
    /// hat. Resolution runs after the merge and reads its finished answer.
    relations: crate::edges::RelationStore,
    /// The published edge set and its change gate.
    edges: crate::edges::EdgeState,
    /// Firing alerts (#923). Like `relations`, deliberately **not** an input
    /// to the merge: an alert cannot make two machines the same machine.
    alerts: crate::incidents::AlertStore,
    /// Operator acknowledgements, keyed by the alert they name.
    acks: std::collections::BTreeMap<
        zensight_common::alert::AlertRef,
        zensight_common::ack::AlertAck,
    >,
    /// Live suppression windows.
    silences: std::collections::BTreeMap<String, zensight_common::silence::Silence>,
    /// The published incident set and its change gate.
    incidents: crate::incidents::IncidentState,
    /// Origins whose liveliness token has been seen and is currently present
    /// (#1101). The alert sweep keeps a live origin's alerts whatever their
    /// age; an origin never seen alive ages out as if dead, so an alert left
    /// behind by a sensor that died before this catalog started still expires.
    live_origins: std::collections::BTreeSet<String>,
    /// Origins whose liveliness token is currently absent.
    ///
    /// Held as *origins* rather than entities because that is what the token's
    /// key carries; `recompute_incidents` maps them to entities through the
    /// same evidence join the incidents use, so the two cannot disagree.
    dead_origins: std::collections::BTreeSet<String>,
}

impl CorrelatorState {
    /// Create an empty state.
    pub fn new(config: CorrelatorConfig) -> Self {
        Self {
            config,
            evidence: EvidenceStore::default(),
            names: NameStore::default(),
            last: HashMap::new(),
            assertions: HashMap::new(),
            relations: crate::edges::RelationStore::default(),
            edges: crate::edges::EdgeState::default(),
            alerts: crate::incidents::AlertStore::default(),
            acks: std::collections::BTreeMap::new(),
            silences: std::collections::BTreeMap::new(),
            incidents: crate::incidents::IncidentState::default(),
            live_origins: std::collections::BTreeSet::new(),
            dead_origins: std::collections::BTreeSet::new(),
        }
    }

    /// Apply one incoming message to the stores.
    pub fn apply(&mut self, msg: EvidenceMsg) {
        match msg {
            EvidenceMsg::Host { origin, ev } => self.evidence.upsert(origin, *ev),
            EvidenceMsg::Name(obs) => self.names.upsert(obs),
            EvidenceMsg::RemoveHost { sensor, source } => {
                self.evidence.remove(&sensor, &source);
            }
            EvidenceMsg::Assert(a) => {
                self.assertions.insert(a.id.clone(), a);
            }
            EvidenceMsg::Relation { origin, ev } => self.relations.upsert(origin, *ev),
            EvidenceMsg::Alert { r, alert } => self.alerts.observe(*r, alert.map(|a| *a)),
            EvidenceMsg::Ack(ack) => {
                self.acks.insert(ack.alert_ref.clone(), *ack);
            }
            EvidenceMsg::RemoveAck(r) => {
                self.acks.remove(&r);
            }
            EvidenceMsg::Silence(s) => {
                self.silences.insert(s.id.clone(), *s);
            }
            EvidenceMsg::RemoveSilence { id } => {
                self.silences.remove(&id);
            }
            EvidenceMsg::Liveliness { origin, alive } => {
                if alive {
                    self.dead_origins.remove(&origin);
                    self.live_origins.insert(origin);
                } else {
                    self.live_origins.remove(&origin);
                    self.dead_origins.insert(origin);
                }
            }
            EvidenceMsg::RemoveRelation {
                sensor,
                origin,
                relation_id,
            } => {
                self.relations.remove(&sensor, &origin, &relation_id);
            }
            EvidenceMsg::RemoveAssertion { id } => {
                self.assertions.remove(&id);
            }
        }
    }

    /// The operator's assertions, indexed for the merge. Note there is no TTL
    /// sweep: operator evidence is **strong and does not age out** (RFC 06 §5.4).
    /// A link survives the machine being off for a month; only an operator
    /// retires it.
    fn assertions(&self) -> merge::Assertions {
        merge::Assertions::new(self.assertions.values().cloned())
    }

    /// The current assertion set (serves the `@rpc/link`/`unlink` idempotency
    /// check and the assertion seed).
    pub fn current_assertions(&self) -> Vec<OperatorAssertion> {
        let mut v: Vec<OperatorAssertion> = self.assertions.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
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

        // Origin-tagged, because `HostEntity::origins` is a conclusion this
        // pass draws (#1007): the origin is in the *key* a claim arrived on
        // and in no payload field, so the merge has to be handed it.
        let live = self.evidence.live_with_origin(now_ms, ttl_ms);
        let mut entities = merge::correlate(&live, &self.config.rules, &self.assertions());

        for e in &mut entities {
            self.inject_names(e);
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

    /// Recompute the **edge** set at `now_ms` and return the ops to publish.
    ///
    /// Called straight after [`CorrelatorState::recompute`], and separately
    /// rather than folded into it: the two outputs go to two key families with
    /// two lifecycles, and an edge pass that could fail or lag must not be able
    /// to delay an entity publish. It reads `self.last`, so it sees exactly the
    /// entity set that was just published — never a half-updated one.
    pub fn recompute_edges(&mut self, now_ms: i64) -> Vec<crate::edges::EdgeOp> {
        let ttl_ms = self.config.evidence_ttl_secs as i64 * 1000;
        self.relations.sweep(now_ms, ttl_ms);
        let live = self.relations.live(now_ms, ttl_ms);
        let entities: Vec<HostEntity> = self.last.values().map(|r| r.entity.clone()).collect();
        self.edges
            .diff(crate::edges::resolve(&live, &entities, now_ms))
    }

    /// Recompute the **incident** set at `now_ms` and return the ops (#923).
    ///
    /// Runs after both other passes for the same reason edges run after
    /// entities: it reads the entity set *and* the edge set as just published,
    /// so an incident can never name an entity id retired in the same pass or
    /// attribute through an edge that no longer exists.
    ///
    /// `down` is the set of entities the catalog believes are not alive.
    /// Resolving that is the caller's job — it needs the liveliness plane and
    /// a clock, neither of which belongs in a pure pass.
    pub fn recompute_incidents(&mut self, now_ms: i64) -> Vec<crate::incidents::IncidentOp> {
        let ttl_ms = self.config.evidence_ttl_secs as i64 * 1000;
        // Alerts age out by their origin's liveliness, not by their firing
        // timestamp (#1101): a firing alert's timestamp does not move.
        let live = &self.live_origins;
        self.alerts
            .sweep(now_ms, ttl_ms, |origin| live.contains(origin));
        // A silence past its window stops applying whether or not its
        // tombstone has arrived, so a partitioned catalog cannot keep an
        // expired suppression alive.
        self.silences.retain(|_, s| now_ms < s.ends_at);
        let firing = self.alerts.firing();
        let evidence = self.evidence.live_with_origin(now_ms, ttl_ms);
        let entities: Vec<HostEntity> = self.last.values().map(|r| r.entity.clone()).collect();
        let edges = self.edges.current();
        let silences: Vec<_> = self.silences.values().cloned().collect();
        // Origins → entities, through the same evidence join the incidents
        // use, so "this entity is down" and "this alert belongs to this
        // entity" can never disagree about who is who.
        let entity_of = crate::incidents::origins_by_entity(&evidence, &entities);
        let down: std::collections::BTreeSet<String> = self
            .dead_origins
            .iter()
            .filter_map(|o| entity_of.get(o).cloned())
            .collect();
        self.incidents.diff(crate::incidents::resolve(
            crate::incidents::Pass {
                firing: &firing,
                evidence: &evidence,
                entities: &entities,
                edges: &edges,
                acks: &self.acks,
                silences: &silences,
                down: &down,
            },
            now_ms,
        ))
    }

    /// Re-publish every current incident with a refreshed `last_updated`.
    pub fn reemit_incidents(&mut self, now_ms: i64) -> Vec<crate::incidents::IncidentOp> {
        self.incidents.reemit(now_ms)
    }

    /// The current published incident set (serves the incidents queryable).
    pub fn current_incidents(&self) -> Vec<zensight_common::incident::Incident> {
        self.incidents.current()
    }

    /// The current acknowledgement set (serves the ack seed queryable, #925).
    ///
    /// Sorted by ref so the seed is deterministic, the same reason
    /// [`Self::current_assertions`] sorts by id.
    pub fn current_acks(&self) -> Vec<zensight_common::ack::AlertAck> {
        self.acks.values().cloned().collect()
    }

    /// The current suppression set (serves the silence seed queryable, #925).
    pub fn current_silences(&self) -> Vec<zensight_common::silence::Silence> {
        self.silences.values().cloned().collect()
    }

    /// The firing alert for `r`, if it is firing right now (#924).
    ///
    /// The gate on `ack`: acknowledging something nobody is reporting is a
    /// suppression waiting to happen — the ack would sit on the key, inert by
    /// the projection rule, and then quietly apply the moment that exact
    /// alert next fired within its `fired_at`.
    pub fn firing_alert(
        &self,
        r: &zensight_common::alert::AlertRef,
    ) -> Option<zensight_common::alert::Alert> {
        self.alerts.firing().into_iter().find_map(
            |(k, a)| {
                if &k == r { Some(a.clone()) } else { None }
            },
        )
    }

    /// Acks whose alert is no longer firing, or whose alert has re-fired past
    /// `fired_at` (#924).
    ///
    /// The sweep's input. Both cases retire the ack, and they are the two
    /// halves of one rule: an ack names an *occurrence*. The occurrence ended
    /// (resolved, tombstoned) or a different one began (re-fire), and either
    /// way the operator who said "I am on this" was talking about something
    /// else.
    pub fn stale_acks(&self) -> Vec<zensight_common::alert::AlertRef> {
        let firing: std::collections::BTreeMap<_, _> = self.alerts.firing().into_iter().collect();
        self.acks
            .iter()
            .filter(|(r, ack)| !ack.applies_to(firing.get(*r).copied()))
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Silences whose window has closed (#924).
    pub fn expired_silences(&self, now_ms: i64) -> Vec<String> {
        self.silences
            .values()
            .filter(|s| now_ms >= s.ends_at)
            .map(|s| s.id.clone())
            .collect()
    }

    /// Whether a silence with this id is currently held.
    pub fn has_silence(&self, id: &str) -> bool {
        self.silences.contains_key(id)
    }

    /// Number of firing alerts held, for health reporting.
    pub fn firing_alerts(&self) -> usize {
        self.alerts.len()
    }

    /// Re-publish every current edge with a refreshed `last_updated`, the
    /// edge-side twin of [`CorrelatorState::reemit`].
    pub fn reemit_edges(&mut self, now_ms: i64) -> Vec<crate::edges::EdgeOp> {
        self.edges.reemit(now_ms)
    }

    /// The current published edge set (serves the edges queryable).
    pub fn current_edges(&self) -> Vec<zensight_common::relation::Edge> {
        self.edges.current()
    }

    /// Number of stored relationship claims, for health reporting.
    pub fn relation_claims(&self) -> usize {
        self.relations.len()
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

/// Shared, lockable correlation state — the engine mutates it; the queryables
/// (commit 4) read snapshots from it. The lock is only ever held across cheap
/// in-memory work (never across an `.await`).
pub type SharedState = std::sync::Arc<std::sync::Mutex<CorrelatorState>>;

/// Async driver around a [`SharedState`].
pub struct Engine {
    state: SharedState,
    rx: mpsc::Receiver<EvidenceMsg>,
    out: mpsc::Sender<EntityOp>,
    /// Optional sink for durable historical passive-DNS records
    /// (`@catalog/state/pdns`, #310).
    /// Fed on every name-store update so a storage backend can capture the full
    /// IP↔name history. `None` disables the historical tier.
    pdns_out: Option<mpsc::Sender<PdnsRecord>>,
    /// Optional sink for resolved edges (`@catalog/state/edge/*`, #917).
    ///
    /// A second output channel rather than a second message on the entity one:
    /// the two families have two lifecycles and two subscribers, and an edge
    /// pass that lags or fails must not be able to delay an entity publish.
    /// `None` disables edge publishing entirely, which is what the demo feed
    /// and every entity-only test use.
    edge_out: Option<mpsc::Sender<crate::edges::EdgeOp>>,
    /// Optional sink for incidents (`@catalog/state/incident/*`, #923).
    ///
    /// A third output channel for the third key family, for the same reason
    /// edges got the second: three lifecycles, three subscriber sets, and an
    /// incident pass that lags must not delay an entity publish. `None`
    /// disables incidents entirely — the `incidents.enabled` kill switch, and
    /// what every entity-only test uses.
    incident_out: Option<mpsc::Sender<crate::incidents::IncidentOp>>,
    debounce: Duration,
    reemit: Duration,
}

impl Engine {
    /// Create the engine. `state` is shared with the queryables; `rx` receives
    /// decoded evidence; `out` carries the entity ops to the publisher.
    pub fn new(
        state: SharedState,
        rx: mpsc::Receiver<EvidenceMsg>,
        out: mpsc::Sender<EntityOp>,
    ) -> Self {
        let (debounce, reemit) = {
            let s = state.lock().unwrap();
            (
                Duration::from_millis(s.config.recompute_debounce_ms),
                Duration::from_secs(s.config.reemit_secs),
            )
        };
        Self {
            state,
            rx,
            out,
            pdns_out: None,
            edge_out: None,
            incident_out: None,
            debounce,
            reemit,
        }
    }

    /// Attach the durable historical passive-DNS sink (`@catalog/state/pdns`,
    /// #310). When set, each name-store update emits a [`PdnsRecord`] with the
    /// IP's full accumulated name set onto this channel for the pdns publisher.
    pub fn with_pdns(mut self, pdns_out: mpsc::Sender<PdnsRecord>) -> Self {
        self.pdns_out = Some(pdns_out);
        self
    }

    /// Attach the resolved-edge sink (`@catalog/state/edge/*`, #917).
    ///
    /// Opt-in, like [`Engine::with_pdns`]: an engine without it computes no
    /// edges and publishes none, which is what the `--demo` feed wants.
    pub fn with_edges(mut self, edge_out: mpsc::Sender<crate::edges::EdgeOp>) -> Self {
        self.edge_out = Some(edge_out);
        self
    }

    /// Attach the incident sink (`@catalog/state/incident/*`, #923).
    ///
    /// Opt-in like the other two: an engine without it computes no incidents
    /// and publishes none.
    pub fn with_incidents(
        mut self,
        incident_out: mpsc::Sender<crate::incidents::IncidentOp>,
    ) -> Self {
        self.incident_out = Some(incident_out);
        self
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
                            // A name observation updates the accumulated names for
                            // its IP; after applying, emit the IP's full name set
                            // as a durable historical `@catalog/state/pdns` record (#310). Cheap
                            // and off the packet hot path — fires per name-store
                            // update, not per packet.
                            let pdns_ip = match &msg {
                                EvidenceMsg::Name(obs) => Some(obs.ip.clone()),
                                _ => None,
                            };
                            {
                                let mut st = self.state.lock().unwrap();
                                st.apply(msg);
                                if let (Some(ip), Some(out)) = (&pdns_ip, &self.pdns_out) {
                                    let names = st.names_for_ip(ip, MAX_NAMES_PER_IP);
                                    let rec = PdnsRecord {
                                        ip: ip.clone(),
                                        names,
                                        last_updated: current_timestamp_millis(),
                                    };
                                    // try_send: never block the engine loop on a
                                    // full historical-tier channel — a dropped
                                    // record is refreshed by the next observation.
                                    if let Err(e) = out.try_send(rec) {
                                        debug!(error = %e, ip = %ip, "pdns channel full/closed; skipping record");
                                    }
                                }
                            }
                            deadline = Some(Instant::now() + self.debounce);
                        }
                        None => break, // subscribers gone
                    }
                }
                _ = async { sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                    deadline = None;
                    let now = current_timestamp_millis();
                    let (ops, edge_ops, incident_ops) = {
                        let mut st = self.state.lock().unwrap();
                        // Entities, then edges, then incidents: each pass
                        // reads the finished answer of the one before it, so
                        // an edge can never name an entity retired in the same
                        // pass and an incident can never attribute through an
                        // edge that no longer exists.
                        let ops = st.recompute(now);
                        let edge_ops = st.recompute_edges(now);
                        let incident_ops = if self.incident_out.is_some() {
                            st.recompute_incidents(now)
                        } else {
                            Vec::new()
                        };
                        (ops, edge_ops, incident_ops)
                    };
                    debug!(
                        ops = ops.len(),
                        edges = edge_ops.len(),
                        incidents = incident_ops.len(),
                        "recompute produced ops"
                    );
                    self.forward(ops).await;
                    self.forward_edges(edge_ops).await;
                    self.forward_incidents(incident_ops).await;
                }
                _ = reemit.tick() => {
                    let now = current_timestamp_millis();
                    let (ops, edge_ops, incident_ops) = {
                        let mut st = self.state.lock().unwrap();
                        (st.reemit(now), st.reemit_edges(now), st.reemit_incidents(now))
                    };
                    debug!(
                        ops = ops.len(),
                        edges = edge_ops.len(),
                        incidents = incident_ops.len(),
                        "re-emit"
                    );
                    self.forward(ops).await;
                    self.forward_edges(edge_ops).await;
                    self.forward_incidents(incident_ops).await;
                }
            }
        }
        info!("correlation engine stopped");
        Ok(())
    }

    async fn forward_incidents(&self, ops: Vec<crate::incidents::IncidentOp>) {
        let Some(out) = &self.incident_out else {
            return;
        };
        for op in ops {
            if out.send(op).await.is_err() {
                debug!("incident-op channel closed");
                return;
            }
        }
    }

    async fn forward_edges(&self, ops: Vec<crate::edges::EdgeOp>) {
        let Some(out) = &self.edge_out else {
            return;
        };
        for op in ops {
            if out.send(op).await.is_err() {
                debug!("edge-op channel closed");
                return;
            }
        }
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
            container_id: None,
            cloud: None,
            last_updated: 1000,
        }
    }

    fn hid(b: u8) -> String {
        format!("{:02x}", b).repeat(32)
    }

    #[test]
    fn recompute_emits_upsert_then_no_change() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(self_report("sysinfo", "host1", &hid(1))),
        });
        let ops = s.recompute(2000);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], EntityOp::Upsert(_)));
        // Idempotent: same evidence → no ops second time.
        let ops2 = s.recompute(3000);
        assert!(ops2.is_empty(), "no content change → no ops");
    }

    #[test]
    fn remove_host_tombstones_the_entity() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(self_report("sysinfo", "host1", &hid(7))),
        });
        let ops = s.recompute(2000);
        assert!(matches!(ops.as_slice(), [EntityOp::Upsert(_)]));
        // A tombstone on that evidence key drops the claim → the entity retires.
        s.apply(EvidenceMsg::RemoveHost {
            sensor: "sysinfo".into(),
            source: "host1".into(),
        });
        let ops = s.recompute(3000);
        assert!(
            ops.iter().any(|o| matches!(o, EntityOp::Tombstone(_))),
            "removing the only member's evidence must tombstone the entity"
        );
    }

    #[test]
    fn two_sensor_self_report_merges_to_one_entity() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(self_report("sysinfo", "host1", &hid(2))),
        });
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(self_report("netlink", "host1", &hid(2))),
        });
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
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(self_report("sysinfo", "host1", &hid(3))),
        });
        let _ = s.recompute(2000);
        // Advance well past the TTL: evidence (last_updated 1000) ages out.
        let ops = s.recompute(1000 + 901_000 + 1);
        assert!(ops.iter().any(|o| matches!(o, EntityOp::Tombstone(_))));
        assert!(s.current_entities().is_empty());
    }

    /// A firing alert whose origin is alive survives the evidence TTL
    /// (#1101). Before this, `recompute_incidents` swept the alert store on
    /// `Alert::timestamp` — the firing *transition*, which never moves — so
    /// every incident older than fifteen minutes was tombstoned mid-fire.
    #[test]
    fn a_live_origins_alert_outlives_the_evidence_ttl() {
        use zensight_common::alert::{Alert, AlertKind, AlertRef, AlertSeverity};
        let alert_for = |origin: &str| {
            let mut a = Alert::new(
                "host1",
                zensight_common::Protocol::Sysinfo,
                AlertKind::Expectation,
                "disk-full",
                AlertSeverity::Critical,
                "/var 97% full",
            );
            a.timestamp = 1_000;
            EvidenceMsg::Alert {
                r: Box::new(AlertRef {
                    origin: origin.into(),
                    producer: "sysinfo".into(),
                    alert_key: "k".into(),
                }),
                alert: Some(Box::new(a)),
            }
        };
        let far_past_ttl = 1_000 + 900_000 * 4; // an hour, four TTLs

        // Alive: kept.
        let mut s = CorrelatorState::new(cfg()); // ttl 900s
        s.apply(EvidenceMsg::Liveliness {
            origin: "h-alive".into(),
            alive: true,
        });
        s.apply(alert_for("h-alive"));
        let _ = s.recompute_incidents(far_past_ttl);
        assert_eq!(
            s.firing_alerts(),
            1,
            "a live origin's alert is not swept by age"
        );

        // Dead: ages out on the TTL, as before.
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Liveliness {
            origin: "h-dead".into(),
            alive: true,
        });
        s.apply(alert_for("h-dead"));
        s.apply(EvidenceMsg::Liveliness {
            origin: "h-dead".into(),
            alive: false,
        });
        let _ = s.recompute_incidents(far_past_ttl);
        assert_eq!(s.firing_alerts(), 0, "a dead origin's alert ages out");

        // Never seen alive (died before this catalog started): ages out too.
        let mut s = CorrelatorState::new(cfg());
        s.apply(alert_for("h-unknown"));
        let _ = s.recompute_incidents(2_000);
        assert_eq!(s.firing_alerts(), 1, "inside the TTL it is still held");
        let _ = s.recompute_incidents(far_past_ttl);
        assert_eq!(
            s.firing_alerts(),
            0,
            "an origin never seen alive is treated as dead"
        );
    }

    #[test]
    fn name_enrichment_populates_and_ranks() {
        let mut s = CorrelatorState::new(cfg());
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(self_report("sysinfo", "host1", &hid(4))),
        });
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

    #[tokio::test]
    async fn name_message_emits_historical_pdns_record() {
        // #310: a passive-DNS name observation flowing through the engine emits a
        // durable `@catalog/state/pdns` record carrying the IP's full accumulated name set.
        let state = std::sync::Arc::new(std::sync::Mutex::new(CorrelatorState::new(cfg())));
        let (tx, rx) = mpsc::channel(16);
        let (op_tx, _op_rx) = mpsc::channel(16);
        let (pdns_tx, mut pdns_rx) = mpsc::channel(16);
        let engine = Engine::new(state, rx, op_tx).with_pdns(pdns_tx);
        let (sh_tx, sh_rx) = watch::channel(false);
        let handle = tokio::spawn(engine.run(sh_rx));

        // Two names for the same IP accumulate into one record's name set.
        for (name, prov, ts) in [
            ("a.example.com", "dns_a", 400),
            ("printer.example.com", "dns_ptr", 500),
        ] {
            tx.send(EvidenceMsg::Name(NameObservation {
                observer: "netring".into(),
                ip: "10.0.0.9".into(),
                name: name.into(),
                provenance: prov.into(),
                last_seen: ts,
            }))
            .await
            .unwrap();
        }

        // The most recent record reflects both accumulated names.
        let mut last = None;
        for _ in 0..2 {
            let rec = tokio::time::timeout(Duration::from_secs(2), pdns_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(rec.ip, "10.0.0.9");
            last = Some(rec);
        }
        let rec = last.unwrap();
        assert_eq!(rec.names.len(), 2, "accumulated both names for the IP");

        let _ = sh_tx.send(true);
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
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
            container_id: None,
            cloud: None,
            last_updated: 1000,
        };
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(asset.clone()),
        });
        let ops1 = s.recompute(2000);
        let old_id = ops1
            .iter()
            .find_map(|o| match o {
                EntityOp::Upsert(e) => Some(e.entity_id.clone()),
                _ => None,
            })
            .unwrap();
        assert!(old_id.starts_with("h-"));

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
            container_id: None,
            cloud: None,
            last_updated: 1500,
        };
        asset.last_updated = 1500; // keep asset fresh
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(asset),
        });
        s.apply(EvidenceMsg::Host {
            origin: "h-demo".into(),
            ev: Box::new(selfrep),
        });
        let ops2 = s.recompute(2500);

        let new_entity = ops2
            .iter()
            .find_map(|o| match o {
                EntityOp::Upsert(e) => Some(e),
                _ => None,
            })
            .unwrap();
        let new_id = format!("h-{}", &hid(6)[..12]);
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
