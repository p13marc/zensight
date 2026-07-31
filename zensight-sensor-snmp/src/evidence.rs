//! Observed-device identity evidence (#537).
//!
//! Polled devices finally reach the correlator's entity catalog:
//! `zensight-common`'s evidence model names SNMP as the canonical
//! third-party observer, and this module builds one [`HostEvidence`] claim
//! per polled device from what each cycle already reads —
//!
//! - `hostname` ← sysName,
//! - `platform` ← sysDescr, `vendor` ← the sysObjectID enterprise arc,
//! - `macs` ← ifPhysAddress column (empty/all-zero filtered),
//! - `ips` ← ipAdEntAddr walk (the generic-device profile carries it) plus
//!   the address we poll —
//!
//! published as `observer = Some("snmp")` claims on
//! `state/snmp/evidence/device/<device>` with Evidence QoS. The correlator
//! merges them with netring/netlink observations of the same MAC/IP into
//! one `HostEntity`. `host_id` stays `None`, the netlink observed-device
//! precedent: a synthetic hash adds no merge power (nothing else would
//! carry it) and would masquerade as the hashed-machine-id contract.

use zensight_common::HostEvidence;

/// Well-known IANA enterprise numbers → vendor names (display-only hint).
/// Everything else renders as `enterprise-<n>`.
fn enterprise_vendor(number: u64) -> String {
    match number {
        2 => "ibm",
        9 => "cisco",
        11 => "hp",
        43 => "3com",
        171 => "d-link",
        890 => "zyxel",
        1588 => "brocade",
        1916 => "extreme",
        2011 => "huawei",
        2636 => "juniper",
        4526 => "netgear",
        6027 => "force10",
        8072 => "net-snmp",
        10002 => "ubiquiti",
        14988 => "mikrotik",
        25461 => "palo-alto",
        30065 => "arista",
        _ => return format!("enterprise-{number}"),
    }
    .to_string()
}

/// Accumulates identity observations over one poll cycle.
#[derive(Default)]
pub struct EvidenceCollector {
    hostname: Option<String>,
    platform: Option<String>,
    vendor: Option<String>,
    macs: Vec<String>,
    ips: Vec<String>,
}

impl EvidenceCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest one polled value; only identity-bearing OIDs are read.
    pub fn ingest(&mut self, oid: &str, value: &async_snmp::Value) {
        use async_snmp::Value;
        match (oid, value) {
            // sysName.0
            ("1.3.6.1.2.1.1.5.0", Value::OctetString(s)) => {
                self.hostname = String::from_utf8(s.to_vec()).ok().filter(|h| !h.is_empty());
            }
            // sysDescr.0 — platform hint (first line, truncated).
            ("1.3.6.1.2.1.1.1.0", Value::OctetString(s)) => {
                self.platform = String::from_utf8(s.to_vec()).ok().map(|d| {
                    let first = d.lines().next().unwrap_or_default();
                    first.chars().take(120).collect()
                });
            }
            // sysObjectID.0 — vendor from the enterprise arc.
            ("1.3.6.1.2.1.1.2.0", Value::ObjectIdentifier(o)) => {
                let oid_str = o.to_string();
                self.vendor = oid_str
                    .strip_prefix("1.3.6.1.4.1.")
                    .and_then(|rest| rest.split('.').next())
                    .and_then(|n| n.parse::<u64>().ok())
                    .map(enterprise_vendor);
            }
            // ifPhysAddress column (empty/all-zero filtered).
            (_, Value::OctetString(s))
                if oid.starts_with("1.3.6.1.2.1.2.2.1.6.")
                    && s.len() == 6
                    && s.iter().any(|b| *b != 0) =>
            {
                let mac = s
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":");
                if !self.macs.contains(&mac) {
                    self.macs.push(mac);
                }
            }
            // ipAdEntAddr column.
            (_, Value::IpAddress(ip)) if oid.starts_with("1.3.6.1.2.1.4.20.1.1.") => {
                let ip = format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
                self.push_ip(ip);
            }
            _ => {}
        }
    }

    /// Record the address the poller targets (host part of `ip:port`).
    pub fn note_polled_address(&mut self, address: &str) {
        if let Some(host) = address.rsplit_once(':').map(|(h, _)| h) {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            if host.parse::<std::net::IpAddr>().is_ok() {
                self.push_ip(host.to_string());
            }
        }
    }

    fn push_ip(&mut self, ip: String) {
        if ip == "127.0.0.1" || ip == "::1" || ip == "0.0.0.0" {
            return;
        }
        if !self.ips.contains(&ip) {
            self.ips.push(ip);
        }
    }

    /// Whether this cycle saw anything identity-bearing at all.
    pub fn is_empty(&self) -> bool {
        self.hostname.is_none() && self.macs.is_empty() && self.ips.is_empty()
    }

    /// Build the claim for `device`.
    pub fn build(mut self, device: &str) -> HostEvidence {
        self.macs.sort();
        self.ips.sort();
        HostEvidence {
            sensor: "snmp".to_string(),
            source: device.to_string(),
            observer: Some("snmp".to_string()),
            host_id: None,
            boot_id: None,
            hostname: self.hostname,
            fqdn: None,
            ips: self.ips,
            macs: self.macs,
            vendor: self.vendor,
            platform: self.platform,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            container_id: None,
            cloud: None,
            last_updated: zensight_common::current_timestamp_millis(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_snmp::Value;
    use bytes::Bytes;

    #[test]
    fn collects_identity_from_walked_values() {
        let mut c = EvidenceCollector::new();
        c.ingest(
            "1.3.6.1.2.1.1.5.0",
            &Value::OctetString(Bytes::from_static(b"core-sw-1")),
        );
        c.ingest(
            "1.3.6.1.2.1.1.1.0",
            &Value::OctetString(Bytes::from_static(b"Cisco IOS Software, C2960\nmore")),
        );
        c.ingest(
            "1.3.6.1.2.1.1.2.0",
            &Value::ObjectIdentifier("1.3.6.1.4.1.9.1.716".parse().unwrap()),
        );
        c.ingest(
            "1.3.6.1.2.1.2.2.1.6.1",
            &Value::OctetString(Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x01])),
        );
        // Empty and all-zero MACs are filtered.
        c.ingest("1.3.6.1.2.1.2.2.1.6.2", &Value::OctetString(Bytes::new()));
        c.ingest(
            "1.3.6.1.2.1.2.2.1.6.3",
            &Value::OctetString(Bytes::from_static(&[0; 6])),
        );
        c.ingest(
            "1.3.6.1.2.1.4.20.1.1.10.0.0.5",
            &Value::IpAddress([10, 0, 0, 5]),
        );
        c.note_polled_address("192.168.1.1:161");
        c.note_polled_address("router.example.com:161"); // hostname → skipped

        let ev = c.build("router01");
        assert_eq!(ev.sensor, "snmp");
        assert_eq!(ev.observer.as_deref(), Some("snmp"));
        assert_eq!(ev.host_id, None);
        assert_eq!(ev.hostname.as_deref(), Some("core-sw-1"));
        assert_eq!(ev.vendor.as_deref(), Some("cisco"));
        assert_eq!(ev.platform.as_deref(), Some("Cisco IOS Software, C2960"));
        assert_eq!(ev.macs, vec!["de:ad:be:ef:00:01"]);
        assert_eq!(ev.ips, vec!["10.0.0.5", "192.168.1.1"]);
    }

    #[test]
    fn unknown_enterprise_renders_number() {
        assert_eq!(enterprise_vendor(9), "cisco");
        assert_eq!(enterprise_vendor(424242), "enterprise-424242");
    }

    #[test]
    fn loopback_and_duplicates_filtered() {
        let mut c = EvidenceCollector::new();
        c.note_polled_address("127.0.0.1:161");
        c.ingest(
            "1.3.6.1.2.1.4.20.1.1.10.0.0.5",
            &Value::IpAddress([10, 0, 0, 5]),
        );
        c.ingest(
            "1.3.6.1.2.1.4.20.1.1.10.0.0.5",
            &Value::IpAddress([10, 0, 0, 5]),
        );
        let ev = c.build("d");
        assert_eq!(ev.ips, vec!["10.0.0.5"]);
    }
}
