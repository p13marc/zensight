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

/// Cloud-provider identity facts from an instance-metadata service (#311).
///
/// `(provider, instance_id)` is **authoritative** for the correlator: cloned
/// images/snapshots duplicate machine-ids, but a cloud control plane never
/// hands out the same instance id twice, so this pair merges evidence almost
/// as strongly as `host_id`. `region`/`account` are descriptive extras.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudFacts {
    /// Provider slug: `"aws"`, `"gcp"`, `"azure"`.
    pub provider: String,
    /// Provider-scoped instance id (EC2 instance-id, GCE numeric id, Azure vmId).
    pub instance_id: String,
    /// Region/location, when the metadata service reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Account/project/subscription id, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

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
    /// Container the *reporting sensor process* runs in (#311) — a host-scoped
    /// qualifier ("this sensor's view is from inside container X"), **never** a
    /// cross-host merge key: container ids are only unique per host runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    /// Cloud-provider identity from the instance-metadata service, when the
    /// opt-in IMDS probe found one (#311). Authoritative for merging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudFacts>,
    /// Unix epoch millis of the latest refresh. Claims older than the evidence
    /// TTL are ignored by merge rules.
    pub last_updated: i64,
}

/// One passive-DNS name observation, published on
/// `zensight/_meta/evidence/names/<sensor>/<ip-slug>` (#307).
///
/// A **third-party** claim binding an observed IP to a name seen on the wire
/// (DNS answer, PTR, TLS SNI, ...). The correlator uses these to attach
/// hostnames/FQDNs to entities that emit no telemetry of their own. Like
/// [`HostEvidence`], stale records (past the evidence TTL) are ignored, so the
/// publisher refreshes live bindings periodically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameObservation {
    /// Observing sensor (e.g. `"netring"`).
    pub observer: String,
    /// The observed IP address this name is bound to.
    pub ip: String,
    /// The canonical name (lowercased, no trailing dot).
    pub name: String,
    /// Provenance slug from the passive-DNS source (`dns_a`, `dns_cname`,
    /// `dns_ptr`, `sni`, `mdns`, ...).
    pub provenance: String,
    /// Unix epoch millis of the most recent sighting.
    pub last_seen: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_observation_roundtrip() {
        let obs = NameObservation {
            observer: "netring".into(),
            ip: "10.0.0.9".into(),
            name: "printer.example.com".into(),
            provenance: "dns_a".into(),
            last_seen: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&obs).unwrap();
        let back: NameObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, obs);
    }

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
            container_id: None,
            cloud: None,
            last_updated: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&ev).unwrap();
        // Skipped-when-empty fields must not appear on the wire.
        assert!(!json.contains("observer"));
        assert!(!json.contains("macs"));
        assert!(!json.contains("container_id"));
        assert!(!json.contains("cloud"));
        let back: HostEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);

        // A minimal doc (old publisher / sparse claim) decodes with defaults.
        let sparse: HostEvidence = serde_json::from_str(
            r#"{"sensor":"netring","source":"aa-bb-cc-dd-ee-ff","last_updated":1}"#,
        )
        .unwrap();
        assert_eq!(sparse.observer, None);
        assert!(sparse.ips.is_empty());
        assert_eq!(sparse.container_id, None);
        assert_eq!(sparse.cloud, None);
    }

    #[test]
    fn container_and_cloud_fields_roundtrip() {
        // Additive #311 fields survive a wire round-trip, and CloudFacts skips
        // its own empty optionals.
        let ev = HostEvidence {
            sensor: "sysinfo".into(),
            source: "host1".into(),
            observer: None,
            host_id: Some("ab".repeat(32)),
            boot_id: None,
            hostname: None,
            fqdn: None,
            ips: vec![],
            macs: vec![],
            vendor: None,
            platform: None,
            container_id: Some("f".repeat(64)),
            cloud: Some(CloudFacts {
                provider: "aws".into(),
                instance_id: "i-0abc123def456".into(),
                region: Some("eu-west-1".into()),
                account: None,
            }),
            last_updated: 1,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("container_id"));
        assert!(json.contains(r#""provider":"aws""#));
        assert!(!json.contains("account"));
        let back: HostEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn cloud_facts_decode_from_minimal_doc() {
        // Only provider + instance_id are required on the wire.
        let facts: CloudFacts =
            serde_json::from_str(r#"{"provider":"gcp","instance_id":"12345"}"#).unwrap();
        assert_eq!(facts.region, None);
        assert_eq!(facts.account, None);
    }
}
