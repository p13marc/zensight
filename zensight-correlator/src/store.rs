//! In-memory evidence and name stores.
//!
//! Both are bounded and TTL-swept: the correlator holds only current evidence,
//! so a restart rebuilds identical state from the sensors' cached self-reports
//! (no local DB). The [`NameStore`] is the one place that *accumulates* rather
//! than replaces — see its docs.

use std::collections::HashMap;

use zensight_common::{HostEvidence, NameObservation, NameVal};

/// Store of the latest [`HostEvidence`] per `(sensor, source)`.
#[derive(Debug, Default)]
pub struct EvidenceStore {
    map: HashMap<(String, String), HostEvidence>,
}

impl EvidenceStore {
    /// Insert or replace the claim for its `(sensor, source)` key.
    pub fn upsert(&mut self, ev: HostEvidence) {
        self.map.insert((ev.sensor.clone(), ev.source.clone()), ev);
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
        self.map.retain(|_, ev| ev.last_updated >= cutoff);
    }

    /// Snapshot the TTL-live evidence (`last_updated >= now_ms - ttl_ms`).
    pub fn live(&self, now_ms: i64, ttl_ms: i64) -> Vec<HostEvidence> {
        let cutoff = now_ms - ttl_ms;
        self.map
            .values()
            .filter(|ev| ev.last_updated >= cutoff)
            .cloned()
            .collect()
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
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            container_id: None,
            cloud: None,
            last_updated: 1000,
        };
        s.upsert(fresh.clone());
        fresh.source = "b".into();
        fresh.last_updated = 100;
        s.upsert(fresh);
        // now=1500, ttl=600 → cutoff 900: only "a" is live.
        let live = s.live(1500, 600);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].source, "a");
        s.sweep(1500, 600);
        assert_eq!(s.len(), 1);
    }
}
