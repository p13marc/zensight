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

/// A completed-connection record (tcplife), submitted to the ring buffer when a
/// socket transitions to `TCP_CLOSE`. Fixed-size, no heap — kernel-safe.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConnRecord {
    /// Wall-clock close time (ns since boot, from `bpf_ktime_get_ns`).
    pub ts_ns: u64,
    pub pid: u32,
    pub comm: [u8; 16],
    pub family: u8,
    pub _pad: [u8; 3],
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
}
