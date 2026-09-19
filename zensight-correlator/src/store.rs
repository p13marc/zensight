//! In-memory evidence and name stores.
//!
//! Both are bounded and TTL-swept: the correlator holds only current evidence,
//! so a restart rebuilds identical state from the sensors' cached self-reports
//! (no local DB). The [`NameStore`] is the one place that *accumulates* rather
//! than replaces — see its docs.

use std::collections::HashMap;

use zensight_common::{HostEvidence, NameObservation, NameVal};

/// How a claim is keyed: `(sensor, source)`.
type StoreKey = (String, String);

/// Store of the latest [`HostEvidence`] per [`StoreKey`].
#[derive(Debug, Default)]
pub struct EvidenceStore {
    map: HashMap<StoreKey, (String, HostEvidence)>,
}

/// The most identity claims held at once (#1145).
///
/// This was the one per-key store in this crate with **no cap**, beside
/// [`MAX_IPS`], `RelationStore::MAX_RELATIONS` and `AlertStore::MAX_FIRING`,
/// each of which says why it has one.
///
/// netring publishes one `evidence/device/<mac>` per observed L2 asset, so a
/// flat segment is thousands of claims — every one of them re-merged on each
/// debounce, with the `mac_ip` rule quadratic inside a MAC bucket. A segment
/// that grows, or a sender that mints addresses, grew this without limit.
///
/// Generous next to a real fleet and far below where the merge stops being
/// affordable. Past it the claim whose `last_updated` is oldest is evicted:
/// a stale claim is the one the TTL sweep would have taken anyway.
pub const MAX_EVIDENCE: usize = 50_000;

impl EvidenceStore {
    /// Insert or replace the claim for its `(sensor, source)` key, recording
    /// the **origin the claim was published from**.
    ///
    /// The origin is not in the payload and cannot be derived from it: a claim
    /// says which *sensor* made it, only the key says which *host* that sensor
    /// ran on. Identity does not need it — the merge is a function of the
    /// claim's content — but the topology graph does: "netlink on host A sees
    /// device D" is a statement about A's link-layer segment, and without A
    /// the observation is unattributable (#917).
    /// Bounded by [`MAX_EVIDENCE`] (#1145): past it the claim with the oldest
    /// `last_updated` is evicted, which is the one the TTL sweep would have
    /// taken next anyway.
    pub fn upsert(&mut self, origin: String, ev: HostEvidence) {
        let key = (ev.sensor.clone(), ev.source.clone());
        if !self.map.contains_key(&key) && self.map.len() >= MAX_EVIDENCE {
            self.evict_stalest();
        }
        self.map.insert(key, (origin, ev));
    }

    /// Drop the claim whose `last_updated` is oldest.
    fn evict_stalest(&mut self) {
        if let Some(victim) = self
            .map
            .iter()
            .min_by_key(|(_, (_, ev))| ev.last_updated)
            .map(|(k, _)| k.clone())
        {
            tracing::warn!(
                sensor = %victim.0, source = %victim.1, cap = MAX_EVIDENCE,
                "evidence store full; evicting the stalest claim"
            );
            self.map.remove(&victim);
        }
    }

    /// Drop the claim for `(sensor, source)` (evidence tombstone). Returns
    /// whether anything was removed.
    pub fn remove(&mut self, sensor: &str, source: &str) -> bool {
        self.map
            .remove(&(sensor.to_string(), source.to_string()))
            .is_some()
    }

    /// Remove claims whose `last_updated` is older than `now_ms - ttl_ms`.
    pub fn sweep(&mut self, now_ms: i64, ttl_ms: i64) {
        let cutoff = now_ms - ttl_ms;
        self.map.retain(|_, (_, ev)| ev.last_updated >= cutoff);
    }

    /// Snapshot the TTL-live evidence (`last_updated >= now_ms - ttl_ms`).
    pub fn live(&self, now_ms: i64, ttl_ms: i64) -> Vec<HostEvidence> {
        let cutoff = now_ms - ttl_ms;
        self.map
            .values()
            .filter(|(_, ev)| ev.last_updated >= cutoff)
            .map(|(_, ev)| ev.clone())
            .collect()
    }

    /// The TTL-live evidence with the origin each claim was published from, in
    /// a **deterministic order**.
    ///
    /// Sorted by the store key, because this feeds edge derivation and
    /// therefore a content hash: a `HashMap`'s iteration order is not stable
    /// between runs of the same binary, never mind across restarts, and an
    /// unstable order here would churn the entire edge set forever.
    pub fn live_with_origin(&self, now_ms: i64, ttl_ms: i64) -> Vec<(String, HostEvidence)> {
        let cutoff = now_ms - ttl_ms;
        let mut v: Vec<(StoreKey, String, HostEvidence)> = self
            .map
            .iter()
            .filter(|(_, (_, ev))| ev.last_updated >= cutoff)
            .map(|(k, (origin, ev))| (k.clone(), origin.clone(), ev.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.into_iter().map(|(_, origin, ev)| (origin, ev)).collect()
    }

    /// Number of stored claims.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Maximum accumulated names kept per IP.
pub const MAX_NAMES_PER_IP: usize = 16;
/// Maximum number of distinct IPs tracked.
pub const MAX_IPS: usize = 100_000;

/// Per-IP accumulator of provenance-tagged names.
///
/// **Accumulates, does not replace.** #307 publishes exactly one
/// [`NameObservation`] per IP key (last-writer-wins on the wire), so the
/// distinct names an IP resolves to (an A record, a PTR, a TLS SNI, ...) arrive
/// as *separate* samples over time. Replacing on each sample would keep only the
/// most recent name; instead each observation add-or-refreshes a
/// `(name, provenance)` entry so `entity.names` and the names queryable can
/// return the full set despite the single-name wire limitation.
#[derive(Debug, Default)]
pub struct NameStore {
    map: HashMap<String, Vec<NameVal>>,
}

impl NameStore {
    /// Add or refresh a name for an IP. If the `(name, provenance)` pair already
    /// exists, its `last_seen` is bumped (never regressed); otherwise it is
    /// pushed. The per-IP vec is capped ([`MAX_NAMES_PER_IP`], evicting the
    /// oldest `last_seen`) and the IP count is bounded ([`MAX_IPS`]).
    pub fn upsert(&mut self, obs: NameObservation) {
        // Enforce the global IP cap before inserting a brand-new IP.
        if !self.map.contains_key(&obs.ip) && self.map.len() >= MAX_IPS {
            self.evict_stalest_ip();
        }
        let entry = self.map.entry(obs.ip.clone()).or_default();
        if let Some(existing) = entry
            .iter_mut()
            .find(|n| n.name == obs.name && n.provenance == obs.provenance)
        {
            existing.last_seen = existing.last_seen.max(obs.last_seen);
        } else {
            entry.push(NameVal {
                name: obs.name,
                provenance: obs.provenance,
                last_seen: obs.last_seen,
            });
        }
        if entry.len() > MAX_NAMES_PER_IP {
            // Evict the oldest by last_seen (most-recent first, then truncate).
            entry.sort_by_key(|n| std::cmp::Reverse(n.last_seen));
            entry.truncate(MAX_NAMES_PER_IP);
        }
    }

    /// Top-N names for an IP, ranked most-recent-first then by name/provenance
    /// (deterministic for equal `last_seen`).
    pub fn top_n(&self, ip: &str, n: usize) -> Vec<NameVal> {
        let mut names = self.map.get(ip).cloned().unwrap_or_default();
        names.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.provenance.cmp(&b.provenance))
        });
        names.truncate(n);
        names
    }

    /// Remove name entries older than `now_ms - ttl_ms`; drop IPs left empty.
    pub fn sweep(&mut self, now_ms: i64, ttl_ms: i64) {
        let cutoff = now_ms - ttl_ms;
        self.map.retain(|_, names| {
            names.retain(|n| n.last_seen >= cutoff);
            !names.is_empty()
        });
    }

    /// Number of tracked IPs.
    pub fn ip_count(&self) -> usize {
        self.map.len()
    }

    /// Evict the IP whose most-recent name is the oldest (bounds memory).
    fn evict_stalest_ip(&mut self) {
        if let Some(victim) = self
            .map
            .iter()
            .min_by_key(|(_, names)| names.iter().map(|n| n.last_seen).max().unwrap_or(i64::MIN))
            .map(|(ip, _)| ip.clone())
        {
            self.map.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(ip: &str, name: &str, prov: &str, ts: i64) -> NameObservation {
        NameObservation {
            observer: "netring".into(),
            ip: ip.into(),
            name: name.into(),
            provenance: prov.into(),
            last_seen: ts,
        }
    }

    #[test]
    fn name_store_accumulates_multiple_names_per_ip() {
        let mut s = NameStore::default();
        s.upsert(obs("10.0.0.9", "a.example.com", "dns_a", 100));
        s.upsert(obs("10.0.0.9", "ptr.example.com", "dns_ptr", 200));
        // A different name for the same IP must NOT replace the first.
        let names = s.top_n("10.0.0.9", 10);
        assert_eq!(names.len(), 2);
        // Ranked most-recent-first.
        assert_eq!(names[0].name, "ptr.example.com");
    }

    #[test]
    fn name_store_refreshes_last_seen_in_place() {
        let mut s = NameStore::default();
        s.upsert(obs("10.0.0.9", "a.example.com", "dns_a", 100));
        s.upsert(obs("10.0.0.9", "a.example.com", "dns_a", 300));
        let names = s.top_n("10.0.0.9", 10);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].last_seen, 300);
    }

    #[test]
    fn name_store_caps_per_ip() {
        let mut s = NameStore::default();
        for i in 0..(MAX_NAMES_PER_IP + 5) {
            s.upsert(obs(
                "10.0.0.9",
                &format!("n{i}.example.com"),
                "dns_a",
                i as i64,
            ));
        }
        assert!(s.top_n("10.0.0.9", 1000).len() <= MAX_NAMES_PER_IP);
    }

    #[test]
    fn name_store_sweeps_stale() {
        let mut s = NameStore::default();
        s.upsert(obs("10.0.0.9", "old.example.com", "dns_a", 100));
        s.upsert(obs("10.0.0.9", "new.example.com", "dns_a", 1000));
        s.sweep(1500, 600); // cutoff 900 → drops the ts=100 entry
        let names = s.top_n("10.0.0.9", 10);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].name, "new.example.com");
    }

    #[test]
    fn evidence_store_ttl_filter() {
        let mut s = EvidenceStore::default();
        let mut fresh = HostEvidence {
            sensor: "sysinfo".into(),
            source: "a".into(),
            observer: None,
            host_id: None,
            boot_id: None,
            hostname: None,
            fqdn: None,
            ips: vec![],
            macs: vec![],
            vendor: None,
            platform: None,
            container_id: None,
            cloud: None,
            last_updated: 1000,
        };
        s.upsert("h-test".into(), fresh.clone());
        fresh.source = "b".into();
        fresh.last_updated = 100;
        s.upsert("h-test".into(), fresh);
        // now=1500, ttl=600 → cutoff 900: only "a" is live.
        let live = s.live(1500, 600);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].source, "a");
        s.sweep(1500, 600);
        assert_eq!(s.len(), 1);
    }

    /// **#1145.** The evidence store is bounded, and evicts the stalest claim.
    ///
    /// It was the one per-key store in this crate with no cap, beside
    /// `MAX_IPS`, `MAX_RELATIONS` and `MAX_FIRING` — each of which says why it
    /// has one. netring publishes one `evidence/device/<mac>` per observed L2
    /// asset, so a flat segment is thousands of claims, every one re-merged on
    /// each debounce with the `mac_ip` rule quadratic inside a MAC bucket.
    #[test]
    fn the_evidence_store_is_bounded_and_evicts_the_stalest() {
        let mut s = EvidenceStore::default();
        let claim = |source: &str, ts: i64| HostEvidence {
            sensor: "netring".into(),
            source: source.into(),
            observer: Some("netring".into()),
            host_id: None,
            boot_id: None,
            hostname: None,
            fqdn: None,
            ips: vec![],
            macs: vec![source.to_string()],
            vendor: None,
            platform: None,
            container_id: None,
            cloud: None,
            last_updated: ts,
        };

        // The one we care about keeping: seen most recently.
        s.upsert("h-test".into(), claim("live", i64::MAX));
        // A flat segment's worth, oldest first, well past the cap.
        for i in 0..(MAX_EVIDENCE + 100) {
            s.upsert("h-test".into(), claim(&format!("mac{i}"), i as i64));
        }

        assert!(
            s.map.len() <= MAX_EVIDENCE,
            "{} claims held, cap is {MAX_EVIDENCE}",
            s.map.len()
        );
        assert!(
            s.map
                .contains_key(&("netring".to_string(), "live".to_string())),
            "the most recently seen claim must survive the flood"
        );
        // And the very oldest are the ones that went.
        assert!(
            !s.map
                .contains_key(&("netring".to_string(), "mac0".to_string())),
            "the stalest claim is the one evicted"
        );
    }
}
