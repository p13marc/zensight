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
    /// SNMP `state/snmp/{device}/interfaces` — joined interface table (#529).
    SnmpInterfaces { device: &'a str },
    /// Sysinfo `state/sysinfo/system/info` — static host facts.
    SysinfoSystemInfo,
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
            AnySubject::Snmp(registry::snmp::Subject::Interfaces { device }) => {
                Some(ZensightState::SnmpInterfaces { device })
            }
            AnySubject::Sysinfo(registry::sysinfo::Subject::SystemInfo) => {
                Some(ZensightState::SysinfoSystemInfo)
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
