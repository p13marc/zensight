//! Shared types/constants for the netlink eBPF module (#114).
//!
//! Used by both the kernel programs (`zensight-sensor-netlink-ebpf`, no_std,
//! `bpfel-unknown-none`) and the userspace loader (`zensight-sensor-netlink`
//! `ebpf` feature). All record types are `#[repr(C)]` POD so they cross the
//! kernel/userspace boundary (ring buffer / hash map) byte-for-byte.
#![no_std]

/// Number of log2 connect-latency buckets (µs). 27 slots → top bucket upper
/// bound `2^26` µs ≈ 67 s, which caps any TCP connect latency.
pub const CONNLAT_BUCKETS: usize = 27;

/// log2 bucket index for a microsecond value, clamped to `CONNLAT_BUCKETS - 1`.
/// Shared so the kernel (increment) and userspace (percentile boundaries) agree.
#[inline]
pub fn connlat_bucket(us: u64) -> u32 {
    if us == 0 {
        return 0;
    }
    let b = 64 - us.leading_zeros();
    if b >= CONNLAT_BUCKETS as u32 {
        CONNLAT_BUCKETS as u32 - 1
    } else {
        b
    }
}

/// Per-peer retransmit key: address family (`AF_INET`/`AF_INET6`) + raw address
/// bytes (v4 in the first 4 bytes, v6 uses all 16).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RetransKey {
    pub family: u8,
    pub _pad: [u8; 3],
    pub addr: [u8; 16],
}

/// `ConnRecord.event`: the connection closed (tcplife record).
pub const CONN_EVENT_CLOSE: u8 = 0;
/// `ConnRecord.event`: the connection reached ESTABLISHED (client-side
/// connect) — live-attribution record (#304, tier 2b).
pub const CONN_EVENT_ESTABLISHED: u8 = 1;

/// A connection event record, submitted to the ring buffer when a socket
/// transitions to `TCP_CLOSE` (`event == CONN_EVENT_CLOSE`, tcplife) or —
/// tier 2b (#304) — when a client-side connect reaches `TCP_ESTABLISHED`
/// (`event == CONN_EVENT_ESTABLISHED`). Fixed-size, no heap — kernel-safe.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConnRecord {
    /// Wall-clock event time (ns since boot, from `bpf_ktime_get_ns`).
    pub ts_ns: u64,
    pub pid: u32,
    pub comm: [u8; 16],
    pub family: u8,
    /// Event kind: [`CONN_EVENT_CLOSE`] or [`CONN_EVENT_ESTABLISHED`].
    /// Repurposed from the first `_pad` byte (#304) — old producers zeroed
    /// it, and 0 == close, so the layout AND semantics stay compatible.
    pub event: u8,
    pub _pad: [u8; 2],
    pub saddr: [u8; 16],
    pub daddr: [u8; 16],
    pub sport: u16,
    pub dport: u16,
    /// Connection duration (ns), from the birth→close delta.
    pub duration_ns: u64,
    /// Bytes/segments/retransmits. Populated from `tcp_sock` via CO-RE in a
    /// follow-up (blind build leaves them 0 — see PR #114 checklist).
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub segs_out: u32,
    pub segs_in: u32,
    pub retrans: u32,
    pub _pad2: u32,
}

#[cfg(feature = "user")]
// SAFETY: both are `#[repr(C)]` plain-old-data with no padding-dependent
// invariants, valid to read as raw bytes from BPF maps / ring buffers.
unsafe impl aya::Pod for RetransKey {}
#[cfg(feature = "user")]
// SAFETY: see above.
unsafe impl aya::Pod for ConnRecord {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_edges_and_clamp() {
        assert_eq!(connlat_bucket(0), 0);
        assert_eq!(connlat_bucket(1), 1);
        assert_eq!(connlat_bucket(4), 3);
        assert_eq!(connlat_bucket(u64::MAX), CONNLAT_BUCKETS as u32 - 1);
    }

    /// Pins the `repr(C)` layout across the kernel/userspace boundary. The
    /// tier-2b `event` byte (#304) repurposes the first old `_pad` byte, so
    /// the record must stay byte-identical to the pre-#304 shape: any change
    /// here means kernel and userspace disagree on the wire format.
    #[test]
    fn conn_record_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};

        assert_eq!(size_of::<ConnRecord>(), 112);
        assert_eq!(align_of::<ConnRecord>(), 8);

        assert_eq!(offset_of!(ConnRecord, ts_ns), 0);
        assert_eq!(offset_of!(ConnRecord, pid), 8);
        assert_eq!(offset_of!(ConnRecord, comm), 12);
        assert_eq!(offset_of!(ConnRecord, family), 28);
        // `event` sits exactly where the first padding byte used to be.
        assert_eq!(offset_of!(ConnRecord, event), 29);
        assert_eq!(offset_of!(ConnRecord, _pad), 30);
        assert_eq!(offset_of!(ConnRecord, saddr), 32);
        assert_eq!(offset_of!(ConnRecord, daddr), 48);
        assert_eq!(offset_of!(ConnRecord, sport), 64);
        assert_eq!(offset_of!(ConnRecord, dport), 66);
        assert_eq!(offset_of!(ConnRecord, duration_ns), 72);
        assert_eq!(offset_of!(ConnRecord, retrans), 104);
    }
}
