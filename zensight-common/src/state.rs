//! ZenSight's state-subject refinement.
//!
//! `zenkey::CommonState` covers only the RFC-defined framework state set
//! (RFC 04 §1.2/§5, RFC 06 §4/§5); the app-specific state subjects —
//! parallax `stream/{stream}`, the per-producer `artifact/{kind}` family,
//! and catalog `assertion/{id}` — are ZenSight vocabulary, refined here over
//! the generated registry (RFC 08 §1).

use crate::registry::{self, AnySubject};

/// ZenSight's state-subject refinement: the RFC framework set plus the
/// app-specific ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZensightState<'a> {
    /// One of the RFC-defined framework state subjects.
    Common(zenkey::CommonState<'a>),
    /// Parallax `state/parallax/stream/{stream}` — live stream descriptor.
    Stream { stream: &'a str },
    /// Per-producer `state/<producer>/artifact/{kind}` — artifact kind advert.
    Artifact { kind: &'a str },
    /// Catalog `state/assertion/{id}` — operator identity assertion.
    CatalogAssertion { id: &'a str },
    /// Catalog `state/edge/{edge_id}` — a resolved relationship (#915).
    ///
    /// Refined here rather than through `zenkey::CommonState` for the same
    /// reason as [`ZensightState::CatalogAssertion`]: `CommonState` is a
    /// closed RFC enum in an external crate, so a new variant costs a zenkey
    /// release. If the RFC decides the family is genuinely cross-producer
    /// (zenkey#416), this moves; until then the code and the RFC disagree, on
    /// purpose and in writing.
    CatalogEdge { edge_id: &'a str },
    /// Per-sensor `state/<producer>/evidence/relation/{relation_id}` — one
    /// sensor's claim that two things are related (#915).
    EvidenceRelation { relation_id: &'a str },
    /// SNMP `state/snmp/{device}/interfaces` — joined interface table (#529).
    SnmpInterfaces { device: &'a str },
    /// SNMP `state/snmp/discovery` — subnet-discovery report (#541/#579).
    SnmpDiscovery,
}

impl<'a> ZensightState<'a> {
    /// The state subject this registry-refined subject represents, if any.
    pub fn of(subject: &'a AnySubject) -> Option<Self> {
        if let Some(common) = subject.common_state() {
            return Some(ZensightState::Common(common));
        }
        match subject {
            AnySubject::Parallax(registry::parallax::Subject::Stream { stream }) => {
                Some(ZensightState::Stream { stream })
            }
            AnySubject::Catalog(registry::catalog::Subject::Assertion { id }) => {
                Some(ZensightState::CatalogAssertion { id })
            }
            AnySubject::Catalog(registry::catalog::Subject::Edge { edge_id }) => {
                Some(ZensightState::CatalogEdge { edge_id })
            }
            AnySubject::Container(registry::container::Subject::EvidenceRelation {
                relation_id,
            })
            | AnySubject::Netlink(registry::netlink::Subject::EvidenceRelation { relation_id })
            | AnySubject::Probe(registry::probe::Subject::EvidenceRelation { relation_id })
            | AnySubject::Pve(registry::pve::Subject::EvidenceRelation { relation_id }) => {
                Some(ZensightState::EvidenceRelation { relation_id })
            }
            AnySubject::Snmp(registry::snmp::Subject::Interfaces { device }) => {
                Some(ZensightState::SnmpInterfaces { device })
            }
            AnySubject::Snmp(registry::snmp::Subject::Discovery) => {
                Some(ZensightState::SnmpDiscovery)
            }
            AnySubject::Gnmi(registry::gnmi::Subject::Artifact { kind })
            | AnySubject::Logs(registry::logs::Subject::Artifact { kind })
            | AnySubject::Modbus(registry::modbus::Subject::Artifact { kind })
            | AnySubject::Netflow(registry::netflow::Subject::Artifact { kind })
            | AnySubject::Netlink(registry::netlink::Subject::Artifact { kind })
            | AnySubject::Netring(registry::netring::Subject::Artifact { kind })
            | AnySubject::Parallax(registry::parallax::Subject::Artifact { kind })
            | AnySubject::Snmp(registry::snmp::Subject::Artifact { kind })
            | AnySubject::Sysinfo(registry::sysinfo::Subject::Artifact { kind })
            | AnySubject::Systemd(registry::systemd::Subject::Artifact { kind }) => {
                Some(ZensightState::Artifact { kind })
            }
            _ => None,
        }
    }
}
