//! Lateral-movement classification (#123).
//!
//! Pure decision logic over primitives extracted from flowscope's SMB / RDP /
//! Kerberos parsers (the parsers themselves are pulled in only under the
//! `lateral` cargo feature; this module has no flowscope dependency, so the
//! detection rules stay unit-testable on the default build). The monitor's L7
//! handlers extract the relevant fields and call these; a returned
//! [`LateralFinding`] becomes a `zensight` alert via `map::lateral_view`.
//!
//! Each rule targets a high-signal MITRE ATT&CK technique:
//! - **SMB** to an admin share (`C$`/`ADMIN$`) or `IPC$` service pipe → T1021.002
//! - **RDP** connection request between peers → T1021.001
//! - **Kerberos** kerberoasting / weak-etype / brute-force signals → T1558

use zensight_common::alert::AlertSeverity;

/// A classified lateral-movement event. `kind` is the detector slug consumed by
/// `map::lateral_view` / `attack_technique`.
#[derive(Debug, Clone, PartialEq)]
pub struct LateralFinding {
    pub kind: &'static str,
    pub severity: AlertSeverity,
    pub observations: Vec<(String, String)>,
}

/// Classify an SMB tree-connect / create as lateral movement (#123, T1021.002).
///
/// Fires on access to a Windows **administrative share** (`tree_is_admin_share`,
/// e.g. `\\host\C$` / `ADMIN$`) or an `IPC$` service-control **named pipe**
/// (`create_is_admin_pipe`, the PsExec/SCM pattern). Returns the share path and
/// the authenticating NTLM user when the parser recovered them.
pub fn smb_finding(
    tree_is_admin_share: bool,
    create_is_admin_pipe: bool,
    tree_path: Option<&str>,
    create_path: Option<&str>,
    ntlm_user: Option<&str>,
) -> Option<LateralFinding> {
    if !tree_is_admin_share && !create_is_admin_pipe {
        return None;
    }
    let mut observations = Vec::new();
    if let Some(p) = tree_path.filter(|s| !s.is_empty()) {
        observations.push(("share".to_string(), p.to_string()));
    }
    if create_is_admin_pipe && let Some(p) = create_path.filter(|s| !s.is_empty()) {
        observations.push(("pipe".to_string(), p.to_string()));
    }
    if let Some(u) = ntlm_user.filter(|s| !s.is_empty()) {
        observations.push(("user".to_string(), u.to_string()));
    }
    Some(LateralFinding {
        kind: "LateralSmb",
        severity: AlertSeverity::Warning,
        observations,
    })
}

/// Classify an RDP connection request as lateral movement (#123, T1021.001).
///
/// A `ConnectionRequest` between internal peers is the signal; Info severity so
/// it's allowlist-friendly in environments with legitimate east-west RDP. The
/// `mstshash=` cookie (target username), when present, is attached for triage.
pub fn rdp_finding(cookie_user: Option<&str>) -> Option<LateralFinding> {
    let mut observations = Vec::new();
    if let Some(u) = cookie_user.filter(|s| !s.is_empty()) {
        observations.push(("cookie_user".to_string(), u.to_string()));
    }
    Some(LateralFinding {
        kind: "LateralRdp",
        severity: AlertSeverity::Info,
        observations,
    })
}

/// Classify a Kerberos message for credential-access / lateral signals (#123,
/// T1558). Fires on a kerberoasting-suspect TGS request (RC4 service ticket),
/// an explicit brute-force error code, or a weak encryption type. Returns the
/// strongest signal present (kerberoast > brute-force > weak-etype).
pub fn kerberos_finding(
    kerberoast_suspect: bool,
    brute_force_error: bool,
    weak_etype: bool,
    realm: Option<&str>,
    sname: Option<&str>,
) -> Option<LateralFinding> {
    let (signal, severity): (&str, AlertSeverity) = if kerberoast_suspect {
        ("kerberoast", AlertSeverity::Warning)
    } else if brute_force_error {
        ("brute_force", AlertSeverity::Warning)
    } else if weak_etype {
        ("weak_etype", AlertSeverity::Info)
    } else {
        return None;
    };
    let mut observations = vec![("signal".to_string(), signal.to_string())];
    if let Some(r) = realm.filter(|s| !s.is_empty()) {
        observations.push(("realm".to_string(), r.to_string()));
    }
    if let Some(s) = sname.filter(|s| !s.is_empty()) {
        observations.push(("service".to_string(), s.to_string()));
    }
    Some(LateralFinding {
        kind: "LateralKerberos",
        severity,
        observations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smb_admin_share_fires_with_detail() {
        let f = smb_finding(true, false, Some("\\\\dc1\\C$"), None, Some("ACME\\admin")).unwrap();
        assert_eq!(f.kind, "LateralSmb");
        assert_eq!(f.severity, AlertSeverity::Warning);
        assert!(
            f.observations
                .contains(&("share".to_string(), "\\\\dc1\\C$".to_string()))
        );
        assert!(
            f.observations
                .contains(&("user".to_string(), "ACME\\admin".to_string()))
        );
    }

    #[test]
    fn smb_ipc_service_pipe_fires() {
        let f = smb_finding(false, true, None, Some("\\PIPE\\svcctl"), None).unwrap();
        assert_eq!(f.kind, "LateralSmb");
        assert!(
            f.observations
                .contains(&("pipe".to_string(), "\\PIPE\\svcctl".to_string()))
        );
    }

    #[test]
    fn smb_ordinary_share_is_quiet() {
        assert!(smb_finding(false, false, Some("\\\\fs\\Public"), None, None).is_none());
    }

    #[test]
    fn rdp_request_fires_info_with_cookie() {
        let f = rdp_finding(Some("administrator")).unwrap();
        assert_eq!(f.kind, "LateralRdp");
        assert_eq!(f.severity, AlertSeverity::Info);
        assert!(
            f.observations
                .contains(&("cookie_user".to_string(), "administrator".to_string()))
        );
        // No cookie still fires (the request itself is the signal).
        assert!(rdp_finding(None).is_some());
    }

    #[test]
    fn kerberos_signal_priority_and_quiet_path() {
        // Kerberoast outranks the others and is a Warning.
        let f =
            kerberos_finding(true, true, true, Some("ACME.LOCAL"), Some("MSSQLSvc/db")).unwrap();
        assert_eq!(f.severity, AlertSeverity::Warning);
        assert_eq!(
            f.observations[0],
            ("signal".to_string(), "kerberoast".to_string())
        );
        // Weak etype alone is Info.
        let w = kerberos_finding(false, false, true, None, None).unwrap();
        assert_eq!(w.severity, AlertSeverity::Info);
        assert_eq!(
            w.observations[0],
            ("signal".to_string(), "weak_etype".to_string())
        );
        // Nothing suspicious → no finding.
        assert!(kerberos_finding(false, false, false, None, None).is_none());
    }
}
