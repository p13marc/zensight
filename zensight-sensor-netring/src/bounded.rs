//! Byte-bounded LRU tables (#814).
//!
//! Every inventory in this crate used to guard itself with `len() < CONST` —
//! entries, which nobody can convert to megabytes, and **refuse-at-cap**,
//! which is worse than full: a table that stops admitting freezes on whatever
//! it saw first, so a long-running sensor's TLS/asset/QUIC inventories go
//! *stale*, not just large. [`BoundedTable`] replaces both defects at once:
//! the cap is in **bytes** (the unit the host actually enforces), the policy
//! is true LRU by touch, and eviction is honest — counted, and drainable into
//! the fleet-visible `PublishCounters`.
//!
//! Byte accounting is incremental per record: each table supplies one
//! `entry_bytes(&K, &V)` estimator (shallow struct size + heap capacities).
//! An estimate, but an *adversary-proof* one — a flood of max-size records
//! raises the per-entry cost and therefore lowers the entry count, unlike a
//! sampled average, which drifts exactly when an adversary sends outliers.
//!
//! The governor (#812) evicts through the same path
//! ([`evict_bytes`](BoundedTable::evict_bytes)), so local caps and
//! budget-pressure eviction cannot disagree about what "evict" means.

use std::collections::HashMap;
use std::hash::Hash;

/// One resident value plus its LRU touch-sequence and accounted size.
struct Slot<V> {
    v: V,
    seq: u64,
    bytes: usize,
}

/// A byte-capped, LRU-evicting map. Not a general-purpose cache: sized for
/// this crate's inventories (thousands of entries), where an O(n) scan on the
/// rare eviction batch is microseconds and a second index would be more
/// memory — in the structure whose job is bounding memory.
pub struct BoundedTable<K, V> {
    map: HashMap<K, Slot<V>>,
    next_seq: u64,
    bytes: usize,
    max_bytes: usize,
    entry_bytes: fn(&K, &V) -> usize,
    evicted_entries: u64,
    evicted_bytes: u64,
}

impl<K: Eq + Hash + Clone, V> BoundedTable<K, V> {
    pub fn new(max_bytes: usize, entry_bytes: fn(&K, &V) -> usize) -> Self {
        Self {
            map: HashMap::new(),
            next_seq: 0,
            bytes: 0,
            max_bytes: max_bytes.max(1),
            entry_bytes,
            evicted_entries: 0,
            evicted_bytes: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Accounted bytes currently held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Cumulative `(entries, bytes)` evicted — local overflow and governor
    /// pressure alike. The caller diffs this into `PublishCounters`.
    pub fn evicted_totals(&self) -> (u64, u64) {
        (self.evicted_entries, self.evicted_bytes)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    fn tick(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Read without touching (for iteration-style consumers, see [`iter`]).
    pub fn peek(&self, k: &K) -> Option<&V> {
        self.map.get(k).map(|s| &s.v)
    }

    /// Read and mark recently-used.
    pub fn get(&mut self, k: &K) -> Option<&V> {
        let seq = self.tick();
        self.map.get_mut(k).map(|s| {
            s.seq = seq;
            &s.v
        })
    }

    /// Insert or update in place, re-accounting bytes after the mutation and
    /// marking the entry recently-used. This is the one write path — the old
    /// refuse-at-cap idiom (`contains_key || len < CAP`) becomes a plain
    /// `upsert`, and the cap holds by eviction instead of refusal.
    pub fn upsert(&mut self, k: K, create: impl FnOnce() -> V, update: impl FnOnce(&mut V)) {
        let seq = self.tick();
        match self.map.get_mut(&k) {
            Some(slot) => {
                self.bytes -= slot.bytes;
                update(&mut slot.v);
                slot.bytes = (self.entry_bytes)(&k, &slot.v);
                slot.seq = seq;
                self.bytes += slot.bytes;
            }
            None => {
                let mut v = create();
                update(&mut v);
                let bytes = (self.entry_bytes)(&k, &v);
                self.bytes += bytes;
                self.map.insert(k, Slot { v, seq, bytes });
            }
        }
        if self.bytes > self.max_bytes {
            // Batch down to 15/16 of cap so the O(n) scan amortizes.
            let target = (self.bytes - (self.max_bytes - self.max_bytes / 16)) as u64;
            self.evict_bytes(target);
        }
    }

    /// Evict least-recently-used entries until roughly `target` bytes are
    /// freed (or the table is empty). Returns `(entries, bytes)` actually
    /// freed — the honest number the governor and the counters get.
    pub fn evict_bytes(&mut self, target: u64) -> (u64, u64) {
        if target == 0 || self.map.is_empty() {
            return (0, 0);
        }
        let mut order: Vec<(u64, K)> = self
            .map
            .iter()
            .map(|(k, s)| (s.seq, k.clone()))
            .collect();
        order.sort_unstable_by_key(|(seq, _)| *seq);
        let (mut entries, mut freed) = (0u64, 0u64);
        for (_, k) in order {
            if freed >= target {
                break;
            }
            if let Some(slot) = self.map.remove(&k) {
                self.bytes -= slot.bytes;
                freed += slot.bytes as u64;
                entries += 1;
            }
        }
        self.evicted_entries += entries;
        self.evicted_bytes += freed;
        (entries, freed)
    }

    /// Iterate without touching LRU order (query serving reads everything;
    /// touching on read would make the reader rewrite the eviction order).
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.map.iter().map(|(k, s)| (k, &s.v))
    }
}

// ── entry-size estimators ──────────────────────────────────────────────────
// Shallow struct size + heap capacities. Composed per table beside its type.

/// Heap bytes behind a `String` (the struct itself is counted by the caller's
/// `size_of` term).
pub fn str_heap(s: &str) -> usize {
    s.len()
}

pub fn opt_str_heap(s: &Option<String>) -> usize {
    s.as_deref().map_or(0, str_heap)
}

pub fn vec_str_heap(v: &[String]) -> usize {
    v.iter()
        .map(|s| std::mem::size_of::<String>() + s.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(max: usize) -> BoundedTable<String, String> {
        BoundedTable::new(max, |k, v| {
            std::mem::size_of::<(String, String)>() + k.len() + v.len()
        })
    }

    #[test]
    fn bytes_stay_bounded_under_adversarial_inserts() {
        let mut t = table(64 * 1024);
        // An adversary inserting max-size stringy records forever: the byte
        // cap must hold on EVERY step, not on average.
        for i in 0..10_000 {
            t.upsert(format!("key-{i}"), || "x".repeat(512), |_| {});
            assert!(
                t.bytes() <= t.max_bytes(),
                "cap violated at insert {i}: {} > {}",
                t.bytes(),
                t.max_bytes()
            );
        }
        let (entries, bytes) = t.evicted_totals();
        assert!(entries > 0 && bytes > 0, "eviction was counted");
        assert!(!t.is_empty(), "eviction never empties a live table");
    }

    #[test]
    fn lru_evicts_the_untouched_not_the_recent() {
        let mut t = table(10_000);
        for i in 0..8 {
            t.upsert(format!("k{i}"), || "v".repeat(100), |_| {});
        }
        // Touch k0 so it is the most recent…
        assert!(t.get(&"k0".to_string()).is_some());
        // …then force eviction of roughly half the table.
        t.evict_bytes(t.bytes() as u64 / 2);
        assert!(
            t.contains_key(&"k0".to_string()),
            "the touched entry survives"
        );
        assert!(
            !t.contains_key(&"k1".to_string()),
            "the oldest untouched entry goes first"
        );
    }

    #[test]
    fn upsert_reaccounts_growth_in_place() {
        let mut t = table(1_000_000);
        t.upsert("k".to_string(), String::new, |_| {});
        let before = t.bytes();
        t.upsert("k".to_string(), String::new, |v| *v = "y".repeat(10_000));
        assert_eq!(t.len(), 1);
        assert!(
            t.bytes() >= before + 10_000,
            "in-place growth must be re-accounted"
        );
    }

    #[test]
    fn replace_at_cap_admits_new_data_instead_of_freezing() {
        // The refuse-at-cap regression test: a full table must still learn.
        let mut t = table(4_096);
        for i in 0..200 {
            t.upsert(format!("old-{i}"), || "v".repeat(64), |_| {});
        }
        t.upsert("fresh".to_string(), || "v".repeat(64), |_| {});
        assert!(
            t.contains_key(&"fresh".to_string()),
            "a full table admits new entries by evicting stale ones"
        );
    }
}
