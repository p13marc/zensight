//! Host-evidence feed (#307): republish observed neighbors (ARP/NDP cache) as
//! third-party identity evidence on `zensight/_meta/evidence/**` for the
//! correlator (epic #312).
//!
//! Each VALID neighbor (Reachable/Stale/Permanent) becomes a `HostEvidence`
//! claim keyed by MAC (`observer = Some("netlink")`), collecting every IP the
//! kernel has bound to that MAC. Publishing is change-driven with a periodic
//! liveness refresh and a hard per-run cap; the neighbor poll loop drives the
//! feed but only acts every `min_interval_secs` so a fast poll can't spam.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use nlink::netlink::messages::NeighborMessage;
use nlink::netlink::neigh::State as NeighborState;
use zensight_common::HostEvidence;

/// Cap on the per-source dedup bookkeeping maps.
const DEDUP_CAP: usize = 4_096;

/// Per-source publish bookkeeping for the neighbor evidence feed.
#[derive(Default)]
pub struct EvidenceState {
    /// Last-published content hash per source (evidence source = MAC slug).
    last_hash: HashMap<String, u64>,
    /// Last-emit time (unix ms) per source.
    last_emit: HashMap<String, i64>,
    /// Wall-clock (unix ms) the feed last ran — the `min_interval_secs` floor.
    /// `None` until the first run so the feed always fires once at startup.
    last_run_ms: Option<i64>,
}

/// A neighbor state carrying an identifying binding worth republishing.
/// FAILED / INCOMPLETE / NONE / DELAY / PROBE / NOARP are transient or
/// unresolved and skipped.
fn is_valid(state: NeighborState) -> bool {
    matches!(
        state,
        NeighborState::Reachable | NeighborState::Stale | NeighborState::Permanent
    )
}

/// Slugify a MAC for use as an evidence `source`: lowercase, `:` → `-`.
pub fn mac_slug(mac: &str) -> String {
    mac.to_ascii_lowercase().replace(':', "-")
}

/// Whether a MAC is empty or all-zero (`00:00:...`) — not identifying.
pub fn is_zero_mac(mac: &str) -> bool {
    mac.chars().all(|c| matches!(c, '0' | ':' | '-'))
}

/// Pure map: valid neighbors → one third-party `HostEvidence` per MAC, merging
/// every destination IP the kernel bound to that MAC. Neighbors in a transient
/// state or without a resolvable MAC are skipped. Output is sorted by source
/// for deterministic ordering.
pub fn neighbor_evidence(neighbors: &[NeighborMessage], now_ms: i64) -> Vec<HostEvidence> {
    let mut by_mac: HashMap<String, Vec<String>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for n in neighbors {
        if !is_valid(n.state()) {
            continue;
        }
        let Some(mac) = n.mac_address() else {
            continue;
        };
        if is_zero_mac(&mac) {
            continue;
        }
        let ips = by_mac.entry(mac.clone()).or_insert_with(|| {
            order.push(mac.clone());
            Vec::new()
        });
        if let Some(ip) = n.destination() {
            let s = ip.to_string();
            if !ips.contains(&s) {
                ips.push(s);
            }
        }
    }
    order.sort();
    order
        .into_iter()
        .map(|mac| {
            let ips = by_mac.remove(&mac).unwrap_or_default();
            HostEvidence {
                sensor: "netlink".to_string(),
                source: mac_slug(&mac),
                observer: Some("netlink".to_string()),
                host_id: None,
                boot_id: None,
                hostname: None,
                fqdn: None,
                ips,
                macs: vec![mac],
                vendor: None,
                platform: None,
                last_updated: now_ms,
            }
        })
        .collect()
}

/// Content hash of the identifying fields (everything but `last_updated`), used
/// to distinguish real changes from liveness refreshes. IPs/MACs are sorted so
/// a reordering isn't mistaken for a change.
pub fn content_hash(ev: &HostEvidence) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ev.sensor.hash(&mut h);
    ev.source.hash(&mut h);
    ev.observer.hash(&mut h);
    ev.host_id.hash(&mut h);
    ev.boot_id.hash(&mut h);
    ev.hostname.hash(&mut h);
    ev.fqdn.hash(&mut h);
    let mut ips = ev.ips.clone();
    ips.sort();
    ips.hash(&mut h);
    let mut macs = ev.macs.clone();
    macs.sort();
    macs.hash(&mut h);
    ev.vendor.hash(&mut h);
    ev.platform.hash(&mut h);
    h.finish()
}

/// Pure decision: should this claim be (re)published now? New/changed content →
/// yes; unchanged but `refresh_ms` elapsed since last emit → yes (liveness);
/// otherwise no.
pub fn should_publish(
    now_ms: i64,
    last_hash: Option<u64>,
    new_hash: u64,
    last_emit_ms: Option<i64>,
    refresh_ms: i64,
) -> bool {
    match last_hash {
        Some(h) if h == new_hash => match last_emit_ms {
            Some(t) => now_ms.saturating_sub(t) >= refresh_ms,
            None => true,
        },
        _ => true,
    }
}

impl EvidenceState {
    /// Whether the feed may run now given the `min_interval_secs` floor; records
    /// the run time when it returns `true`.
    pub fn may_run(&mut self, now_ms: i64, min_interval_ms: i64) -> bool {
        if let Some(last) = self.last_run_ms
            && now_ms.saturating_sub(last) < min_interval_ms
        {
            return false;
        }
        self.last_run_ms = Some(now_ms);
        true
    }

    /// Decide which of `evs` to publish this run (change- or refresh-driven).
    /// Decision-only — call [`EvidenceState::commit`] for the claims actually
    /// published so deferred (over-cap) claims aren't marked emitted.
    pub fn select(
        &self,
        evs: Vec<HostEvidence>,
        now_ms: i64,
        refresh_ms: i64,
    ) -> Vec<HostEvidence> {
        evs.into_iter()
            .filter(|ev| {
                should_publish(
                    now_ms,
                    self.last_hash.get(&ev.source).copied(),
                    content_hash(ev),
                    self.last_emit.get(&ev.source).copied(),
                    refresh_ms,
                )
            })
            .collect()
    }

    /// Record the emit of each published claim (bounded, stalest-evicted).
    pub fn commit(&mut self, published: &[HostEvidence], now_ms: i64) {
        for ev in published {
            let hash = content_hash(ev);
            self.record_emit(&ev.source, hash, now_ms);
        }
    }

    fn record_emit(&mut self, source: &str, hash: u64, now_ms: i64) {
        if !self.last_hash.contains_key(source)
            && self.last_hash.len() >= DEDUP_CAP
            && let Some(victim) = self
                .last_emit
                .iter()
                .min_by_key(|&(_, &t)| t)
                .map(|(k, _)| k.clone())
        {
            self.last_hash.remove(&victim);
            self.last_emit.remove(&victim);
        }
        self.last_hash.insert(source.to_string(), hash);
        self.last_emit.insert(source.to_string(), now_ms);
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use nlink::netlink::messages::NeighborMessageBuilder;

    use super::*;

    fn neigh(ip: &str, mac: &[u8], state: NeighborState) -> NeighborMessage {
        NeighborMessageBuilder::new()
            .destination(ip.parse::<IpAddr>().unwrap())
            .lladdr(mac.to_vec())
            .state(state)
            .build()
    }

    #[test]
    fn groups_two_ips_of_one_mac_into_one_evidence() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let ns = vec![
            neigh("10.0.0.1", &mac, NeighborState::Reachable),
            neigh("10.0.0.2", &mac, NeighborState::Stale),
        ];
        let evs = neighbor_evidence(&ns, 1_000);
        assert_eq!(evs.len(), 1);
        let ev = &evs[0];
        assert_eq!(ev.sensor, "netlink");
        assert_eq!(ev.observer.as_deref(), Some("netlink"));
        assert_eq!(ev.source, "aa-bb-cc-dd-ee-ff");
        assert_eq!(ev.macs, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert_eq!(ev.ips.len(), 2);
        assert!(ev.ips.contains(&"10.0.0.1".to_string()));
        assert!(ev.ips.contains(&"10.0.0.2".to_string()));
        assert_eq!(ev.hostname, None);
        assert_eq!(ev.last_updated, 1_000);
    }

    #[test]
    fn skips_invalid_states() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01];
        for st in [
            NeighborState::Failed,
            NeighborState::Incomplete,
            NeighborState::None,
            NeighborState::Delay,
            NeighborState::Probe,
        ] {
            let ns = vec![neigh("10.0.0.9", &mac, st)];
            assert!(
                neighbor_evidence(&ns, 1).is_empty(),
                "state {st:?} should be skipped"
            );
        }
    }

    #[test]
    fn skips_missing_and_zero_mac() {
        // A non-6-byte lladdr yields no MAC → skipped.
        let short = NeighborMessageBuilder::new()
            .destination("10.0.0.3".parse().unwrap())
            .lladdr(vec![0xaa, 0xbb])
            .state(NeighborState::Reachable)
            .build();
        assert!(neighbor_evidence(&[short], 1).is_empty());
        // An all-zero MAC is not identifying.
        let zero = neigh("10.0.0.4", &[0, 0, 0, 0, 0, 0], NeighborState::Reachable);
        assert!(neighbor_evidence(&[zero], 1).is_empty());
    }

    #[test]
    fn distinct_macs_yield_distinct_sorted_evidence() {
        let ns = vec![
            neigh("10.0.0.2", &[0xbb, 0, 0, 0, 0, 2], NeighborState::Reachable),
            neigh("10.0.0.1", &[0xaa, 0, 0, 0, 0, 1], NeighborState::Reachable),
        ];
        let evs = neighbor_evidence(&ns, 1);
        assert_eq!(evs.len(), 2);
        // Sorted by source.
        assert_eq!(evs[0].source, "aa-00-00-00-00-01");
        assert_eq!(evs[1].source, "bb-00-00-00-00-02");
    }

    #[test]
    fn select_dedups_then_refreshes() {
        let mut st = EvidenceState::default();
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let ns = vec![neigh("10.0.0.1", &mac, NeighborState::Reachable)];
        let refresh = 420_000;
        // First run publishes; commit records it.
        let out = st.select(neighbor_evidence(&ns, 1_000), 1_000, refresh);
        assert_eq!(out.len(), 1);
        st.commit(&out, 1_000);
        // Same content within refresh window → nothing.
        let out = st.select(neighbor_evidence(&ns, 1_100), 1_100, refresh);
        assert!(out.is_empty());
        // Same content past refresh window → re-emitted for liveness.
        let out = st.select(
            neighbor_evidence(&ns, 1_000 + refresh),
            1_000 + refresh,
            refresh,
        );
        assert_eq!(out.len(), 1);
        st.commit(&out, 1_000 + refresh);
    }

    #[test]
    fn may_run_honors_min_interval() {
        let mut st = EvidenceState::default();
        assert!(st.may_run(10_000, 60_000));
        assert!(!st.may_run(40_000, 60_000));
        assert!(st.may_run(70_000, 60_000));
    }
}
