//! Static host facts — the sysinfo sensor's `state/sysinfo/system/info` doc.
//!
//! One near-static document per host: os-release identity, kernel release,
//! architecture, hostname, boot time. Published LWW with a long TTL and a
//! cached publisher, so a late-joining GUI seeds the current doc instead of
//! waiting for the next slow refresh. Everything here is **descriptive** —
//! host identity/merging rides the evidence plane, never this doc.

use serde::{Deserialize, Serialize};

/// Static system information for the host the sysinfo sensor runs on.
///
/// Every field is optional and elided when absent: a minimal container has no
/// `/etc/os-release`, and absence is a fact worth distinguishing from an
/// empty string (the GUI's zero ≠ absent doctrine).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SystemInfo {
    /// os-release `PRETTY_NAME` (`"Fedora Linux 42 (Workstation Edition)"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_pretty_name: Option<String>,
    /// os-release `NAME` (`"Fedora Linux"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_name: Option<String>,
    /// os-release `ID` (`"fedora"`, `"debian"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_id: Option<String>,
    /// os-release `VERSION_ID` (`"42"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// os-release `VERSION_CODENAME` (`"bookworm"`), when the distro sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_codename: Option<String>,
    /// Kernel release (`uname -r` / `/proc/sys/kernel/osrelease`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// CPU architecture (`"x86_64"`, `"aarch64"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Local hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Boot time, Unix epoch **milliseconds**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time_ms: Option<i64>,
    /// Unix epoch millis this doc was (re)collected.
    pub timestamp: i64,
}

impl SystemInfo {
    /// The one-line display form for headers/cards: `PRETTY_NAME`, else
    /// `NAME VERSION_ID`, else `NAME`.
    pub fn display_name(&self) -> Option<String> {
        if let Some(pretty) = &self.os_pretty_name {
            return Some(pretty.clone());
        }
        match (&self.os_name, &self.os_version) {
            (Some(n), Some(v)) => Some(format!("{n} {v}")),
            (Some(n), None) => Some(n.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_absent_fields_stay_off_the_wire() {
        let doc = SystemInfo {
            os_pretty_name: Some("Fedora Linux 42 (Workstation Edition)".into()),
            os_id: Some("fedora".into()),
            kernel: Some("6.15.3-200.fc42.x86_64".into()),
            timestamp: 1_700_000_000_000,
            ..Default::default()
        };
        let json = serde_json::to_string(&doc).unwrap();
        assert!(!json.contains("os_codename"));
        assert!(!json.contains("boot_time_ms"));
        let back: SystemInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, doc);
        // A minimal/old doc decodes with defaults.
        let sparse: SystemInfo = serde_json::from_str(r#"{"timestamp":1}"#).unwrap();
        assert_eq!(sparse.os_pretty_name, None);
    }

    #[test]
    fn display_name_prefers_pretty_name_then_composes() {
        let mut doc = SystemInfo {
            os_name: Some("Debian GNU/Linux".into()),
            os_version: Some("12".into()),
            ..Default::default()
        };
        assert_eq!(doc.display_name().as_deref(), Some("Debian GNU/Linux 12"));
        doc.os_pretty_name = Some("Debian GNU/Linux 12 (bookworm)".into());
        assert_eq!(
            doc.display_name().as_deref(),
            Some("Debian GNU/Linux 12 (bookworm)")
        );
        assert_eq!(SystemInfo::default().display_name(), None);
    }
}
