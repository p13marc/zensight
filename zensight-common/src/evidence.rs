//! Host-evidence wire types — the `zensight/_meta/evidence/**` keyspace (#301).
//!
//! Sensors publish identity *evidence*; the correlator (single writer) merges it
//! into entities. Evidence is a claim, not a verdict: `observer: None` marks a
//! sensor's **self-report** about the host it runs on, `observer: Some(sensor)`
//! marks a **third-party claim** about a device observed on the wire (netring
//! assets, netlink neighbors, snmp sysName, ...) which merge rules weigh lower.
//!
//! Evidence freshness matters: consumers ignore records whose `last_updated` is
//! older than the evidence TTL, so publishers must periodically refresh live
//! claims (see `docs/KEYSPACE.md` for the TTL contract).

use serde::{Deserialize, Serialize};

/// One host-identity claim, published on
/// `zensight/_meta/evidence/host/<sensor>/<source>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostEvidence {
    /// Publishing sensor (e.g. `"sysinfo"`, `"netring"`).
    pub sensor: String,
    /// The `source` this claim is about — the same value used as `source` in
    /// telemetry keys for self-reports; an observed-device slug for third-party
    /// claims.
    pub source: String,
    /// `None` = self-report; `Some(sensor)` = third-party claim about an
    /// observed device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer: Option<String>,
    /// Hashed machine-id (`sha256(machine_id + salt)` hex) — never the raw id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Kernel boot id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<String>,
    /// Known IP addresses (identifying).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Known MAC addresses (merge *evidence*, not identity — VMs clone MACs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Hardware vendor, when observed (descriptive, display-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// OS/platform hint, when observed (descriptive, display-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Unix epoch millis of the latest refresh. Claims older than the evidence
    /// TTL are ignored by merge rules.
    pub last_updated: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_optional_field_defaults() {
        let ev = HostEvidence {
            sensor: "sysinfo".into(),
            source: "host1".into(),
            observer: None,
            host_id: Some("ab".repeat(32)),
            boot_id: None,
            hostname: Some("host1".into()),
            fqdn: None,
            ips: vec!["10.0.0.5".into()],
            macs: vec![],
            vendor: None,
            platform: None,
            last_updated: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&ev).unwrap();
        // Skipped-when-empty fields must not appear on the wire.
        assert!(!json.contains("observer"));
        assert!(!json.contains("macs"));
        let back: HostEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);

        // A minimal doc (old publisher / sparse claim) decodes with defaults.
        let sparse: HostEvidence = serde_json::from_str(
            r#"{"sensor":"netring","source":"aa-bb-cc-dd-ee-ff","last_updated":1}"#,
        )
        .unwrap();
        assert_eq!(sparse.observer, None);
        assert!(sparse.ips.is_empty());
    }
}
