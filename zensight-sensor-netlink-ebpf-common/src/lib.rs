//! Shared types/constants for the netlink eBPF module (#114).
//!
//! Used by both the kernel programs (`zensight-sensor-netlink-ebpf`, no_std,
//! `bpfel-unknown-none`) and the userspace loader (`zensight-sensor-netlink`
//! `ebpf` feature). All record types are `#[repr(C)]` POD so they cross the
//! kernel/userspace boundary (ring buffer / hash map) byte-for-byte.
#![cfg_attr(not(test), no_std)]

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

// ── Tracepoint field offsets ────────────────────────────────────────────────
//
// These live here rather than in the program crate for the reason sysinfo's do:
// that crate only compiles for the `bpf` target, so a constant declared inside
// it is invisible to a host test. `btf_offsets_match_this_kernel` below is that
// test, and #682 is the issue where it turned out two comments had been
// promising it for a while without it existing.
//
// Getting one wrong raises no error — it reads the wrong field and publishes a
// plausible-looking lie. The four address offsets here were once off by one
// (31/35/39/55), encoding an older layout where `protocol` was a `__u8`; it is a
// `__u16` at 30, so everything after it shifted a byte. Reading `saddr` at 31
// yields [0x00, s0, s1, s2] little-endian, so 192.168.1.5 published as
// 0.192.168.1 — wrong, and entirely believable.

/// `inet_sock_set_state` field offsets, against
/// [`SS_STRUCT`] (size [`SS_SIZE`]).
pub const SS_SKADDR: usize = 8;
/// See [`SS_SKADDR`].
pub const SS_OLDSTATE: usize = 16;
/// See [`SS_SKADDR`].
pub const SS_NEWSTATE: usize = 20;
/// See [`SS_SKADDR`].
pub const SS_SPORT: usize = 24;
/// See [`SS_SKADDR`].
pub const SS_DPORT: usize = 26;
/// See [`SS_SKADDR`].
pub const SS_FAMILY: usize = 28;
/// `protocol` is a `__u16` here; see [`SS_SKADDR`] for why that matters.
pub const SS_PROTOCOL: usize = 30;
/// See [`SS_SKADDR`].
pub const SS_SADDR_V4: usize = 32;
/// See [`SS_SKADDR`].
pub const SS_DADDR_V4: usize = 36;
/// See [`SS_SKADDR`].
pub const SS_SADDR_V6: usize = 40;
/// See [`SS_SKADDR`].
pub const SS_DADDR_V6: usize = 56;

/// The struct `inet_sock_set_state`'s offsets are read from.
pub const SS_STRUCT: &str = "trace_event_raw_inet_sock_set_state";
/// Its size, asserted alongside the offsets: a same-name struct of a different
/// size is a layout that moved wholesale.
pub const SS_SIZE: usize = 72;

/// `tcp_retransmit_skb` field offsets, against the first of [`RT_STRUCTS`] this
/// kernel has (size [`RT_SIZE`]).
pub const RT_FAMILY: usize = 32;
/// See [`RT_FAMILY`].
pub const RT_DADDR_V4: usize = 38;
/// See [`RT_FAMILY`].
pub const RT_DADDR_V6: usize = 58;

/// Candidate struct names for `tcp:tcp_retransmit_skb`, most likely first.
///
/// A comment here used to name `trace_event_raw_tcp_retransmit_skb` and say the
/// event "has its OWN struct rather than the shared template". That is wrong on
/// the kernels checked: the name is **not in BTF at all**, because
/// `tcp:tcp_retransmit_skb` is a `DEFINE_EVENT` of the `tcp_event_sk_skb` class
/// and so resolves to `trace_event_raw_tcp_event_sk_skb`. The right struct was
/// read and the wrong name written down (#682).
///
/// The correction is a list rather than a different single name, because the
/// real portability hazard is exactly this: a tracepoint changing event class
/// renames the struct and moves every field at once. A candidate list survives
/// that; one hardcoded name cannot.
///
/// Still true, and worth keeping: `trace_event_raw_tcp_event_sk` (no `_skb`) is
/// a *different* struct that is also in BTF and disagrees — family@20,
/// daddr@26 — so it must never be added here.
pub const RT_STRUCTS: [&str; 2] = [
    "trace_event_raw_tcp_event_sk_skb",
    "trace_event_raw_tcp_retransmit_skb",
];
/// Size shared by both [`RT_STRUCTS`] candidates.
pub const RT_SIZE: usize = 80;

/// `ConnRecord.event`: the connection closed (tcplife record).
pub const CONN_EVENT_CLOSE: u8 = 0;
/// `ConnRecord.event`: the connection reached ESTABLISHED (client-side
/// connect) — live-attribution record (#304, tier 2b).
pub const CONN_EVENT_ESTABLISHED: u8 = 1;

/// What `connect(2)` stamped when the socket left `CLOSE` for `SYN_SENT`.
///
/// `tcp_v4_connect()` calls `tcp_set_state(sk, TCP_SYN_SENT)` in **syscall
/// context**, so this is the one moment in a client connection's life where
/// `bpf_get_current_pid_tgid()` / `bpf_get_current_comm()` are certain to be
/// the connecting task. The SYN-ACK that completes the handshake, and the
/// FIN/RST that closes the socket, are both frequently processed in softirq —
/// measured on a real host, 32 of 91 records for a single `curl` loop were
/// attributed to `bash`, `python3`, `claude` and twice to `ksoftirqd/1` (#114).
///
/// So the identity is captured here and replayed at ESTABLISHED and CLOSE,
/// and the timestamp doubles as the connect-latency start. Keyed by `sk`
/// pointer, which is the only identifier both ends of that join share.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConnectStart {
    /// `bpf_ktime_get_ns()` at the CLOSE -> SYN_SENT transition.
    pub ts_ns: u64,
    /// TGID of the task that called `connect(2)`.
    pub pid: u32,
    pub _pad: u32,
    /// `comm` of that task.
    pub comm: [u8; 16],
}

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

    /// The hardcoded tracepoint offsets must match the kernel we are running on.
    ///
    /// Getting one wrong does not raise an error — it silently reads the wrong
    /// field and publishes a plausible-looking lie (see the note on
    /// [`SS_SKADDR`] for the time that actually happened). That is the whole
    /// reason this test exists rather than a comment saying "verify these".
    ///
    /// Two comments in `zensight-sensor-netlink-ebpf` promised this test long
    /// before it existed (#682); it is the sysinfo guard, ported.
    ///
    /// Not `#[ignore]`d: fast, needs no privilege, and a failure here is real
    /// news on any kernel. It skips only where BTF genuinely is not exposed
    /// (non-Linux, or a kernel built without `CONFIG_DEBUG_INFO_BTF`).
    #[test]
    fn btf_offsets_match_this_kernel() {
        let Some(blob) = zensight_btf::read_vmlinux() else {
            eprintln!(
                "skipping: {} unreadable (no CONFIG_DEBUG_INFO_BTF?)",
                zensight_btf::VMLINUX_PATH
            );
            return;
        };

        assert_eq!(
            zensight_btf::struct_size(&blob, SS_STRUCT),
            Some(SS_SIZE),
            "{SS_STRUCT} is a different size on this kernel — every offset below \
             is suspect, not just the one that would fail next"
        );

        let ss: &[(&str, usize)] = &[
            ("skaddr", SS_SKADDR),
            ("oldstate", SS_OLDSTATE),
            ("newstate", SS_NEWSTATE),
            ("sport", SS_SPORT),
            ("dport", SS_DPORT),
            ("family", SS_FAMILY),
            ("protocol", SS_PROTOCOL),
            ("saddr", SS_SADDR_V4),
            ("daddr", SS_DADDR_V4),
            ("saddr_v6", SS_SADDR_V6),
            ("daddr_v6", SS_DADDR_V6),
        ];
        for (member, ours) in ss {
            let truth = zensight_btf::member_offset(&blob, SS_STRUCT, member)
                .unwrap_or_else(|| panic!("BTF has no {SS_STRUCT}.{member} — the ABI moved"));
            assert_eq!(
                truth, *ours,
                "{SS_STRUCT}.{member} is at {truth} on this kernel, but we hardcode \
                 {ours} — the sensor would publish the wrong field. Update the constant."
            );
        }

        // Resolved against a candidate list: `tcp:tcp_retransmit_skb` is a
        // DEFINE_EVENT whose class — and therefore whose struct name — differs
        // across kernels. Whichever one this kernel has must agree.
        let rt: &[(&str, usize)] = &[
            ("family", RT_FAMILY),
            ("daddr", RT_DADDR_V4),
            ("daddr_v6", RT_DADDR_V6),
        ];
        let mut matched: Option<usize> = None;
        for (member, ours) in rt {
            let (index, truth) = zensight_btf::member_offset_of_any(&blob, &RT_STRUCTS, member)
                .unwrap_or_else(|| {
                    panic!(
                        "none of {RT_STRUCTS:?} has a `{member}` on this kernel — \
                         tcp_retransmit_skb changed event class again; add the new name"
                    )
                });
            assert_eq!(
                truth, *ours,
                "{}.{member} is at {truth} on this kernel, but we hardcode {ours}",
                RT_STRUCTS[index]
            );
            // Every field must come from the SAME struct, or the offsets are a
            // mix of two layouts and each one looking right proves nothing.
            match matched {
                None => matched = Some(index),
                Some(first) => assert_eq!(
                    index, first,
                    "`{member}` resolved against {} but earlier fields against {} — \
                     the offsets would be a blend of two layouts",
                    RT_STRUCTS[index], RT_STRUCTS[first]
                ),
            }
        }
        let index = matched.expect("the loop above asserts at least once");
        assert_eq!(
            zensight_btf::struct_size(&blob, RT_STRUCTS[index]),
            Some(RT_SIZE),
            "{} is a different size on this kernel",
            RT_STRUCTS[index]
        );
    }

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
