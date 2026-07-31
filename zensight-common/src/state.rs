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

#[cfg(test)]
mod tests {
    use super::*;

    /// The full wire key must refine to the SystemInfo variant — pins the
    /// subject-parse precedence against the `system/*` telemetry family
    /// (`system/uptime`, `system/load`, …) sharing the same head chunk.
    #[test]
    fn system_info_key_refines_to_its_variant() {
        let (_, protocol, subject) =
            crate::keyexpr::refine_key("v1/h-3fa9c2d41b7e/state/sysinfo/system/info").unwrap();
        assert_eq!(protocol, "sysinfo");
        assert_eq!(
            ZensightState::of(&subject),
            Some(ZensightState::SysinfoSystemInfo)
        );
    }
}
