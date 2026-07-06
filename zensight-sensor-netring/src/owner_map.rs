//! Wire-level bandwidth-by-process attribution (#318) — the **opt-in,
//! best-effort** capture tier of epic #320.
//!
//! netring measures per-flow wire bandwidth on the capture hot path; the kernel
//! socket table (sock_diag) plus a `/proc` fd scan
//! ([`SocketOwnerMap`](nlink::sockdiag::SocketOwnerMap)) say which process owns
//! each 5-tuple. Joining the two attributes wire throughput to processes without
//! any cross-process query: netring's
//! [`with_flow_attribution`](netring::monitor::MonitorBuilder::with_flow_attribution)
//! hook is a synchronous hot-path closure, so all the scanning happens **off the
//! hook** on a fixed cadence and the hook itself does one allocation-free map
//! lookup returning an opaque owner slot (`u64`).
//!
//! Byte-semantics are **wire-L2** (full frame incl. Ethernet — the highest
//! count) and the protocol scope is **all** L4s netring tracks (TCP + UDP), so
//! rows are tagged [`ByteSemantics::WireL2`] / [`ProtoScope::All`] and the GUI
//! never blends them with the sock_diag goodput tier.
//!
//! Hard limits, surfaced not hidden: attribution is a **snapshot join** (a flow
//! seen before its socket appears in the dump — or after its process exits —
//! falls into the explicit unattributed bucket, `pid = -1`), it does `/proc`
//! scans (hence opt-in), and netring caches a flow's owner at first packet, so a
//! late socket-table refresh does not retro-attribute an in-flight flow.

use std::collections::HashMap;
use std::net::SocketAddr;

use flowscope::L4Proto;
use netring::protocol::FlowKey;
use zensight_common::bandwidth::{
    BandwidthKey, BandwidthRecord, BandwidthSource, ByteSemantics, ProtoScope,
};

/// Upper bound on distinct owner slots retained across refreshes (#318). Owner
/// identities are interned append-only so a slot handed to the hook stays valid
/// forever within a sensor run; the cap keeps that registry bounded under
/// process churn — beyond it, new owners fold into the unattributed bucket.
pub const MAX_SLOTS: usize = 16_384;

/// A resolved owner: process identity as the `(pid, start_time)` pair (bare PIDs
/// get reused) plus the kernel `comm` for display. Mirrors the netlink tier's
/// [`BandwidthKey::Process`] shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerIdent {
    pub pid: i32,
    pub start_time: u64,
    pub comm: String,
}

/// One socket-table row for the join: canonical L4 + endpoints + inode. Built
/// from a sock_diag dump (or a mock, in tests).
#[derive(Debug, Clone)]
pub struct SocketRow {
    pub proto: L4Proto,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub inode: u32,
}

/// Canonicalise a 5-tuple into netring's bidirectional [`FlowKey`] ordering
/// (`a < b` lexicographic on `SocketAddr`) so a socket-table row keys the same
/// as the live flow netring hands the attribution hook.
pub fn canonical_key(proto: L4Proto, local: SocketAddr, remote: SocketAddr) -> FlowKey {
    let (a, b) = if local <= remote {
        (local, remote)
    } else {
        (remote, local)
    };
    FlowKey::new(proto, a, b)
}

/// The immutable hot-path lookup table, hot-swapped (`ArcSwap`) by the off-hook
/// refresh. The attribution hook loads it and does a single `slot_for` lookup;
/// the periodic report resolves slots back to owners via `owner`.
#[derive(Debug, Default)]
pub struct OwnerTable {
    by_flow: HashMap<FlowKey, u64>,
    slots: Vec<OwnerIdent>,
}

impl OwnerTable {
    /// An empty table — the attribution-disabled / pre-first-scan value.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Allocation-free hot-path lookup used by `with_flow_attribution`: the owner
    /// slot for a live flow, or `None` (→ unattributed) when unknown.
    #[inline]
    pub fn slot_for(&self, key: &FlowKey) -> Option<u64> {
        self.by_flow.get(key).copied()
    }

    /// Resolve a slot handed out by the hook back to its owner identity.
    pub fn owner(&self, slot: u64) -> Option<&OwnerIdent> {
        self.slots.get(slot as usize)
    }

    /// Number of attributed flow keys (for telemetry / debugging).
    pub fn flow_count(&self) -> usize {
        self.by_flow.len()
    }
}

/// Append-only owner-slot registry owned by the refresh task. Interning the same
/// `(pid, start_time)` always returns the same slot for the sensor's lifetime, so
/// a slot cached inside netring (at a flow's first packet) never resolves to a
/// different process after a later refresh.
#[derive(Debug, Default)]
pub struct SlotRegistry {
    by_ident: HashMap<(i32, u64), u64>,
    slots: Vec<OwnerIdent>,
}

impl SlotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern an owner, returning its stable slot. `None` once [`MAX_SLOTS`] is
    /// reached and the owner is new (folds into the unattributed bucket).
    pub fn intern(&mut self, ident: OwnerIdent) -> Option<u64> {
        if let Some(&slot) = self.by_ident.get(&(ident.pid, ident.start_time)) {
            return Some(slot);
        }
        if self.slots.len() >= MAX_SLOTS {
            return None;
        }
        let slot = self.slots.len() as u64;
        self.by_ident.insert((ident.pid, ident.start_time), slot);
        self.slots.push(ident);
        Some(slot)
    }

    /// Clone the current slot table (for a fresh [`OwnerTable`] snapshot).
    fn slots_snapshot(&self) -> Vec<OwnerIdent> {
        self.slots.clone()
    }

    /// Build a fresh [`OwnerTable`] from a socket dump, joining each socket's
    /// inode to its owner via `resolve` and interning the owner into this
    /// registry. Unconnected sockets (listeners, `remote` port 0) are skipped —
    /// their `(local, *:0)` key can never equal a live flow key. Sockets whose
    /// inode resolves to no process, or whose owner exceeds the slot cap, are
    /// simply left out (→ their flows land in the unattributed bucket).
    pub fn build_table(
        &mut self,
        sockets: impl IntoIterator<Item = SocketRow>,
        mut resolve: impl FnMut(u32) -> Option<OwnerIdent>,
    ) -> OwnerTable {
        let mut by_flow: HashMap<FlowKey, u64> = HashMap::new();
        for row in sockets {
            if row.remote.port() == 0 || row.inode == 0 {
                continue;
            }
            let Some(ident) = resolve(row.inode) else {
                continue;
            };
            let Some(slot) = self.intern(ident) else {
                continue;
            };
            by_flow.insert(canonical_key(row.proto, row.local, row.remote), slot);
        }
        OwnerTable {
            by_flow,
            slots: self.slots_snapshot(),
        }
    }
}

/// Turn a periodic owner-bandwidth report into [`BandwidthRecord`]s (#318).
///
/// `top` is `(owner_slot, total_bps)` from `OwnerBandwidthReport::top(..)`;
/// `unknown_bps` is `OwnerBandwidthReport::unknown_rate()`. Owner bandwidth is
/// **undirected** (netring counts total frame bytes per owner), so the whole
/// rate is reported as `tx_bps` with `rx_bps = 0.0` — the GUI shows it under the
/// wire-L2 semantics badge, never summed with the directional goodput tier. An
/// explicit unattributed bucket (`pid = -1`) carries the share netring couldn't
/// map, mirroring the netlink tier. Rows are emitted largest-first; a zero
/// unattributed rate is omitted.
pub fn owner_records(
    top: &[(u64, f64)],
    unknown_bps: f64,
    table: &OwnerTable,
    host: Option<&str>,
) -> Vec<BandwidthRecord> {
    let mut out: Vec<BandwidthRecord> = Vec::with_capacity(top.len() + 1);
    for &(slot, bps) in top {
        if bps <= 0.0 {
            continue;
        }
        // A slot with no owner (registry churned past the cap between the hook
        // caching it and this report) is accounted as unattributed, not dropped.
        let key = match table.owner(slot) {
            Some(o) => BandwidthKey::Process {
                pid: o.pid,
                start_time: o.start_time,
                comm: o.comm.clone(),
            },
            None => unattributed_key(),
        };
        out.push(record(key, bps, host));
    }
    if unknown_bps > 0.0 {
        out.push(record(unattributed_key(), unknown_bps, host));
    }
    out.sort_by(|a, b| {
        b.tx_bps
            .total_cmp(&a.tx_bps)
            .then_with(|| key_ord(a).cmp(&key_ord(b)))
    });
    out
}

fn unattributed_key() -> BandwidthKey {
    BandwidthKey::Process {
        pid: -1,
        start_time: 0,
        comm: "unattributed".into(),
    }
}

fn record(key: BandwidthKey, tx_bps: f64, host: Option<&str>) -> BandwidthRecord {
    BandwidthRecord {
        key,
        tx_bps,
        rx_bps: 0.0,
        source: BandwidthSource::Netring,
        semantics: ByteSemantics::WireL2,
        proto: ProtoScope::All,
        host: host.map(String::from),
    }
}

/// Stable secondary sort key so equal-rate rows order deterministically.
fn key_ord(r: &BandwidthRecord) -> (i32, u64) {
    match &r.key {
        BandwidthKey::Process {
            pid, start_time, ..
        } => (*pid, *start_time),
        _ => (i32::MAX, u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn owner(pid: i32, start: u64, comm: &str) -> OwnerIdent {
        OwnerIdent {
            pid,
            start_time: start,
            comm: comm.into(),
        }
    }

    #[test]
    fn canonical_key_is_order_independent() {
        let a = sa("10.0.0.1:1234");
        let b = sa("93.184.216.34:443");
        let k1 = canonical_key(L4Proto::Tcp, a, b);
        let k2 = canonical_key(L4Proto::Tcp, b, a);
        assert_eq!(k1, k2);
        assert!(k1.a <= k1.b);
    }

    #[test]
    fn build_table_resolves_five_tuple_to_owner() {
        let mut reg = SlotRegistry::new();
        let sockets = vec![
            SocketRow {
                proto: L4Proto::Tcp,
                local: sa("10.0.0.1:1234"),
                remote: sa("93.184.216.34:443"),
                inode: 111,
            },
            SocketRow {
                proto: L4Proto::Udp,
                local: sa("10.0.0.1:5353"),
                remote: sa("8.8.8.8:53"),
                inode: 222,
            },
            // Listener: unconnected → skipped, no slot burned.
            SocketRow {
                proto: L4Proto::Tcp,
                local: sa("0.0.0.0:8080"),
                remote: sa("0.0.0.0:0"),
                inode: 333,
            },
        ];
        let idents: HashMap<u32, OwnerIdent> = HashMap::from([
            (111, owner(42, 900, "curl")),
            (222, owner(7, 100, "resolved")),
            (333, owner(99, 5, "nginx")),
        ]);
        let table = reg.build_table(sockets, |ino| idents.get(&ino).cloned());
        assert_eq!(table.flow_count(), 2);

        // The live flow (either endpoint order) resolves to curl.
        let flow = canonical_key(L4Proto::Tcp, sa("93.184.216.34:443"), sa("10.0.0.1:1234"));
        let slot = table.slot_for(&flow).expect("attributed");
        assert_eq!(table.owner(slot), Some(&owner(42, 900, "curl")));

        // An unknown flow is unattributed.
        let other = canonical_key(L4Proto::Tcp, sa("10.0.0.9:22"), sa("10.0.0.10:40000"));
        assert_eq!(table.slot_for(&other), None);
    }

    #[test]
    fn slots_are_stable_across_refresh() {
        let mut reg = SlotRegistry::new();
        let s1 = reg.intern(owner(42, 900, "curl")).unwrap();
        let s2 = reg.intern(owner(7, 100, "dns")).unwrap();
        assert_ne!(s1, s2);
        // Re-interning the same identity returns the same slot after churn.
        let _ = reg.intern(owner(1000, 1, "other")).unwrap();
        assert_eq!(reg.intern(owner(42, 900, "curl")), Some(s1));
    }

    #[test]
    fn registry_caps_slot_growth() {
        let mut reg = SlotRegistry::new();
        // Pre-fill to the cap.
        reg.slots = (0..MAX_SLOTS as i32).map(|i| owner(i, 1, "p")).collect();
        for (i, id) in reg.slots.iter().enumerate() {
            reg.by_ident.insert((id.pid, id.start_time), i as u64);
        }
        // A new owner beyond the cap can't intern.
        assert_eq!(reg.intern(owner(999_999, 1, "new")), None);
        // But an existing one still resolves.
        assert_eq!(reg.intern(owner(0, 1, "p")), Some(0));
    }

    #[test]
    fn owner_records_maps_slots_and_buckets_unattributed() {
        let mut reg = SlotRegistry::new();
        let sockets = vec![
            SocketRow {
                proto: L4Proto::Tcp,
                local: sa("10.0.0.1:1234"),
                remote: sa("93.184.216.34:443"),
                inode: 111,
            },
            SocketRow {
                proto: L4Proto::Tcp,
                local: sa("10.0.0.1:2000"),
                remote: sa("1.1.1.1:443"),
                inode: 222,
            },
        ];
        let idents: HashMap<u32, OwnerIdent> =
            HashMap::from([(111, owner(42, 900, "curl")), (222, owner(7, 100, "wget"))]);
        let table = reg.build_table(sockets, |ino| idents.get(&ino).cloned());
        let curl = table
            .slot_for(&canonical_key(
                L4Proto::Tcp,
                sa("10.0.0.1:1234"),
                sa("93.184.216.34:443"),
            ))
            .unwrap();
        let wget = table
            .slot_for(&canonical_key(
                L4Proto::Tcp,
                sa("10.0.0.1:2000"),
                sa("1.1.1.1:443"),
            ))
            .unwrap();

        let top = vec![(curl, 3000.0), (wget, 1000.0)];
        let recs = owner_records(&top, 500.0, &table, Some("host01"));
        // curl (3000), wget (1000), unattributed (500), largest-first.
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].tx_bps, 3000.0);
        assert!(matches!(
            &recs[0].key,
            BandwidthKey::Process { pid: 42, .. }
        ));
        assert_eq!(recs[0].source, BandwidthSource::Netring);
        assert_eq!(recs[0].semantics, ByteSemantics::WireL2);
        assert_eq!(recs[0].proto, ProtoScope::All);
        assert_eq!(recs[0].rx_bps, 0.0);
        assert_eq!(recs[0].host.as_deref(), Some("host01"));
        assert_eq!(recs[2].tx_bps, 500.0);
        assert!(matches!(
            &recs[2].key,
            BandwidthKey::Process { pid: -1, .. }
        ));
    }

    #[test]
    fn owner_records_skips_zero_and_omits_zero_unknown() {
        let table = OwnerTable::empty();
        // Slot with no owner + zero unknown → nothing emitted.
        let recs = owner_records(&[(0, 0.0)], 0.0, &table, None);
        assert!(recs.is_empty());
    }

    #[test]
    fn owner_records_unknown_slot_folds_to_unattributed() {
        let table = OwnerTable::empty();
        // A slot the table can't resolve (churn) with real traffic is accounted
        // as unattributed rather than dropped.
        let recs = owner_records(&[(5, 800.0)], 0.0, &table, None);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tx_bps, 800.0);
        assert!(matches!(
            &recs[0].key,
            BandwidthKey::Process { pid: -1, .. }
        ));
    }
}
