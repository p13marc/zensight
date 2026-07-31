//! Observer evidence for remote syslog senders (#552).
//!
//! A central collector ingesting syslog from many devices publishes a
//! `HostEvidence { observer: Some("logs"), … }` claim per remote sender on
//! `state/logs/evidence/device/<host>` (the same key the SNMP observer uses,
//! #537), so those devices reach the correlator's entity catalog and fuse with
//! SNMP / netring observations of the same gear.
//!
//! Names come from the message header / `hostname_aliases` / mTLS peer CN — no
//! DNS on the intake path. Optional reverse-DNS enrichment (off by default) runs
//! only in the periodic publish tick, cached, so it never blocks ingestion.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

use zensight_common::HostEvidence;

use crate::config::EvidenceConfig;
use crate::receiver::{MessageSource, ReceivedMessage};

/// Accumulated identity facts for one remote sender.
#[derive(Debug, Default, Clone)]
struct SenderInfo {
    hostname: Option<String>,
    ips: BTreeSet<String>,
    peer_cn: Option<String>,
    last_seen_ms: i64,
}

/// Tracks remote senders and publishes observer [`HostEvidence`] for them.
pub struct EvidenceTracker {
    cfg: EvidenceConfig,
    /// sender key (resolved hostname, else IP) → facts.
    senders: Mutex<HashMap<String, SenderInfo>>,
    /// Reverse-DNS cache: IP → (FQDN or None), refreshed lazily in the tick.
    rdns_cache: Mutex<HashMap<IpAddr, Option<String>>>,
}

impl EvidenceTracker {
    pub fn new(cfg: EvidenceConfig) -> Self {
        Self {
            cfg,
            senders: Mutex::new(HashMap::new()),
            rdns_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Record a message's sender. Only network/TLS peers are "observed devices";
    /// unix / journald / file lines are the collector's own. Pure map update —
    /// no I/O — safe to call on the hot intake path.
    pub fn observe(&self, received: &ReceivedMessage, now_ms: i64) {
        let MessageSource::Network(addr) = &received.source else {
            return;
        };
        let ip = addr.ip().to_string();
        // Prefer a real hostname (resolved alias / message header) over the bare
        // IP for the sender key; fall back to the IP.
        let host = &received.resolved_hostname;
        let hostname = (!host.is_empty() && host != &ip).then(|| host.clone());
        let key = hostname.clone().unwrap_or_else(|| ip.clone());
        let cn = received
            .message
            .structured_data
            .get("tls")
            .and_then(|m| m.get("peer_cn"))
            .cloned();

        let mut senders = self.senders.lock().unwrap();
        // Bounded cardinality: once full, refresh known senders but don't admit
        // new ones (avoids an unbounded key space from spoofed sources).
        if !senders.contains_key(&key) && senders.len() >= self.cfg.max_senders {
            return;
        }
        let info = senders.entry(key).or_default();
        if hostname.is_some() {
            info.hostname = hostname;
        }
        info.ips.insert(ip);
        if cn.is_some() {
            info.peer_cn = cn;
        }
        info.last_seen_ms = now_ms;
    }

    /// Build the current evidence claims (dropping senders silent past the expiry
    /// window) and, when enabled, enrich each with a cached reverse-DNS FQDN.
    /// Returns `(key, claim)` pairs to publish. Blocking (reverse-DNS) — call via
    /// `spawn_blocking`.
    pub fn build_claims(&self, now_ms: i64) -> Vec<(String, HostEvidence)> {
        let expire_ms = (self.cfg.expire_secs as i64).saturating_mul(1000);
        let active: Vec<(String, SenderInfo)> = {
            let mut senders = self.senders.lock().unwrap();
            senders.retain(|_, s| now_ms - s.last_seen_ms <= expire_ms);
            senders
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        active
            .into_iter()
            .map(|(key, info)| {
                let fqdn = if self.cfg.reverse_dns {
                    info.ips
                        .iter()
                        .find_map(|ip| ip.parse::<IpAddr>().ok())
                        .and_then(|ip| self.reverse_dns(ip))
                } else {
                    None
                };
                let claim = HostEvidence {
                    sensor: "logs".to_string(),
                    source: key.clone(),
                    observer: Some("logs".to_string()),
                    host_id: None,
                    boot_id: None,
                    // Prefer the mTLS-authenticated CN as the hostname when present.
                    hostname: info.peer_cn.clone().or(info.hostname),
                    fqdn,
                    ips: info.ips.into_iter().collect(),
                    macs: Vec::new(),
                    vendor: None,
                    platform: None,
                    os_name: None,
                    os_version: None,
                    kernel: None,
                    arch: None,
                    container_id: None,
                    cloud: None,
                    last_updated: now_ms,
                };
                (key, claim)
            })
            .collect()
    }

    /// Cached reverse-DNS (PTR) via the system resolver. First lookup per IP is a
    /// blocking `getnameinfo`; the result (including "no name") is cached, so a
    /// sender is looked up at most once per process lifetime.
    fn reverse_dns(&self, ip: IpAddr) -> Option<String> {
        if let Some(cached) = self.rdns_cache.lock().unwrap().get(&ip) {
            return cached.clone();
        }
        let resolved = reverse_lookup(ip);
        self.rdns_cache.lock().unwrap().insert(ip, resolved.clone());
        resolved
    }
}

/// Reverse-DNS (PTR) for `ip` via libc `getnameinfo` with `NI_NAMEREQD` (so a
/// missing PTR yields `None` rather than the numeric string). Returns the FQDN
/// lowercased. Blocking.
fn reverse_lookup(ip: IpAddr) -> Option<String> {
    let sa = SocketAddr::new(ip, 0);
    let mut host = [0i8; libc::NI_MAXHOST as usize];
    // Build the sockaddr storage for the address family.
    let (sockaddr_ptr, socklen): (*const libc::sockaddr, libc::socklen_t) = match sa {
        SocketAddr::V4(v4) => {
            let raw = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            let boxed = Box::new(raw);
            (
                Box::into_raw(boxed) as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let raw = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: 0,
            };
            let boxed = Box::new(raw);
            (
                Box::into_raw(boxed) as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    };

    // SAFETY: `sockaddr_ptr` points at a valid, correctly-sized sockaddr for the
    // family; `host` is a writable NI_MAXHOST buffer. We reclaim the Box after.
    let rc = unsafe {
        libc::getnameinfo(
            sockaddr_ptr,
            socklen,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    // Reclaim the leaked sockaddr box.
    unsafe {
        match sa {
            SocketAddr::V4(_) => drop(Box::from_raw(sockaddr_ptr as *mut libc::sockaddr_in)),
            SocketAddr::V6(_) => drop(Box::from_raw(sockaddr_ptr as *mut libc::sockaddr_in6)),
        }
    }
    if rc != 0 {
        return None;
    }
    // SAFETY: on success getnameinfo NUL-terminated `host`.
    let cstr = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) };
    cstr.to_str()
        .ok()
        .map(|s| s.trim_end_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn net_msg(ip: &str, host: &str, cn: Option<&str>) -> ReceivedMessage {
        let mut message = parse("<34>Oct 11 22:14:15 h app: x").unwrap();
        if let Some(cn) = cn {
            message
                .structured_data
                .entry("tls".to_string())
                .or_default()
                .insert("peer_cn".to_string(), cn.to_string());
        }
        ReceivedMessage {
            message,
            source: MessageSource::Network(format!("{ip}:5000").parse().unwrap()),
            resolved_hostname: host.to_string(),
        }
    }

    fn cfg() -> EvidenceConfig {
        EvidenceConfig {
            enabled: true,
            refresh_secs: 300,
            expire_secs: 3600,
            max_senders: 4,
            reverse_dns: false,
        }
    }

    #[test]
    fn observes_network_senders_as_observer_claims() {
        let t = EvidenceTracker::new(cfg());
        t.observe(&net_msg("10.0.0.5", "web01", None), 1_000);
        let claims = t.build_claims(2_000);
        assert_eq!(claims.len(), 1);
        let (key, claim) = &claims[0];
        assert_eq!(key, "web01");
        assert_eq!(claim.observer.as_deref(), Some("logs"));
        assert_eq!(claim.sensor, "logs");
        assert_eq!(claim.hostname.as_deref(), Some("web01"));
        assert_eq!(claim.ips, vec!["10.0.0.5".to_string()]);
    }

    #[test]
    fn mtls_cn_wins_as_hostname() {
        let t = EvidenceTracker::new(cfg());
        t.observe(
            &net_msg("10.0.0.6", "10.0.0.6", Some("edge.example.com")),
            1_000,
        );
        let claims = t.build_claims(2_000);
        assert_eq!(claims[0].1.hostname.as_deref(), Some("edge.example.com"));
    }

    #[test]
    fn silent_senders_expire() {
        let t = EvidenceTracker::new(cfg());
        t.observe(&net_msg("10.0.0.5", "web01", None), 1_000);
        // expire_secs=3600 → 3.6M ms; a claim 4M ms later is dropped.
        assert!(t.build_claims(1_000 + 4_000_000).is_empty());
    }

    #[test]
    fn cardinality_is_bounded() {
        let t = EvidenceTracker::new(cfg()); // max_senders = 4
        for i in 0..10 {
            t.observe(
                &net_msg(&format!("10.0.0.{i}"), &format!("h{i}"), None),
                1_000,
            );
        }
        assert!(t.build_claims(2_000).len() <= 4);
    }

    #[test]
    fn local_sources_are_ignored() {
        let t = EvidenceTracker::new(cfg());
        let mut m = net_msg("10.0.0.5", "web01", None);
        m.source = MessageSource::Unix;
        t.observe(&m, 1_000);
        assert!(
            t.build_claims(2_000).is_empty(),
            "unix sender is not observed"
        );
    }

    #[test]
    fn reverse_dns_off_yields_no_fqdn() {
        let t = EvidenceTracker::new(cfg()); // reverse_dns: false
        t.observe(&net_msg("10.0.0.5", "web01", None), 1_000);
        assert_eq!(t.build_claims(2_000)[0].1.fqdn, None);
    }

    #[test]
    fn loopback_reverse_lookup_is_localhost_or_none() {
        // Real PTR against 127.0.0.1: either resolves (often "localhost") or, on
        // a host with no PTR, None — both are acceptable; it must not panic.
        let r = reverse_lookup("127.0.0.1".parse().unwrap());
        if let Some(name) = r {
            assert!(!name.is_empty());
        }
    }
}
