//! The relationship model (#915): what a sensor *claims* about how two things
//! are connected, and what the catalog *concludes* from those claims.
//!
//! # Why this exists
//!
//! The topology graph was derived entirely inside the GUI, from telemetry it
//! happened to have in memory. That made it the one thing on this bus that no
//! exporter, notifier or second console could see — and it made every
//! consumer that wanted it re-implement the derivation. This module moves the
//! *inputs* onto the bus (sensors publish [`RelationshipEvidence`]) and the
//! *conclusion* onto the bus (the catalog publishes [`Edge`]).
//!
//! # The two halves, and why they are different types
//!
//! A sensor does not know entity ids. It knows what it observed: a vmid, a
//! MAC, a gateway address, a probe target's hostname. So evidence carries an
//! [`EndpointClaim`] — observable attributes, no identity — exactly as
//! [`crate::HostEvidence`] does for host identity. Resolving a claim to an
//! entity is the catalog's job, because the catalog is the only participant
//! that has run the union-find.
//!
//! The catalog's output carries [`Endpoint`], which *is* resolved: either an
//! entity id, or an honest `External` for something the fleet observed but
//! does not run a sensor on (an upstream router, a probe target on the public
//! internet). Collapsing the two into one type would force every consumer to
//! ask "is this resolved yet?" on every read, and would let an unresolved
//! claim reach a UI as though it were a conclusion.
//!
//! # Determinism is the contract
//!
//! [`Edge::edge_id`] is derived from `(kind, from, to)` and nothing else, so
//! the same relationship gets the same key from any correlator, on any
//! restart, in any evidence arrival order. That is what makes republishing an
//! unchanged edge a no-op LWW overwrite instead of a second document, and what
//! lets a tombstone name the thing it retires. Anything non-deterministic
//! reaching that hash — a `HashMap` iteration order, a clock — produces a
//! churn of tombstones and upserts across restarts, so [`Edge::canonicalize`]
//! exists and the tests pin it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How two things are connected.
///
/// The set is deliberately small and structural. It names relationships that
/// are *facts about the deployment* — a guest runs on this node, this is the
/// gateway — not observations that happen to be true this minute. Flow
/// adjacency is the counter-example and is deliberately absent: it is
/// per-observed-peer, unbounded, and belongs on an `@rpc` overlay rather than
/// in a cardinality-budgeted state family.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// A hypervisor node hosts a guest (`from` = node, `to` = guest).
    Hosts,
    /// A host runs a container (`from` = host, `to` = container).
    Runs,
    /// `from` is the gateway for `to`.
    GatewayOf,
    /// `from` is a vantage point that checks `to` (a synthetic probe target).
    Probes,
    /// `from` and `to` share a link-layer segment. Symmetric, and the only
    /// kind that is *not* containment — see [`RelationKind::is_containment`].
    L2Adjacent,
}

impl RelationKind {
    /// The stable wire token. Used in [`Edge::edge_id`], so it must never
    /// change for an existing kind: it is part of the key.
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::Hosts => "hosts",
            RelationKind::Runs => "runs",
            RelationKind::GatewayOf => "gateway_of",
            RelationKind::Probes => "probes",
            RelationKind::L2Adjacent => "l2_adjacent",
        }
    }

    /// Whether failure propagates along this edge from `from` to `to`.
    ///
    /// Containment means "`to` cannot work if `from` is down": a guest cannot
    /// outlive its hypervisor, a container cannot outlive its host, a probe
    /// result cannot outlive the vantage that measures it, and a host behind a
    /// dead gateway is unreachable *from here*.
    ///
    /// [`RelationKind::L2Adjacent`] is inert on purpose. Sharing a segment
    /// says nothing about dependency — two hosts on one switch are peers, not
    /// parent and child — and treating it as containment would make impact
    /// attribution flood the segment and blame an arbitrary neighbour.
    pub fn is_containment(self) -> bool {
        !matches!(self, RelationKind::L2Adjacent)
    }
}

/// What a sensor observed about one end of a relationship.
///
/// No identity: a sensor knows a vmid, a MAC, an address, a name. Which
/// entity that *is* — if any — is the catalog's conclusion, and duplicating
/// that judgement in every sensor is how two sensors come to disagree about
/// which machine is which.
///
/// Every field is optional and defaulted, and an all-empty claim is legal but
/// useless: [`EndpointClaim::is_empty`] says so, and the resolver drops it
/// rather than inventing an endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EndpointClaim {
    /// A self-claim: the publishing host's own stable `host_id`. Present when
    /// this end *is* the sensor's own host, which is the strongest claim
    /// available and the one the resolver prefers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// An observed-device slug — the same vocabulary as
    /// `evidence/device/{device}`, so a relation claim and an identity claim
    /// about the same thing join without a second naming scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Observed addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Observed MACs — merge evidence, never identity on their own (VMs clone
    /// MACs), exactly as in [`crate::HostEvidence`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// A human name for the end: a probe target's hostname, a guest's name.
    /// Display-grade, and the last thing the resolver tries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl EndpointClaim {
    /// A self-claim by stable host id.
    pub fn host(host_id: impl Into<String>) -> Self {
        EndpointClaim {
            host_id: Some(host_id.into()),
            ..Default::default()
        }
    }

    /// A claim about an observed device slug.
    pub fn device(device: impl Into<String>) -> Self {
        EndpointClaim {
            device: Some(device.into()),
            ..Default::default()
        }
    }

    /// Nothing was claimed — no id, no device, no address, no name. Such an
    /// end cannot be resolved and cannot be rendered; the resolver drops the
    /// relation rather than emitting half an edge.
    pub fn is_empty(&self) -> bool {
        self.host_id.is_none()
            && self.device.is_none()
            && self.ips.is_empty()
            && self.macs.is_empty()
            && self.name.is_none()
    }

    /// Sort and dedup the multi-valued fields, so two claims built from the
    /// same observations in different orders are equal and hash alike.
    pub fn canonicalize(&mut self) {
        self.ips.sort();
        self.ips.dedup();
        self.macs.sort();
        self.macs.dedup();
    }
}

/// One sensor's claim that two things are related, published on
/// `state/<sensor>/evidence/relation/{relation_id}`.
///
/// It is *evidence*, in the same sense as [`crate::HostEvidence`]: an
/// observation with a publisher and a timestamp, not a conclusion. The
/// catalog is the only reader that turns it into an [`Edge`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RelationshipEvidence {
    /// Publishing sensor (`"pve"`, `"container"`, `"probe"`, `"netlink"`).
    pub sensor: String,
    /// The `source` this claim is made from — the observing host, in the same
    /// vocabulary telemetry keys use.
    pub source: String,
    /// What kind of relationship is claimed.
    pub kind: RelationKind,
    /// The `from` end. Direction is meaningful for every containment kind:
    /// `from` contains, `to` is contained.
    pub from: EndpointClaim,
    /// The `to` end.
    pub to: EndpointClaim,
    /// Kind-specific detail — `bridge`/`vlan` for a NIC, `unit` for a
    /// container's owning systemd unit, `kind` for a probe. Free-form on
    /// purpose: a closed enum here would need a release to add a field that
    /// only one sensor emits and only a tooltip reads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
    /// Unix epoch millis the claim was last refreshed.
    pub last_updated: i64,
}

impl RelationshipEvidence {
    /// The key chunk for this claim: stable across refreshes, derived from
    /// `(kind, from, to)` only.
    ///
    /// Deliberately **not** a function of `last_updated` or of the publishing
    /// sensor. A sensor that re-observes the same relationship must overwrite
    /// its own previous claim (LWW on one key), not accumulate one key per
    /// refresh — that is the difference between a bounded family and a leak
    /// that quietly outgrows the declared cardinality.
    pub fn relation_id(&self) -> String {
        let mut me = self.clone();
        me.canonicalize();
        format!(
            "r-{:016x}",
            fnv1a_64(&triple_repr(
                me.kind,
                &claim_repr(&me.from),
                &claim_repr(&me.to)
            ))
        )
    }

    /// Order the multi-valued fields so the id and the encoding do not depend
    /// on the order the sensor happened to observe things in.
    pub fn canonicalize(&mut self) {
        self.from.canonicalize();
        self.to.canonicalize();
    }
}

/// A resolved end of an [`Edge`].
///
/// `External` is not a failure mode — it is the honest answer for a thing the
/// fleet can see but does not run a sensor on: the upstream router, a probe
/// target on the public internet. Rendering those as nodes is the difference
/// between a map that ends at the edge of the fleet and one that shows where
/// the fleet connects to the world.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Endpoint {
    /// A host the catalog knows: the merged entity's id.
    Entity { entity_id: String },
    /// Observed, but not an entity. At least one field is always set — the
    /// resolver drops a relation whose end is entirely unidentifiable rather
    /// than emitting an anonymous node.
    External {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ip: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mac: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl Endpoint {
    /// The entity id, when this end resolved to one.
    pub fn entity_id(&self) -> Option<&str> {
        match self {
            Endpoint::Entity { entity_id } => Some(entity_id),
            Endpoint::External { .. } => None,
        }
    }

    /// The canonical byte string this end contributes to [`Edge::edge_id`].
    ///
    /// Fixed field order, explicit empties, and a prefix that keeps the two
    /// variants apart: without the prefix an entity literally named after an
    /// IP would hash to the same edge as the external endpoint at that IP.
    fn repr(&self) -> String {
        match self {
            Endpoint::Entity { entity_id } => format!("e:{entity_id}"),
            Endpoint::External { ip, mac, name } => format!(
                "x:{}|{}|{}",
                ip.as_deref().unwrap_or(""),
                mac.as_deref().unwrap_or(""),
                name.as_deref().unwrap_or("")
            ),
        }
    }
}

/// Which sensor, on which origin, still asserts an edge.
///
/// Plural because the same relationship is often seen from both sides — a
/// container sensor and a systemd sensor on one host, two netlink sensors
/// either side of a segment. Keeping the list is what lets the catalog
/// tombstone an edge when the *last* observer stops claiming it, rather than
/// when the first one does.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct Observer {
    /// The publishing sensor.
    pub sensor: String,
    /// The origin it published from.
    pub origin: String,
}

/// The catalog's conclusion: one relationship, both ends resolved.
///
/// Published on `@catalog/state/edge/{edge_id}`, tombstoned by `delete` when
/// the last observer stops claiming it — the same lifecycle as
/// `@catalog/state/entity/{entity_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Edge {
    /// Derived from `(kind, from, to)` — see [`Edge::edge_id`]. Carried in the
    /// payload as well as the key so a document that has been copied,
    /// exported or replayed still names itself.
    pub edge_id: String,
    pub kind: RelationKind,
    /// The containing / observing end.
    pub from: Endpoint,
    /// The contained / observed end.
    pub to: Endpoint,
    /// Merged `attrs` from every contributing claim.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
    /// Who still claims this edge. Sorted by [`Edge::canonicalize`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<Observer>,
    /// Unix epoch millis of the most recent contributing claim.
    pub last_updated: i64,
}

impl Edge {
    /// The key chunk for a `(kind, from, to)` triple.
    ///
    /// A 64-bit FNV-1a over a canonical representation, hex-encoded with an
    /// `e-` prefix so it is chunk-legal and visibly an edge id in a log line.
    /// The hash is over *structure*, never over the observer set, the attrs or
    /// the timestamp: a second sensor confirming an edge must land on the same
    /// key as the first, or the catalog would publish the same relationship
    /// twice and the cardinality budget would count it twice.
    pub fn edge_id(kind: RelationKind, from: &Endpoint, to: &Endpoint) -> String {
        format!(
            "e-{:016x}",
            fnv1a_64(&triple_repr(kind, &from.repr(), &to.repr()))
        )
    }

    /// Sort the multi-valued fields into a canonical order, so two edges built
    /// from the same claims in different arrival orders serialize
    /// byte-identically.
    ///
    /// The correlator calls this before publishing and before hashing the
    /// document for the change gate. Without it, a `HashMap` iteration order
    /// reaching the observer list is enough to make every restart look like a
    /// fleet-wide change and republish every edge.
    pub fn canonicalize(&mut self) {
        self.observers.sort();
        self.observers.dedup();
    }

    /// Whether failure propagates `from` → `to` along this edge.
    pub fn is_containment(&self) -> bool {
        self.kind.is_containment()
    }
}

/// The canonical byte string a `(kind, from, to)` triple hashes over.
///
/// `\u{1f}` (unit separator) joins the parts: it cannot occur in a key chunk,
/// an entity id, an address or a name, so no combination of field values can
/// forge a different triple's representation. Joining with `-` or `/` would
/// let `("a-b", "c")` and `("a", "b-c")` collide.
fn triple_repr(kind: RelationKind, from: &str, to: &str) -> String {
    format!("{}\u{1f}{from}\u{1f}{to}", kind.as_str())
}

/// The canonical representation of an unresolved claim, for `relation_id`.
fn claim_repr(c: &EndpointClaim) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        c.host_id.as_deref().unwrap_or(""),
        c.device.as_deref().unwrap_or(""),
        c.ips.join(","),
        c.macs.join(","),
        c.name.as_deref().unwrap_or("")
    )
}

/// FNV-1a, 64-bit. Not cryptographic and does not need to be: this names a
/// key, it does not authenticate one. Chosen over a `DefaultHasher` because
/// `std`'s is explicitly not stable across releases, and a key that changes
/// when the toolchain does would tombstone and republish the entire edge set
/// on a Rust upgrade.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::Format;

    fn entity(id: &str) -> Endpoint {
        Endpoint::Entity {
            entity_id: id.into(),
        }
    }

    /// The RFC's published test vectors (zenkey RFC 11 §3.3, v1.30).
    ///
    /// The spec says "implementations MUST reproduce this". Nothing enforced
    /// that until this test: a refactor of `repr`, of the separator, or of the
    /// hash would keep every *other* test in this file green — they all check
    /// self-consistency — while silently re-keying the entire edge family and
    /// diverging from the document two implementations agree through.
    #[test]
    fn the_rfc_test_vectors_still_hold() {
        // A pve node hosts a guest that resolved to an entity.
        //   triple = "hosts" 1f "e:h-3fa9c2d41b7e" 1f "e:h-9d02aa17c44f"
        let node = entity("h-3fa9c2d41b7e");
        let guest = entity("h-9d02aa17c44f");
        assert_eq!(
            Edge::edge_id(RelationKind::Hosts, &node, &guest),
            "e-2879d4667f9d946d"
        );
        // Direction and kind are both in the hash, and the RFC pins both.
        assert_eq!(
            Edge::edge_id(RelationKind::Hosts, &guest, &node),
            "e-37f51d33d897f9b3"
        );
        assert_eq!(
            Edge::edge_id(RelationKind::Runs, &node, &guest),
            "e-f5a68bdecd893aa2"
        );

        // The claim that produced it: pve self-claims its own host_id and
        // names the guest by device slug and name.
        //   repr(from) = "h-3fa9c2d41b7e||||"
        //   repr(to)   = "|vm-101|||db01"
        let claim = RelationshipEvidence {
            sensor: "pve".into(),
            source: "pve01".into(),
            kind: RelationKind::Hosts,
            from: EndpointClaim {
                host_id: Some("h-3fa9c2d41b7e".into()),
                ..Default::default()
            },
            to: EndpointClaim {
                device: Some("vm-101".into()),
                name: Some("db01".into()),
                ..Default::default()
            },
            attrs: BTreeMap::new(),
            last_updated: 0,
        };
        assert_eq!(claim.relation_id(), "r-f5f9a2edb9601155");
        // Neither the sensor, the source, the attrs nor the clock reach the
        // id — the RFC requires that, and a vector cannot show it alone.
        let mut other = claim.clone();
        other.sensor = "container".into();
        other.source = "somewhere-else".into();
        other.last_updated = 1_700_000_000_000;
        other.attrs.insert("bridge".into(), "vmbr0".into());
        assert_eq!(other.relation_id(), claim.relation_id());
    }

    #[test]
    fn edge_id_is_stable_and_direction_sensitive() {
        let a = entity("h-0123456789ab");
        let b = entity("h-ba9876543210");
        let ab = Edge::edge_id(RelationKind::Hosts, &a, &b);
        assert_eq!(ab, Edge::edge_id(RelationKind::Hosts, &a, &b));
        // Direction is meaningful: a hypervisor hosting a guest is not the
        // guest hosting the hypervisor, and impact attribution walks one way.
        assert_ne!(ab, Edge::edge_id(RelationKind::Hosts, &b, &a));
        // So is the kind.
        assert_ne!(ab, Edge::edge_id(RelationKind::Runs, &a, &b));
        assert!(ab.starts_with("e-") && ab.len() == 18);
    }

    #[test]
    fn an_entity_and_an_external_at_the_same_string_are_different_edges() {
        // Without the variant prefix in `Endpoint::repr` these collide, and a
        // resolved host would silently share a key with the unresolved
        // endpoint it was resolved from.
        let a = entity("h-0123456789ab");
        let resolved = entity("10.0.0.1");
        let external = Endpoint::External {
            ip: Some("10.0.0.1".into()),
            mac: None,
            name: None,
        };
        assert_ne!(
            Edge::edge_id(RelationKind::GatewayOf, &resolved, &a),
            Edge::edge_id(RelationKind::GatewayOf, &external, &a)
        );
    }

    #[test]
    fn the_separator_cannot_be_forged_by_field_values() {
        // `-` as a separator would make these two triples identical.
        let x = Edge::edge_id(RelationKind::Runs, &entity("a-b"), &entity("c"));
        let y = Edge::edge_id(RelationKind::Runs, &entity("a"), &entity("b-c"));
        assert_ne!(x, y);
    }

    #[test]
    fn relation_id_ignores_arrival_order_and_refresh_time() {
        let mk = |ips: &[&str], ts: i64| RelationshipEvidence {
            sensor: "pve".into(),
            source: "node1".into(),
            kind: RelationKind::Hosts,
            from: EndpointClaim::host("h-0123456789ab"),
            to: EndpointClaim {
                device: Some("101".into()),
                ips: ips.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            attrs: BTreeMap::new(),
            last_updated: ts,
        };
        // Same observations, different order, different refresh: one key.
        assert_eq!(
            mk(&["10.0.0.2", "10.0.0.1"], 1_000).relation_id(),
            mk(&["10.0.0.1", "10.0.0.2"], 9_000).relation_id()
        );
        // A different guest is a different key.
        assert_ne!(
            mk(&["10.0.0.1"], 0).relation_id(),
            mk(&["10.0.0.9"], 0).relation_id()
        );
    }

    #[test]
    fn only_l2_adjacency_is_inert() {
        for k in [
            RelationKind::Hosts,
            RelationKind::Runs,
            RelationKind::GatewayOf,
            RelationKind::Probes,
        ] {
            assert!(k.is_containment(), "{k:?} must propagate impact");
        }
        assert!(!RelationKind::L2Adjacent.is_containment());
    }

    #[test]
    fn an_edge_canonicalizes_to_one_encoding() {
        let mk = |obs: Vec<(&str, &str)>| {
            let mut e = Edge {
                edge_id: "e-0000000000000000".into(),
                kind: RelationKind::Runs,
                from: entity("h-0123456789ab"),
                to: entity("h-ba9876543210"),
                attrs: BTreeMap::from([("unit".into(), "web.service".into())]),
                observers: obs
                    .into_iter()
                    .map(|(s, o)| Observer {
                        sensor: s.into(),
                        origin: o.into(),
                    })
                    .collect(),
                last_updated: 42,
            };
            e.canonicalize();
            serde_json::to_vec(&e).unwrap()
        };
        assert_eq!(
            mk(vec![("systemd", "h-b"), ("container", "h-a")]),
            mk(vec![("container", "h-a"), ("systemd", "h-b")])
        );
    }

    #[test]
    fn evidence_and_edge_round_trip_in_both_encodings() {
        let ev = RelationshipEvidence {
            sensor: "probe".into(),
            source: "vantage-1".into(),
            kind: RelationKind::Probes,
            from: EndpointClaim::host("h-0123456789ab"),
            to: EndpointClaim {
                name: Some("example.test".into()),
                ips: vec!["93.184.216.34".into()],
                ..Default::default()
            },
            attrs: BTreeMap::from([("kind".into(), "https".into())]),
            last_updated: 1_700_000_000_000,
        };
        let json = serde_json::to_vec(&ev).unwrap();
        assert_eq!(ev, serde_json::from_slice(&json).unwrap());
        let cbor = crate::serialization::encode(&ev, Format::Cbor).unwrap();
        assert_eq!(
            ev,
            crate::serialization::decode(&cbor, Format::Cbor).unwrap()
        );

        let edge = Edge {
            edge_id: Edge::edge_id(
                RelationKind::Probes,
                &entity("h-0123456789ab"),
                &Endpoint::External {
                    ip: Some("93.184.216.34".into()),
                    mac: None,
                    name: Some("example.test".into()),
                },
            ),
            kind: RelationKind::Probes,
            from: entity("h-0123456789ab"),
            to: Endpoint::External {
                ip: Some("93.184.216.34".into()),
                mac: None,
                name: Some("example.test".into()),
            },
            attrs: BTreeMap::new(),
            observers: vec![Observer {
                sensor: "probe".into(),
                origin: "h-0123456789ab".into(),
            }],
            last_updated: 1_700_000_000_000,
        };
        let json = serde_json::to_vec(&edge).unwrap();
        assert_eq!(edge, serde_json::from_slice(&json).unwrap());
        let cbor = crate::serialization::encode(&edge, Format::Cbor).unwrap();
        assert_eq!(
            edge,
            crate::serialization::decode(&cbor, Format::Cbor).unwrap()
        );
    }

    #[test]
    fn an_empty_claim_says_so() {
        assert!(EndpointClaim::default().is_empty());
        assert!(!EndpointClaim::host("h-0123456789ab").is_empty());
        assert!(!EndpointClaim::device("101").is_empty());
    }
}
