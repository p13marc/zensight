//! Resolving relationship claims into catalog edges (#917).
//!
//! Sensors publish [`RelationshipEvidence`]: a kind and two
//! [`EndpointClaim`]s carrying what was *observed* — a vmid, a MAC, a gateway
//! address, a target name. This module turns those into [`Edge`] documents
//! whose ends are resolved: an entity id, or an honest `External` for
//! something the fleet can see and runs no sensor on.
//!
//! # Why this is a separate pass, and `merge.rs` never learns about it
//!
//! The catalog's identity merge is a pure function of host evidence, and its
//! determinism is the property everything else rests on. Relationship claims
//! are an *input to nothing* in that merge: an edge cannot make two machines
//! the same machine, and a claim that could would be an identity claim wearing
//! a different hat. So resolution runs strictly *after* `recompute`, reads the
//! union-find's finished answer, and writes to its own output channel. A test
//! pins that `merge.rs` stays relation-free.
//!
//! # Determinism is the acceptance, not a nice-to-have
//!
//! `edge_id` is `fnv1a_64(kind ‖ from ‖ to)` computed **after** resolution, so
//! anything non-deterministic in the resolver — a `HashMap` iteration order
//! reaching the hash — produces different ids across restarts and an endless
//! churn of tombstones and upserts against a fleet that never changed. Every
//! lookup table here is therefore built once per pass and consulted by *sorted*
//! iteration, every multi-valued field is canonicalized before it can reach a
//! hash, and the tests feed shuffled input and compare the whole edge set.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use zensight_common::entity::HostEntity;
use zensight_common::relation::{Edge, Endpoint, EndpointClaim, Observer, RelationshipEvidence};

/// One resolved-edge op, the relation-side twin of
/// [`crate::engine::EntityOp`].
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeOp {
    /// Publish (create or update) this edge.
    Upsert(Box<Edge>),
    /// Tombstone (delete) the edge id.
    Tombstone(String),
}

/// Store of the latest [`RelationshipEvidence`] per `(sensor, origin, relation_id)`.
///
/// Keyed by the publisher as well as the relation, so two sensors claiming the
/// same relationship are two claims that both keep it alive — and one of them
/// going quiet does not retire an edge the other still asserts. That is what
/// makes [`Edge::observers`] plural and what a single-keyed store could not
/// express.
#[derive(Debug, Default)]
pub struct RelationStore {
    map: HashMap<(String, String, String), RelationshipEvidence>,
}

/// Cap on stored claims, mirroring the sensors' own
/// `zensight_sensor_core::relation::MAX_RELATIONS` times a generous fleet.
///
/// The catalog cannot trust a sensor to respect its cap — a misbehaving or
/// forged publisher is exactly the case a cap exists for — so it keeps its
/// own. Over the cap, the oldest claims go first.
pub const MAX_RELATIONS: usize = 100_000;

impl RelationStore {
    /// Insert or replace a claim.
    pub fn upsert(&mut self, origin: String, ev: RelationshipEvidence) {
        let key = (ev.sensor.clone(), origin, ev.relation_id());
        if self.map.len() >= MAX_RELATIONS && !self.map.contains_key(&key) {
            // Evict the oldest rather than refusing the newest: a fleet at the
            // cap should show its *current* shape, not the shape it had when
            // it first hit the ceiling.
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(k, v)| (v.last_updated, (*k).clone()))
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
                tracing::warn!(
                    cap = MAX_RELATIONS,
                    "relation store at capacity; evicted the oldest claim"
                );
            }
        }
        self.map.insert(key, ev);
    }

    /// Drop a claim (a relation-evidence tombstone). Returns whether anything
    /// was removed.
    pub fn remove(&mut self, sensor: &str, origin: &str, relation_id: &str) -> bool {
        self.map
            .remove(&(
                sensor.to_string(),
                origin.to_string(),
                relation_id.to_string(),
            ))
            .is_some()
    }

    /// Remove claims older than `now_ms - ttl_ms`.
    pub fn sweep(&mut self, now_ms: i64, ttl_ms: i64) {
        let cutoff = now_ms - ttl_ms;
        self.map.retain(|_, ev| ev.last_updated >= cutoff);
    }

    /// The TTL-live claims with their publishing origin, in a **deterministic
    /// order**.
    ///
    /// Sorted, not merely collected: this is the input to a content hash, and
    /// a `HashMap`'s iteration order is not stable between runs of the same
    /// binary — never mind between restarts.
    pub fn live(&self, now_ms: i64, ttl_ms: i64) -> Vec<(String, RelationshipEvidence)> {
        let cutoff = now_ms - ttl_ms;
        let mut v: Vec<((String, String, String), RelationshipEvidence)> = self
            .map
            .iter()
            .filter(|(_, ev)| ev.last_updated >= cutoff)
            .map(|(k, ev)| (k.clone(), ev.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.into_iter().map(|(k, ev)| (k.1, ev)).collect()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Everything needed to turn a claim's endpoint into an [`Endpoint`].
///
/// Built once per pass from the entity set the merge just produced, so the
/// resolver never walks the entity list per claim — and, more importantly,
/// never resolves the same attributes two different ways within one pass.
struct Resolver {
    by_host_id: BTreeMap<String, String>,
    by_ip: BTreeMap<String, String>,
    by_mac: BTreeMap<String, String>,
    by_name: BTreeMap<String, String>,
    by_member_source: BTreeMap<String, String>,
}

impl Resolver {
    fn build(entities: &[HostEntity]) -> Self {
        let mut r = Resolver {
            by_host_id: BTreeMap::new(),
            by_ip: BTreeMap::new(),
            by_mac: BTreeMap::new(),
            by_name: BTreeMap::new(),
            by_member_source: BTreeMap::new(),
        };
        // Entities in a stable order, so a value claimed by two entities — a
        // cloned MAC, a hostname reused after a rebuild — always resolves to
        // the same one of them rather than to whichever the iterator reached
        // first. Ambiguity is unavoidable; *unstable* ambiguity is not.
        let mut sorted: Vec<&HostEntity> = entities.iter().collect();
        sorted.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        for e in sorted {
            let id = &e.entity_id;
            if let Some(h) = &e.host_id {
                r.by_host_id.entry(h.clone()).or_insert_with(|| id.clone());
            }
            for ip in &e.ips {
                r.by_ip.entry(ip.clone()).or_insert_with(|| id.clone());
            }
            for mac in &e.macs {
                r.by_mac
                    .entry(mac.to_ascii_lowercase())
                    .or_insert_with(|| id.clone());
            }
            for n in [e.hostname.as_ref(), e.fqdn.as_ref()].into_iter().flatten() {
                r.by_name
                    .entry(n.to_ascii_lowercase())
                    .or_insert_with(|| id.clone());
            }
            for m in &e.members {
                r.by_member_source
                    .entry(m.source.clone())
                    .or_insert_with(|| id.clone());
            }
        }
        r
    }

    /// Resolve one claim, strongest signal first.
    ///
    /// The order is the same ranking the identity merge uses and for the same
    /// reason: a `host_id` is a hashed machine-id and identifies a machine; an
    /// IP identifies a machine *right now*; a MAC is merge evidence that VMs
    /// clone; a name is what someone typed. Trying them in any other order
    /// would let a weaker signal override a stronger one.
    ///
    /// `None` means the claim named nothing at all — not that it named
    /// something unknown, which is [`Endpoint::External`] and a perfectly good
    /// answer.
    fn resolve(&self, c: &EndpointClaim) -> Option<Endpoint> {
        if let Some(h) = &c.host_id
            && let Some(id) = self.by_host_id.get(h)
        {
            return Some(Endpoint::Entity {
                entity_id: id.clone(),
            });
        }
        if let Some(d) = &c.device
            && let Some(id) = self.by_member_source.get(d)
        {
            return Some(Endpoint::Entity {
                entity_id: id.clone(),
            });
        }
        for ip in &c.ips {
            if let Some(id) = self.by_ip.get(ip) {
                return Some(Endpoint::Entity {
                    entity_id: id.clone(),
                });
            }
        }
        for mac in &c.macs {
            if let Some(id) = self.by_mac.get(&mac.to_ascii_lowercase()) {
                return Some(Endpoint::Entity {
                    entity_id: id.clone(),
                });
            }
        }
        if let Some(n) = &c.name
            && let Some(id) = self.by_name.get(&n.to_ascii_lowercase())
        {
            return Some(Endpoint::Entity {
                entity_id: id.clone(),
            });
        }
        if c.is_empty() {
            return None;
        }
        // Observed, not an entity. Honest, and renderable: this is the
        // upstream router and the probe target on the public internet.
        Some(Endpoint::External {
            ip: c.ips.first().cloned(),
            mac: c.macs.first().cloned(),
            name: c.name.clone().or_else(|| c.device.clone()),
        })
    }
}

/// Derive `L2Adjacent` claims from observed-device evidence (#917).
///
/// A third-party identity claim — `state/<sensor>/evidence/device/{device}`,
/// `observer` set — says "the sensor on **this** host saw **that** device".
/// The sensor learned that from an ARP/NDP neighbour table, which is a
/// statement about a link-layer segment: the two are adjacent at L2. That is
/// exactly the inference the GUI used to make for itself from the netlink
/// neighbour table, and moving it here is what lets an exporter, a notifier or
/// a second console see the same segment map.
///
/// Expressed as synthetic [`RelationshipEvidence`] rather than as edges
/// directly, so it flows through the *same* [`resolve`] as every real claim
/// and inherits its determinism, its self-edge rule and its `External`
/// fallback. A second construction path here would be a second place for the
/// edge id to be computed differently.
///
/// Self-reports are skipped: `observer == None` means "this is me", which is
/// identity, not adjacency, and would make every host adjacent to itself.
pub fn l2_claims(
    evidence: &[(String, zensight_common::HostEvidence)],
    now_ms: i64,
) -> Vec<(String, RelationshipEvidence)> {
    let mut out = Vec::new();
    for (origin, ev) in evidence {
        if ev.observer.is_none() {
            continue;
        }
        if origin.is_empty() {
            continue;
        }
        out.push((
            origin.clone(),
            RelationshipEvidence {
                sensor: ev.sensor.clone(),
                source: origin.clone(),
                kind: zensight_common::relation::RelationKind::L2Adjacent,
                from: EndpointClaim::host(origin.clone()),
                to: EndpointClaim {
                    device: Some(ev.source.clone()),
                    ips: ev.ips.clone(),
                    macs: ev.macs.clone(),
                    name: ev.hostname.clone(),
                    ..Default::default()
                },
                attrs: BTreeMap::new(),
                last_updated: now_ms,
            },
        ));
    }
    out
}

/// Resolve every live claim into the current edge set.
///
/// Claims that resolve to the same `(kind, from, to)` merge into one edge with
/// both observers — which is the point of `edge_id` being derived from
/// structure alone.
pub fn resolve(
    claims: &[(String, RelationshipEvidence)],
    entities: &[HostEntity],
    now_ms: i64,
) -> Vec<Edge> {
    let resolver = Resolver::build(entities);
    let mut by_id: BTreeMap<String, Edge> = BTreeMap::new();

    for (origin, ev) in claims {
        let (Some(from), Some(to)) = (resolver.resolve(&ev.from), resolver.resolve(&ev.to)) else {
            // An end that named nothing. Dropped rather than rendered as an
            // anonymous node: half an edge is worse than no edge, because it
            // looks like a discovery.
            continue;
        };
        // A self-edge is not a relationship. It happens legitimately — a
        // gateway that is also the host, a container claim whose IPs resolve
        // back to its own host — and drawing it would put a loop on the map
        // and make every entity its own containment ancestor in #918.
        if from == to {
            continue;
        }
        let edge_id = Edge::edge_id(ev.kind, &from, &to);
        let entry = by_id.entry(edge_id.clone()).or_insert_with(|| Edge {
            edge_id: edge_id.clone(),
            kind: ev.kind,
            from,
            to,
            attrs: BTreeMap::new(),
            observers: Vec::new(),
            last_updated: now_ms,
        });
        // Later claims do not clobber earlier ones: with claims iterated in a
        // sorted order, first-wins makes the merged attr set a function of the
        // claim set rather than of arrival order.
        for (k, v) in &ev.attrs {
            entry.attrs.entry(k.clone()).or_insert_with(|| v.clone());
        }
        entry.observers.push(Observer {
            sensor: ev.sensor.clone(),
            origin: origin.clone(),
        });
    }

    let mut edges: Vec<Edge> = by_id.into_values().collect();
    for e in &mut edges {
        e.canonicalize();
    }
    edges
}

/// The content hash of an edge, excluding `last_updated`.
///
/// The exclusion is the whole point: a refresh that changes only the timestamp
/// must not count as a change, or every sensor's poll interval would republish
/// the entire graph. Same rule, and same reason, as
/// [`crate::engine::content_hash`] for entities.
pub fn content_hash(e: &Edge) -> u64 {
    let mut probe = e.clone();
    probe.last_updated = 0;
    probe.canonicalize();
    let bytes = serde_json::to_vec(&probe).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The published edge set, and the diff that decides what to publish next.
#[derive(Debug, Default)]
pub struct EdgeState {
    /// `edge_id -> (content hash, the edge as published)`.
    last: BTreeMap<String, (u64, Edge)>,
}

impl EdgeState {
    /// Diff `edges` against what was last published and return the ops.
    ///
    /// A restart with unchanged evidence publishes **nothing**: the hash gate
    /// is what keeps a correlator restart from looking, to every subscriber
    /// and every exporter, like the entire fleet's topology changing at once.
    pub fn diff(&mut self, edges: Vec<Edge>) -> Vec<EdgeOp> {
        let present: BTreeSet<String> = edges.iter().map(|e| e.edge_id.clone()).collect();
        let mut ops = Vec::new();
        let mut next: BTreeMap<String, (u64, Edge)> = BTreeMap::new();
        for e in edges {
            let hash = content_hash(&e);
            if self.last.get(&e.edge_id).map(|(h, _)| *h) != Some(hash) {
                ops.push(EdgeOp::Upsert(Box::new(e.clone())));
            }
            next.insert(e.edge_id.clone(), (hash, e));
        }
        for old in self.last.keys() {
            if !present.contains(old) {
                ops.push(EdgeOp::Tombstone(old.clone()));
            }
        }
        self.last = next;
        ops
    }

    /// Re-publish every current edge with a refreshed `last_updated`. Content
    /// is unchanged, so the stored hash stays.
    pub fn reemit(&mut self, now_ms: i64) -> Vec<EdgeOp> {
        let mut ops = Vec::with_capacity(self.last.len());
        for (_, e) in self.last.values_mut() {
            e.last_updated = now_ms;
            ops.push(EdgeOp::Upsert(Box::new(e.clone())));
        }
        ops
    }

    /// The current published edge set, sorted (serves the edges queryable).
    pub fn current(&self) -> Vec<Edge> {
        self.last.values().map(|(_, e)| e.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.last.len()
    }

    pub fn is_empty(&self) -> bool {
        self.last.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::entity::MemberClaim;
    use zensight_common::relation::RelationKind;

    fn entity(id: &str, host_id: Option<&str>, ips: &[&str], macs: &[&str]) -> HostEntity {
        HostEntity {
            entity_id: id.into(),
            aliases: Vec::new(),
            host_id: host_id.map(Into::into),
            boot_id: None,
            ips: ips.iter().map(|s| s.to_string()).collect(),
            macs: macs.iter().map(|s| s.to_string()).collect(),
            container_ids: Vec::new(),
            hostname: None,
            fqdn: None,
            names: Vec::new(),
            vendor: None,
            platform: None,
            members: Vec::new(),
            status: None,
            last_updated: 0,
        }
    }

    fn claim(
        sensor: &str,
        kind: RelationKind,
        from: EndpointClaim,
        to: EndpointClaim,
        ts: i64,
    ) -> RelationshipEvidence {
        RelationshipEvidence {
            sensor: sensor.into(),
            source: "src".into(),
            kind,
            from,
            to,
            attrs: BTreeMap::new(),
            last_updated: ts,
        }
    }

    /// #917's headline acceptance: the same evidence in any order produces an
    /// identical edge set.
    ///
    /// Not "an equivalent set" — identical, including ids, observer order and
    /// serialized bytes. `edge_id` is hashed after resolution, so a `HashMap`
    /// iteration order reaching it produces different ids across restarts and
    /// an endless churn of tombstones and upserts against a fleet that never
    /// changed.
    #[test]
    fn shuffled_evidence_produces_an_identical_edge_set() {
        let entities = vec![
            entity("h-aaa", Some("hid-a"), &["10.0.0.1"], &[]),
            entity(
                "h-bbb",
                Some("hid-b"),
                &["10.0.0.2"],
                &["AA:BB:CC:00:00:01"],
            ),
            entity("h-ccc", Some("hid-c"), &["10.0.0.3"], &[]),
        ];
        let mut claims = vec![
            (
                "h-aaa".to_string(),
                claim(
                    "pve",
                    RelationKind::Hosts,
                    EndpointClaim::host("hid-a"),
                    EndpointClaim {
                        macs: vec!["aa:bb:cc:00:00:01".into()],
                        ..Default::default()
                    },
                    1,
                ),
            ),
            (
                "h-bbb".to_string(),
                claim(
                    "container",
                    RelationKind::Runs,
                    EndpointClaim::host("hid-b"),
                    EndpointClaim {
                        ips: vec!["10.0.0.3".into()],
                        ..Default::default()
                    },
                    2,
                ),
            ),
            (
                "h-ccc".to_string(),
                claim(
                    "netlink",
                    RelationKind::GatewayOf,
                    EndpointClaim {
                        ips: vec!["10.0.0.1".into()],
                        ..Default::default()
                    },
                    EndpointClaim::host("hid-c"),
                    3,
                ),
            ),
        ];
        let first = resolve(&claims, &entities, 100);
        assert_eq!(first.len(), 3, "{first:#?}");

        claims.reverse();
        let reversed = resolve(&claims, &entities, 100);
        assert_eq!(first, reversed);
        // Byte-identical, not merely equal: the hash gate compares encodings.
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&reversed).unwrap()
        );

        claims.rotate_left(1);
        let mut ents = entities.clone();
        ents.reverse();
        assert_eq!(first, resolve(&claims, &ents, 100));
    }

    /// A restart with unchanged evidence publishes nothing.
    ///
    /// Without the hash gate, every correlator restart would look — to every
    /// subscriber, every exporter and every notifier — like the whole fleet's
    /// topology changing at once.
    #[test]
    fn an_unchanged_pass_publishes_nothing() {
        let entities = vec![
            entity("h-aaa", Some("hid-a"), &[], &[]),
            entity("h-bbb", Some("hid-b"), &[], &[]),
        ];
        let claims = vec![(
            "h-aaa".to_string(),
            claim(
                "pve",
                RelationKind::Hosts,
                EndpointClaim::host("hid-a"),
                EndpointClaim::host("hid-b"),
                1,
            ),
        )];
        let mut state = EdgeState::default();
        assert_eq!(state.diff(resolve(&claims, &entities, 100)).len(), 1);
        assert!(
            state.diff(resolve(&claims, &entities, 100)).is_empty(),
            "an unchanged pass must be silent"
        );
        // A refreshed claim is still unchanged content: only the timestamp
        // moved, and republishing on that would mean republishing the graph on
        // every sensor poll interval.
        let refreshed = vec![(
            "h-aaa".to_string(),
            claim(
                "pve",
                RelationKind::Hosts,
                EndpointClaim::host("hid-a"),
                EndpointClaim::host("hid-b"),
                9_999,
            ),
        )];
        assert!(state.diff(resolve(&refreshed, &entities, 200)).is_empty());
    }

    /// Evidence ageing out produces a tombstone.
    #[test]
    fn an_aged_out_claim_tombstones_its_edge() {
        let entities = vec![
            entity("h-aaa", Some("hid-a"), &[], &[]),
            entity("h-bbb", Some("hid-b"), &[], &[]),
        ];
        let mut store = RelationStore::default();
        store.upsert(
            "h-aaa".into(),
            claim(
                "pve",
                RelationKind::Hosts,
                EndpointClaim::host("hid-a"),
                EndpointClaim::host("hid-b"),
                1_000,
            ),
        );
        let mut state = EdgeState::default();
        let live = store.live(1_000, 900_000);
        let ops = state.diff(resolve(&live, &entities, 1_000));
        assert!(matches!(ops.as_slice(), [EdgeOp::Upsert(_)]));

        // Well past the TTL.
        store.sweep(2_000_000, 900_000);
        assert!(store.is_empty());
        let ops = state.diff(resolve(
            &store.live(2_000_000, 900_000),
            &entities,
            2_000_000,
        ));
        assert!(matches!(ops.as_slice(), [EdgeOp::Tombstone(_)]), "{ops:#?}");
        assert!(state.is_empty());
    }

    /// Two sensors seeing one relationship produce one edge with two
    /// observers — and one of them going quiet does not retire it.
    #[test]
    fn two_observers_are_one_edge_and_either_alone_keeps_it() {
        let entities = vec![
            entity("h-aaa", Some("hid-a"), &[], &[]),
            entity("h-bbb", Some("hid-b"), &[], &[]),
        ];
        let mk = |sensor: &str| {
            (
                "h-aaa".to_string(),
                claim(
                    sensor,
                    RelationKind::Runs,
                    EndpointClaim::host("hid-a"),
                    EndpointClaim::host("hid-b"),
                    1,
                ),
            )
        };
        let both = vec![mk("container"), mk("systemd")];
        let edges = resolve(&both, &entities, 100);
        assert_eq!(edges.len(), 1, "one relationship is one edge");
        assert_eq!(edges[0].observers.len(), 2);

        let mut state = EdgeState::default();
        state.diff(edges);
        // One observer stops claiming: the edge changes (the observer list is
        // content) but is NOT retired, because the other still asserts it.
        let ops = state.diff(resolve(&[mk("systemd")], &entities, 100));
        assert!(matches!(ops.as_slice(), [EdgeOp::Upsert(_)]), "{ops:#?}");
        assert_eq!(state.len(), 1);
    }

    /// An unresolvable end becomes an honest `External`, not a dropped edge.
    #[test]
    fn an_unknown_gateway_becomes_an_external_endpoint() {
        let entities = vec![entity("h-aaa", Some("hid-a"), &[], &[])];
        let claims = vec![(
            "h-aaa".to_string(),
            claim(
                "netlink",
                RelationKind::GatewayOf,
                EndpointClaim {
                    ips: vec!["192.168.1.1".into()],
                    macs: vec!["FF:EE:DD:00:00:01".into()],
                    ..Default::default()
                },
                EndpointClaim::host("hid-a"),
                1,
            ),
        )];
        let edges = resolve(&claims, &entities, 100);
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].from,
            Endpoint::External {
                ip: Some("192.168.1.1".into()),
                mac: Some("FF:EE:DD:00:00:01".into()),
                name: None
            },
            "the upstream router the fleet can see and runs no sensor on"
        );
    }

    /// An end that named nothing at all drops the edge.
    #[test]
    fn a_claim_naming_nothing_produces_no_edge() {
        let entities = vec![entity("h-aaa", Some("hid-a"), &[], &[])];
        let claims = vec![(
            "h-aaa".to_string(),
            claim(
                "probe",
                RelationKind::Probes,
                EndpointClaim::host("hid-a"),
                EndpointClaim::default(),
                1,
            ),
        )];
        assert!(
            resolve(&claims, &entities, 100).is_empty(),
            "half an edge is worse than none: it looks like a discovery"
        );
    }

    /// A claim whose ends resolve to the same entity is not an edge.
    #[test]
    fn a_self_edge_is_dropped() {
        // Legitimately reachable: a host that is its own gateway, or a
        // container claim whose IPs resolve back to its own host. Drawing it
        // would loop the map and make the entity its own containment ancestor.
        let entities = vec![entity("h-aaa", Some("hid-a"), &["10.0.0.1"], &[])];
        let claims = vec![(
            "h-aaa".to_string(),
            claim(
                "netlink",
                RelationKind::GatewayOf,
                EndpointClaim {
                    ips: vec!["10.0.0.1".into()],
                    ..Default::default()
                },
                EndpointClaim::host("hid-a"),
                1,
            ),
        )];
        assert!(resolve(&claims, &entities, 100).is_empty());
    }

    /// A device slug resolves through the entity's member sources — which is
    /// what joins pve's `vmid` claim to the guest's own sensor.
    #[test]
    fn a_device_claim_resolves_through_member_sources() {
        let mut guest = entity("h-guest", Some("hid-g"), &[], &[]);
        guest.members.push(MemberClaim {
            sensor: "pve".into(),
            source: "101".into(),
            rule: "device".into(),
            confidence: 1.0,
            last_seen: 0,
        });
        let entities = vec![entity("h-node", Some("hid-n"), &[], &[]), guest];
        let claims = vec![(
            "h-node".to_string(),
            claim(
                "pve",
                RelationKind::Hosts,
                EndpointClaim::host("hid-n"),
                EndpointClaim::device("101"),
                1,
            ),
        )];
        let edges = resolve(&claims, &entities, 100);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to.entity_id(), Some("h-guest"));
    }

    fn observed(sensor: &str, source: &str, ips: &[&str]) -> zensight_common::HostEvidence {
        zensight_common::HostEvidence {
            sensor: sensor.into(),
            source: source.into(),
            observer: Some(sensor.into()),
            host_id: None,
            boot_id: None,
            hostname: Some(source.into()),
            fqdn: None,
            ips: ips.iter().map(|s| s.to_string()).collect(),
            macs: Vec::new(),
            vendor: None,
            platform: None,
            container_id: None,
            cloud: None,
            last_updated: 1,
        }
    }

    /// An observed-device claim becomes an `L2Adjacent` edge between the
    /// observing host and the device it saw.
    #[test]
    fn observed_device_evidence_derives_l2_adjacency() {
        let seen = entity("h-seen", None, &["10.0.0.9"], &[]);
        let entities = vec![entity("h-obs", Some("h-obs"), &["10.0.0.1"], &[]), seen];
        let ev = vec![(
            "h-obs".to_string(),
            observed("netlink", "dev1", &["10.0.0.9"]),
        )];
        let claims = l2_claims(&ev, 100);
        assert_eq!(claims.len(), 1);
        let edges = resolve(&claims, &entities, 100);
        assert_eq!(edges.len(), 1, "{edges:#?}");
        assert_eq!(edges[0].kind, RelationKind::L2Adjacent);
        assert_eq!(edges[0].from.entity_id(), Some("h-obs"));
        assert_eq!(edges[0].to.entity_id(), Some("h-seen"));
    }

    /// A self-report is identity, not adjacency.
    #[test]
    fn a_self_report_derives_no_adjacency() {
        // `observer == None` means "this is me". Deriving adjacency from it
        // would make every host adjacent to itself and put a loop on every
        // node of the map.
        let mut ev = observed("sysinfo", "hostA", &["10.0.0.1"]);
        ev.observer = None;
        assert!(l2_claims(&[("h-obs".to_string(), ev)], 100).is_empty());
    }

    /// An observation whose device resolves back to the observer is not an
    /// edge — the self-edge rule in `resolve` catches it.
    #[test]
    fn observing_your_own_address_is_not_adjacency() {
        let entities = vec![entity("h-obs", Some("h-obs"), &["10.0.0.1"], &[])];
        let ev = vec![(
            "h-obs".to_string(),
            observed("netlink", "self", &["10.0.0.1"]),
        )];
        assert!(resolve(&l2_claims(&ev, 100), &entities, 100).is_empty());
    }

    /// `merge.rs` never learns about relationships.
    ///
    /// The identity merge is a pure function of host evidence and its
    /// determinism is what every other guarantee rests on. An edge cannot make
    /// two machines the same machine, and a relationship claim that could
    /// would be an identity claim wearing a different hat — so the moment
    /// `merge.rs` reads one, the two concerns are entangled and the next
    /// person to touch either has to reason about both.
    ///
    /// A grep, deliberately: the property is "this file does not mention
    /// relations", and no type signature can express that.
    #[test]
    fn the_identity_merge_stays_relation_free() {
        let src = include_str!("merge.rs");
        // Specific identifiers, not bare words: "correlation" contains
        // "relation", and a test that cries wolf gets deleted rather than
        // heeded.
        for needle in [
            "RelationshipEvidence",
            "RelationKind",
            "RelationStore",
            "EndpointClaim",
            "relation_id",
            "EdgeOp",
            "edge_id",
            "::edges",
        ] {
            assert!(
                !src.contains(needle),
                "merge.rs mentions {needle:?}: identity and topology must not \
                 become one problem"
            );
        }
    }

    /// Ambiguity resolves the same way every time.
    #[test]
    fn a_value_claimed_by_two_entities_resolves_stably() {
        // A cloned MAC across two VMs: which entity wins is arbitrary, but it
        // must not change between passes or the edge id churns forever.
        let mac = "AA:BB:CC:00:00:09";
        let a = entity("h-bbb", Some("hid-b"), &[], &[mac]);
        let b = entity("h-aaa", Some("hid-a"), &[], &[mac]);
        let claims = vec![(
            "h-zzz".to_string(),
            claim(
                "netlink",
                RelationKind::L2Adjacent,
                EndpointClaim::host("hid-z"),
                EndpointClaim {
                    macs: vec![mac.into()],
                    ..Default::default()
                },
                1,
            ),
        )];
        let z = entity("h-zzz", Some("hid-z"), &[], &[]);
        let forward = resolve(&claims, &[a.clone(), b.clone(), z.clone()], 100);
        let backward = resolve(&claims, &[b, a, z], 100);
        assert_eq!(forward, backward);
        // Lowest entity_id wins, deterministically.
        assert_eq!(forward[0].to.entity_id(), Some("h-aaa"));
    }
}
