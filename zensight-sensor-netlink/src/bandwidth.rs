//! sock_diag per-process bandwidth (#317) — the **unprivileged, TCP-only**
//! fallback tier of epic #320.
//!
//! `tcp_info` carries cumulative goodput byte counters (`bytes_acked` = TX,
//! `bytes_received` = RX). Sampling them on a fixed cadence and diffing **per
//! socket cookie** (never inode — inodes get reused, which corrupts deltas)
//! yields per-socket app-goodput rate; the `@/query/bandwidth` handler then joins
//! those rates to processes at query time via the #304 `/proc` attribution scan.
//!
//! Hard limits, surfaced not hidden: **TCP only** (`udp_diag` has no per-socket
//! byte counters — per-process UDP bandwidth by polling is impossible; that needs
//! the eBPF tier), **app-goodput** (no headers/retransmits — reads below wire),
//! and **misses short flows** opened+closed between samples (the eBPF tier catches
//! those). All rows are tagged accordingly so the GUI never blends them.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use zensight_common::bandwidth::{
    BandwidthKey, BandwidthRecord, BandwidthSource, ByteSemantics, ProtoScope,
};
use zensight_common::query_detail::SocketRecord;

/// Per-cookie TCP goodput rate tracker. Fed cheap sockdiag byte samples on a
/// fixed cadence (no `/proc` scan); the query handler reads the latest rates.
#[derive(Default)]
pub struct BandwidthTracker {
    /// cookie → (bytes_acked, bytes_received, sampled-at) of the last sample.
    prev: HashMap<u64, (u64, u64, Instant)>,
    /// cookie → (tx_bps, rx_bps) latest computed rate.
    rates: HashMap<u64, (f64, f64)>,
}

impl BandwidthTracker {
    /// Update from a fresh sample of `(cookie, bytes_acked, bytes_received)`.
    /// Cookies absent from the sample are pruned (socket closed). A counter that
    /// went **backwards** (cookie reuse / reset) re-baselines with no rate that
    /// tick — never a negative rate.
    pub fn update(&mut self, sample: impl IntoIterator<Item = (u64, u64, u64)>, now: Instant) {
        let mut seen: HashSet<u64> = HashSet::new();
        for (cookie, acked, received) in sample {
            seen.insert(cookie);
            if let Some(&(pa, pr, at)) = self.prev.get(&cookie) {
                let dt = now.duration_since(at).as_secs_f64();
                if dt > 0.0 {
                    let tx = acked.checked_sub(pa).map(|d| d as f64 / dt).unwrap_or(0.0);
                    let rx = received
                        .checked_sub(pr)
                        .map(|d| d as f64 / dt)
                        .unwrap_or(0.0);
                    self.rates.insert(cookie, (tx, rx));
                }
            }
            self.prev.insert(cookie, (acked, received, now));
        }
        // Drop sockets that vanished this sample (closed) so the maps stay bounded.
        self.prev.retain(|c, _| seen.contains(c));
        self.rates.retain(|c, _| seen.contains(c));
    }

    /// Latest `(tx_bps, rx_bps)` for a cookie; `(0, 0)` if unseen/idle.
    pub fn rate(&self, cookie: u64) -> (f64, f64) {
        self.rates.get(&cookie).copied().unwrap_or((0.0, 0.0))
    }
}

/// Aggregate attributed sockets into per-process bandwidth (#317), summing each
/// process's socket rates. Sockets without a `(pid, proc_start_time)` fold into a
/// single explicit **unattributed** bucket (`pid = -1`) whose share the GUI shows
/// rather than dropping. Only moving rows (tx or rx > 0) are emitted; the result
/// is sorted by total rate, highest first, and truncated to `top`.
pub fn aggregate_by_process(
    records: &[SocketRecord],
    tracker: &BandwidthTracker,
    host: Option<&str>,
    top: usize,
) -> Vec<BandwidthRecord> {
    // (pid, start_time) → (tx_bps, rx_bps, comm); None key = unattributed bucket.
    let mut agg: HashMap<Option<(i32, u64)>, (f64, f64, String)> = HashMap::new();
    for r in records {
        let (tx, rx) = tracker.rate(r.cookie);
        if tx == 0.0 && rx == 0.0 {
            continue;
        }
        let key = match (r.pid, r.proc_start_time) {
            (Some(pid), Some(st)) => Some((pid, st)),
            _ => None,
        };
        let comm = r.process.clone().unwrap_or_else(|| "unattributed".into());
        let e = agg.entry(key).or_insert((0.0, 0.0, comm));
        e.0 += tx;
        e.1 += rx;
    }
    let mut out: Vec<BandwidthRecord> = agg
        .into_iter()
        .map(|(key, (tx, rx, comm))| {
            let bk = match key {
                Some((pid, start_time)) => BandwidthKey::Process {
                    pid,
                    start_time,
                    comm,
                },
                None => BandwidthKey::Process {
                    pid: -1,
                    start_time: 0,
                    comm: "unattributed".into(),
                },
            };
            BandwidthRecord {
                key: bk,
                tx_bps: tx,
                rx_bps: rx,
                source: BandwidthSource::SockDiag,
                semantics: ByteSemantics::AppGoodput,
                proto: ProtoScope::Tcp,
                host: host.map(String::from),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (b.tx_bps + b.rx_bps)
            .total_cmp(&(a.tx_bps + a.rx_bps))
            .then_with(|| a.key_ord().cmp(&b.key_ord()))
    });
    out.truncate(top);
    out
}

trait KeyOrd {
    /// A stable secondary sort key so equal-rate rows order deterministically.
    fn key_ord(&self) -> (i32, u64);
}
impl KeyOrd for BandwidthRecord {
    fn key_ord(&self) -> (i32, u64) {
        match &self.key {
            BandwidthKey::Process {
                pid, start_time, ..
            } => (*pid, *start_time),
            _ => (i32::MAX, u64::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(cookie: u64, pid: Option<i32>, start: Option<u64>, comm: &str) -> SocketRecord {
        SocketRecord {
            cookie,
            pid,
            proc_start_time: start,
            process: pid.map(|_| comm.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn tracker_computes_goodput_delta_and_guards_reset() {
        let t0 = Instant::now();
        let mut tr = BandwidthTracker::default();
        // First sample seeds the baseline (no rate yet).
        tr.update([(1u64, 1000u64, 500u64)], t0);
        assert_eq!(tr.rate(1), (0.0, 0.0));
        // +2000 acked, +1000 received over 2 s → 1000/500 B/s.
        tr.update(
            [(1u64, 3000u64, 1500u64)],
            t0 + std::time::Duration::from_secs(2),
        );
        let (tx, rx) = tr.rate(1);
        assert_eq!(tx, 1000.0);
        assert_eq!(rx, 500.0);
        // Counter reset (cookie reuse) → no negative rate.
        tr.update(
            [(1u64, 100u64, 50u64)],
            t0 + std::time::Duration::from_secs(4),
        );
        assert_eq!(tr.rate(1), (0.0, 0.0));
    }

    #[test]
    fn tracker_prunes_closed_sockets() {
        let t0 = Instant::now();
        let mut tr = BandwidthTracker::default();
        tr.update([(1u64, 0, 0), (2u64, 0, 0)], t0);
        tr.update([(1u64, 10, 10)], t0 + std::time::Duration::from_secs(1));
        // cookie 2 vanished → pruned to (0,0).
        assert_eq!(tr.rate(2), (0.0, 0.0));
    }

    #[test]
    fn aggregate_sums_per_process_and_buckets_unattributed() {
        let t0 = Instant::now();
        let mut tr = BandwidthTracker::default();
        tr.update([(1u64, 0, 0), (2u64, 0, 0), (3u64, 0, 0)], t0);
        tr.update(
            [(1u64, 2000, 0), (2u64, 1000, 0), (3u64, 4000, 0)],
            t0 + std::time::Duration::from_secs(1),
        );
        let recs = vec![
            sock(1, Some(42), Some(111), "curl"), // 2000 B/s
            sock(2, Some(42), Some(111), "curl"), // 1000 B/s → same process
            sock(3, None, None, ""),              // 4000 B/s → unattributed
        ];
        let out = aggregate_by_process(&recs, &tr, Some("host01"), 10);
        assert_eq!(out.len(), 2);
        // Highest total first: unattributed (4000) then curl (3000).
        assert_eq!(out[0].tx_bps, 4000.0);
        assert!(matches!(&out[0].key, BandwidthKey::Process { pid: -1, .. }));
        assert_eq!(out[1].tx_bps, 3000.0);
        assert!(matches!(&out[1].key, BandwidthKey::Process { pid: 42, .. }));
        assert_eq!(out[1].source, BandwidthSource::SockDiag);
        assert_eq!(out[1].proto, ProtoScope::Tcp);
        assert_eq!(out[0].host.as_deref(), Some("host01"));
    }

    #[test]
    fn aggregate_truncates_to_top() {
        let t0 = Instant::now();
        let mut tr = BandwidthTracker::default();
        let seed: Vec<_> = (0..5u64).map(|c| (c, 0u64, 0u64)).collect();
        tr.update(seed.clone(), t0);
        let next: Vec<_> = (0..5u64).map(|c| (c, (c + 1) * 100, 0)).collect();
        tr.update(next, t0 + std::time::Duration::from_secs(1));
        let recs: Vec<_> = (0..5u64)
            .map(|c| sock(c, Some(c as i32), Some(1), "p"))
            .collect();
        let out = aggregate_by_process(&recs, &tr, None, 2);
        assert_eq!(out.len(), 2);
    }
}
