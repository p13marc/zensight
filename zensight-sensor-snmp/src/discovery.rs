//! Opt-in subnet auto-discovery (#541): sweep, identify, **propose**.
//!
//! Never runs without an explicit `snmp.discovery` block (off by default),
//! probes with bounded concurrency and fast-fail timeouts, and **never
//! auto-adds** devices: unconfigured responders are published as a
//! [`DiscoveryReport`] state doc (`state/snmp/discovery`) with sysObjectID/
//! sysName identification, the device profiles (#531) that would apply, and
//! a copy-pasteable config snippet. Already-configured devices — by their
//! configured address or any IP the pollers' identity evidence has seen —
//! are never re-proposed.
//!
//! Operational note (documented in reference.md): an SNMP sweep can trip
//! IDS in some environments; this stays opt-in, bounded, and paced.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use zensight_common::{DiscoveredDevice, DiscoveryReport};

use crate::config::CredentialSet;

/// Hard cap on addresses per sweep — a typo'd /8 must not become a scan.
pub const MAX_ADDRESSES: usize = 4096;

/// Discovery configuration (#541). The block's presence is the opt-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// IPv4 CIDRs to sweep (e.g. `"192.168.1.0/24"`). Combined size is
    /// capped at [`MAX_ADDRESSES`]; larger configs fail startup.
    pub subnets: Vec<String>,

    /// Named credential sets (#538) to try per address, in order.
    #[serde(default)]
    pub credentials: Vec<String>,

    /// Sweep cadence in seconds (default 3600).
    #[serde(default = "default_scan_interval")]
    pub interval_secs: u64,

    /// SNMP port to probe (default 161).
    #[serde(default = "default_port")]
    pub port: u16,

    /// Concurrent probes (default 8; also the pacing bound).
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,

    /// Per-probe timeout in seconds (default 1, no retries).
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout_secs: u64,
}

fn default_scan_interval() -> u64 {
    3600
}
fn default_port() -> u16 {
    161
}
fn default_concurrency() -> usize {
    8
}
fn default_probe_timeout() -> u64 {
    1
}

/// Expand an IPv4 CIDR into host addresses (network/broadcast skipped for
/// prefixes shorter than /31).
pub fn expand_cidr(cidr: &str) -> Result<Vec<Ipv4Addr>> {
    let (base, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow!("{cidr:?}: expected a.b.c.d/nn"))?;
    let base: Ipv4Addr = base.parse().map_err(|_| anyhow!("{cidr:?}: bad address"))?;
    let prefix: u32 = prefix
        .parse()
        .map_err(|_| anyhow!("{cidr:?}: bad prefix"))?;
    if prefix > 32 {
        bail!("{cidr:?}: prefix > 32");
    }
    let host_bits = 32 - prefix;
    let count: u64 = 1u64 << host_bits;
    if count as usize > MAX_ADDRESSES {
        bail!(
            "{cidr:?}: {count} addresses exceeds the {MAX_ADDRESSES} sweep cap (use a smaller prefix)"
        );
    }
    let network = u32::from(base) & (u32::MAX.checked_shl(host_bits).unwrap_or(0));
    let mut out = Vec::with_capacity(count as usize);
    for offset in 0..count as u32 {
        if host_bits >= 2 && (offset == 0 || u64::from(offset) == count - 1) {
            continue; // network / broadcast
        }
        out.push(Ipv4Addr::from(network + offset));
    }
    Ok(out)
}

/// The sweep driver.
pub struct Discovery {
    config: DiscoveryConfig,
    /// Resolved (name, set) pairs, tried in order.
    credentials: Vec<(String, CredentialSet)>,
    profiles: Option<Arc<crate::profile::ProfileSet>>,
    /// Fleet-known IPs: configured device addresses + every IP the pollers'
    /// identity evidence has observed. Never re-propose these.
    known_ips: Arc<std::sync::RwLock<HashSet<String>>>,
}

impl Discovery {
    pub fn new(
        config: DiscoveryConfig,
        credentials: Vec<(String, CredentialSet)>,
        known_ips: Arc<std::sync::RwLock<HashSet<String>>>,
    ) -> Self {
        Self {
            config,
            credentials,
            profiles: None,
            known_ips,
        }
    }

    pub fn with_profiles(&mut self, profiles: Arc<crate::profile::ProfileSet>) {
        self.profiles = Some(profiles);
    }

    /// Validate + expand the configured subnets (startup check).
    pub fn addresses(&self) -> Result<Vec<std::net::SocketAddr>> {
        let mut out = Vec::new();
        for cidr in &self.config.subnets {
            for ip in expand_cidr(cidr)? {
                out.push(std::net::SocketAddr::from((ip, self.config.port)));
            }
        }
        if out.len() > MAX_ADDRESSES {
            bail!(
                "discovery subnets expand to {} addresses (cap {MAX_ADDRESSES})",
                out.len()
            );
        }
        Ok(out)
    }

    /// One sweep over `addresses`: probe unknown ones, identify responders.
    pub async fn sweep(&self, addresses: &[std::net::SocketAddr]) -> DiscoveryReport {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(
            self.config.max_concurrency.max(1),
        ));
        let mut tasks = tokio::task::JoinSet::new();
        let mut scanned = 0u32;

        for addr in addresses {
            let ip = addr.ip().to_string();
            if self.known_ips.read().unwrap().contains(&ip) {
                continue; // configured (or evidence-known) — never re-propose
            }
            scanned += 1;
            let semaphore = semaphore.clone();
            let credentials = self.credentials.clone();
            let timeout = Duration::from_secs(self.config.probe_timeout_secs.max(1));
            let addr = *addr;
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.ok()?;
                probe(addr, &credentials, timeout).await
            });
        }

        let mut discovered = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(Some(mut device)) = result {
                self.identify(&mut device);
                discovered.push(device);
            }
        }
        discovered.sort_by(|a, b| a.address.cmp(&b.address));

        DiscoveryReport {
            timestamp: zensight_common::current_timestamp_millis(),
            scanned,
            discovered,
        }
    }

    /// Fill profile matches + the suggested config snippet.
    fn identify(&self, device: &mut DiscoveredDevice) {
        if let Some(profiles) = &self.profiles
            && let Ok(selection) = profiles.select(device.sys_object_id.as_deref(), None)
        {
            device.matched_profiles = selection.applied;
        }
        let name = device
            .sys_name
            .clone()
            .unwrap_or_else(|| device.address.replace([':', '.'], "-"));
        let credentials = device
            .credentials
            .as_ref()
            .map(|c| format!(", credentials: \"{c}\""))
            .unwrap_or_default();
        device.suggested = format!(
            "{{ name: \"{name}\", address: \"{}\"{credentials} }}",
            device.address
        );
    }
}

/// Probe one address with each credential set until something answers.
async fn probe(
    addr: std::net::SocketAddr,
    credentials: &[(String, CredentialSet)],
    timeout: Duration,
) -> Option<DiscoveredDevice> {
    // No configured sets: try the classic public community.
    let fallback = [(
        String::new(),
        CredentialSet {
            community: Some("public".to_string()),
            security: None,
        },
    )];
    let sets: &[(String, CredentialSet)] = if credentials.is_empty() {
        &fallback
    } else {
        credentials
    };

    for (set_name, set) in sets {
        let device = crate::config::DeviceConfig {
            name: "discovery-probe".to_string(),
            address: addr.to_string(),
            community: set
                .community
                .clone()
                .unwrap_or_else(|| "public".to_string()),
            version: if set.security.is_some() {
                crate::config::SnmpVersion::V3
            } else {
                crate::config::SnmpVersion::V2c
            },
            security: set.security.clone(),
            poll_interval_secs: 1,
            timeout_secs: timeout.as_secs().max(1),
            retries: 0,
            max_repetitions: 4,
            oids: Vec::new(),
            walks: Vec::new(),
            oid_group: None,
            alerts: None,
            profile: None,
            credentials: None,
        };
        let Ok(client) = crate::poller::build_probe_client(&device).await else {
            continue;
        };

        let get_text = |oid: &'static str| {
            let client = client.clone();
            async move {
                let oid = async_snmp::Oid::parse(oid).ok()?;
                match client.get(&oid).await.ok()?.value {
                    async_snmp::Value::OctetString(s) => String::from_utf8(s.to_vec()).ok(),
                    async_snmp::Value::ObjectIdentifier(o) => Some(o.to_string()),
                    _ => None,
                }
            }
        };

        // sysObjectID answers ⇒ it's an SNMP agent we can read.
        let Some(sys_object_id) = get_text("1.3.6.1.2.1.1.2.0").await else {
            continue;
        };
        let sys_name = get_text("1.3.6.1.2.1.1.5.0").await;
        let sys_descr = get_text("1.3.6.1.2.1.1.1.0").await;

        return Some(DiscoveredDevice {
            address: addr.to_string(),
            credentials: (!set_name.is_empty()).then(|| set_name.clone()),
            sys_object_id: Some(sys_object_id),
            sys_name,
            sys_descr,
            matched_profiles: Vec::new(),
            suggested: String::new(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_expansion_shapes() {
        assert_eq!(
            expand_cidr("10.0.0.5/32").unwrap(),
            vec![Ipv4Addr::new(10, 0, 0, 5)]
        );
        let two = expand_cidr("10.0.0.0/31").unwrap();
        assert_eq!(two.len(), 2);

        let net = expand_cidr("192.168.1.0/24").unwrap();
        assert_eq!(net.len(), 254); // network + broadcast skipped
        assert_eq!(net[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(*net.last().unwrap(), Ipv4Addr::new(192, 168, 1, 254));

        // Misaligned base normalizes onto the network.
        let net = expand_cidr("10.0.0.77/30").unwrap();
        assert_eq!(
            net,
            vec![Ipv4Addr::new(10, 0, 0, 77), Ipv4Addr::new(10, 0, 0, 78)]
        );
    }

    #[test]
    fn cidr_caps_and_errors() {
        assert!(
            expand_cidr("10.0.0.0/8").is_err(),
            "typo'd /8 must not scan"
        );
        assert!(expand_cidr("10.0.0.0/20").is_ok()); // 4094 ≤ cap
        assert!(expand_cidr("10.0.0.0/19").is_err()); // 8190 > cap
        assert!(expand_cidr("nonsense").is_err());
        assert!(expand_cidr("10.0.0.0/33").is_err());
    }
}
