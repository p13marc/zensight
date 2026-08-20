//! eBPF kernel programs for the netlink connlat / retransmit / tcplife module
//! (#114).
//!
//! Two tracepoints, three data sources:
//! * **connlat** — `sock:inet_sock_set_state` stamps a start time when the
//!   socket leaves `CLOSE` for `SYN_SENT`, and measures the delta into a log2
//!   histogram (`CONNLAT_HIST`) when it reaches `ESTABLISHED`. That span is the
//!   handshake the caller actually waited out.
//! * **retransmits** — `tcp:tcp_retransmit_skb` increments a per-peer LRU
//!   counter (`RETRANS`, kernel-side bounded eviction; top-K done userspace).
//! * **tcplife** — the same `inet_sock_set_state` tracepoint stamps a birth
//!   time on the transition into `TCP_ESTABLISHED` and, on `TCP_CLOSE`, submits
//!   a `ConnRecord` (pid/comm/peer/duration) to a ring buffer.
//!
//! ## Status: validated on a host (#168)
//!
//! Loaded and measured on 6.12.101+deb13-cloud-amd64 (2026-08-19) — the first
//! time these programs had ever run. The verifier accepts them, both
//! tracepoints attach and fire, and the numbers were checked against
//! independent kernel counters. What that run found, and what it fixed:
//!
//! * **connlat used to measure the wrong interval, and now does not.** It sat
//!   on a kretprobe on `tcp_v4_connect()`, which builds and sends the SYN and
//!   returns — the handshake wait happens afterwards in `inet_stream_connect()`,
//!   and for a non-blocking socket there is no wait at all. Measured against a
//!   200 ms netem RTT, the histogram reported **16–64 µs**: a ~6000x
//!   understatement that never looks empty, it looks like a suspiciously fast
//!   network. Driving it off the state machine instead now tracks the real
//!   handshake at two independent delays (100 ms RTT → bucket 18 / 131–262 ms;
//!   5 ms RTT → bucket 14 / 8–16 ms), and refused connects no longer enter the
//!   histogram at all — they go `SYN_SENT → CLOSE` and never reach the
//!   measurement point, so they are excluded by construction rather than by
//!   inspecting a return value the kretprobe never checked.
//! * **pid/comm attribution was wrong a third of the time, and now is not.**
//!   Reading `current()` at ESTABLISHED/CLOSE picks up whatever task the
//!   SYN-ACK or FIN landed on, which is frequently a softirq. A 60-connection
//!   `curl` loop was attributed to `curl` in only 59 of 91 records — the rest
//!   went to `bash`, `python3`, `claude` and twice to **`ksoftirqd/1`**. The
//!   identity is now captured at `CLOSE → SYN_SENT`, which runs in the
//!   `connect(2)` syscall context, and replayed: 110/110 after the fix.
//!
//! The tracepoint field offsets below were validated against
//! `/sys/kernel/btf/vmlinux` on 7.1.3-200.fc44 (2026-07-16) and again on
//! 6.12.101+deb13-cloud-amd64 (2026-08-19) — every value holds on both. They
//! remain **kernel-version dependent**. ("Switch to CO-RE" is not available:
//! CO-RE field relocations come from clang's `__builtin_preserve_access_index`,
//! which rustc/bpf-linker do not emit. The portable fix would be
//! `aya::EbpfLoader::set_global` offset injection.)
//!
//! ## The one defect that remains
//!
//! * **tcp_sock byte/seg fields are left 0** pending the offset-injection work
//!   above, so `ConnRecord`'s `tx_bytes`/`rx_bytes`/`segs_*` are always zero —
//!   0 of 196 records carried a non-zero counter on the validation run. That
//!   reads as "idle connection", not "not measured".
#![cfg_attr(target_arch = "bpf", no_std)]
#![cfg_attr(target_arch = "bpf", no_main)]

// Host stub: the real programs only compile for the bpf target (aya-ebpf is
// bpf-only). Keeps the crate a normal workspace member (so `aya-build` resolves
// it by `--package`) while `cargo build --workspace` builds an empty binary.
#[cfg(not(target_arch = "bpf"))]
fn main() {}

#[cfg(target_arch = "bpf")]
mod prog {
    use aya_ebpf::{
        helpers::{bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns},
        macros::{map, tracepoint},
        maps::{LruHashMap, PerCpuArray, RingBuf},
        programs::TracePointContext,
    };
    use zensight_sensor_netlink_ebpf_common::{
        connlat_bucket, ConnRecord, ConnectStart, RetransKey, CONNLAT_BUCKETS, CONN_EVENT_CLOSE,
        CONN_EVENT_ESTABLISHED,
    };

    // -- connlat -------------------------------------------------------------
    // sk pointer -> what connect(2) stamped. LRU, not HashMap: a connect that
    // never resolves (dropped SYN to a black hole) would otherwise hold its
    // slot forever, and a plain HashMap answers a full map with E2BIG on
    // insert — which the `let _ =` swallows, so recording would stop silently.
    #[map]
    static CONNECT_START: LruHashMap<u64, ConnectStart> = LruHashMap::with_max_entries(10240, 0);
    #[map]
    static CONNLAT_HIST: PerCpuArray<u64> =
        PerCpuArray::with_max_entries(CONNLAT_BUCKETS as u32, 0);

    // -- retransmits (per-peer, LRU-evicted in kernel; top-K done userspace) --
    #[map]
    static RETRANS: LruHashMap<RetransKey, u64> = LruHashMap::with_max_entries(4096, 0);

    // -- tcplife -------------------------------------------------------------
    // skaddr -> birth timestamp AND the owner stamped at connect(2), so the
    // CLOSE record can name the process that opened the socket rather than
    // whichever task the FIN happened to land on. LRU for the same reason as
    // CONNECT_START: a socket that never closes must not wedge the map.
    #[map]
    static BIRTH: LruHashMap<u64, ConnectStart> = LruHashMap::with_max_entries(10240, 0);
    // Completed-connection records drained by userspace (256 KiB).
    #[map]
    static CONNS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

    const TCP_ESTABLISHED: i32 = 1;
    const TCP_SYN_SENT: i32 = 2;
    const TCP_CLOSE: i32 = 7;
    const AF_INET: u16 = 2;
    const AF_INET6: u16 = 10;
    // inet_sock_set_state is shared with DCCP and SCTP; only TCP belongs here.
    const IPPROTO_TCP: u16 = 6;

    // `inet_sock_set_state` field offsets, validated against
    // `struct trace_event_raw_inet_sock_set_state` (size 72) in
    // /sys/kernel/btf/vmlinux on 7.1.3-200.fc44 (2026-07-16). BTF member offsets
    // come from the same offsetof() the tracepoint's `format` file is generated
    // from, and BTF is world-readable where tracefs is 0700 — so it validates
    // unprivileged and in CI. See the `btf_offsets` test in -ebpf-common.
    //
    // The four address offsets were previously off by one (31/35/39/55): they
    // encoded an older layout where `protocol` was a __u8. It is a __u16 at
    // offset 30 here, so every field after it shifts up a byte. The bug was
    // invisible rather than loud — reading saddr at 31 yields [0x00, s0, s1, s2]
    // (protocol's high byte, little-endian), so 192.168.1.5 renders as
    // 0.192.168.1 and a plausible-looking address is published.
    const SS_SKADDR: usize = 8;
    const SS_OLDSTATE: usize = 16;
    const SS_NEWSTATE: usize = 20;
    const SS_SPORT: usize = 24;
    const SS_DPORT: usize = 26;
    const SS_PROTOCOL: usize = 30;
    const SS_FAMILY: usize = 28;
    const SS_SADDR_V4: usize = 32;
    const SS_DADDR_V4: usize = 36;
    const SS_SADDR_V6: usize = 40;
    const SS_DADDR_V6: usize = 56;

    // `tcp_retransmit_skb` field offsets, validated the same way against
    // `struct trace_event_raw_tcp_retransmit_skb` (size 80) — note this event has
    // its OWN struct rather than the shared `trace_event_raw_tcp_event_sk`
    // template (which is also in BTF, and disagrees: family@20, daddr@26). These
    // three happened to be right; they would have been badly wrong against the
    // template.
    const RT_FAMILY: usize = 32;
    const RT_DADDR_V4: usize = 38;
    const RT_DADDR_V6: usize = 58;

    // -- retransmit program --------------------------------------------------
    #[tracepoint]
    pub fn tcp_retransmit_skb(ctx: TracePointContext) -> u32 {
        try_retransmit(&ctx).unwrap_or(0)
    }

    fn try_retransmit(ctx: &TracePointContext) -> Result<u32, i64> {
        let family: u16 = unsafe { ctx.read_at(RT_FAMILY)? };
        let mut key = RetransKey {
            family: family as u8,
            _pad: [0; 3],
            addr: [0; 16],
        };
        if family == AF_INET6 {
            let a: [u8; 16] = unsafe { ctx.read_at(RT_DADDR_V6)? };
            key.addr = a;
        } else {
            let a: [u8; 4] = unsafe { ctx.read_at(RT_DADDR_V4)? };
            key.addr[..4].copy_from_slice(&a);
        }
        let next = unsafe { RETRANS.get(&key) }.map(|&c| c + 1).unwrap_or(1);
        let _ = RETRANS.insert(&key, &next, 0);
        Ok(0)
    }

    // -- tcplife program -----------------------------------------------------
    #[tracepoint]
    pub fn inet_sock_set_state(ctx: TracePointContext) -> u32 {
        try_set_state(&ctx).unwrap_or(0)
    }

    fn try_set_state(ctx: &TracePointContext) -> Result<u32, i64> {
        // This tracepoint is shared with DCCP and SCTP. Everything below reads
        // TCP state numbers, so anything else must be dropped before it can be
        // misinterpreted as a TCP transition.
        let protocol: u16 = unsafe { ctx.read_at(SS_PROTOCOL)? };
        if protocol != IPPROTO_TCP {
            return Ok(0);
        }
        let newstate: i32 = unsafe { ctx.read_at(SS_NEWSTATE)? };
        let oldstate: i32 = unsafe { ctx.read_at(SS_OLDSTATE)? };
        let skaddr: u64 = unsafe { ctx.read_at(SS_SKADDR)? };
        let now = unsafe { bpf_ktime_get_ns() };

        // (1) connect(2) starts here. tcp_v4_connect() calls
        //     tcp_set_state(SYN_SENT) before it sends anything, from the
        //     calling task's own syscall context — the only point in a client
        //     connection where current() is certainly the connecting process.
        //     Stash both the clock and the identity; everything downstream
        //     replays them instead of re-reading current().
        //
        //     Note the source port is NOT yet assigned here (inet_hash_connect
        //     runs after tcp_set_state), which is harmless because this joins
        //     on `sk` — but it is why a 4-tuple key would silently fail.
        if newstate == TCP_SYN_SENT {
            let start = ConnectStart {
                ts_ns: now,
                pid: (bpf_get_current_pid_tgid() >> 32) as u32,
                _pad: 0,
                comm: bpf_get_current_comm().unwrap_or([0u8; 16]),
            };
            let _ = CONNECT_START.insert(&skaddr, &start, 0);
            return Ok(0);
        }

        // (2) Leaving SYN_SENT, by either door. Take the stash unconditionally:
        //     a connect that succeeded is recorded below, one that failed is
        //     dropped, and neither leaves an entry behind.
        let started = if oldstate == TCP_SYN_SENT {
            let s = unsafe { CONNECT_START.get(&skaddr) }.copied();
            let _ = CONNECT_START.remove(&skaddr);
            s
        } else {
            None
        };

        if newstate == TCP_ESTABLISHED {
            if let Some(s) = started {
                // The handshake completed, so now - start is the round trip
                // the caller actually waited out. A refused or timed-out
                // connect goes SYN_SENT -> CLOSE and never arrives here, so
                // failures are excluded by construction rather than by
                // inspecting a return value.
                let us = now.saturating_sub(s.ts_ns) / 1_000;
                if let Some(slot) = CONNLAT_HIST.get_ptr_mut(connlat_bucket(us)) {
                    // SAFETY: idx < CONNLAT_BUCKETS; slot owned by this CPU.
                    unsafe {
                        *slot += 1;
                    }
                }
                let birth = ConnectStart { ts_ns: now, ..s };
                let _ = BIRTH.insert(&skaddr, &birth, 0);
                // Tier 2b (#304): live attribution for CLIENT connects only.
                emit_conn_record(ctx, CONN_EVENT_ESTABLISHED, now, 0, &s)?;
            } else {
                // SYN_RECV -> ESTABLISHED: an accepted inbound socket, in
                // softirq. There is no trustworthy owner to stash, so record
                // the birth alone and leave attribution to the /proc scan.
                let birth = ConnectStart {
                    ts_ns: now,
                    pid: 0,
                    _pad: 0,
                    comm: [0u8; 16],
                };
                let _ = BIRTH.insert(&skaddr, &birth, 0);
            }
            return Ok(0);
        }
        if newstate != TCP_CLOSE {
            return Ok(0);
        }

        // Close: emit a record if we saw this socket established, naming the
        // owner captured at connect(2) rather than whatever is on-CPU now.
        let birth = unsafe { BIRTH.get(&skaddr) }.copied();
        let _ = BIRTH.remove(&skaddr);
        let owner = birth.unwrap_or(ConnectStart {
            ts_ns: now,
            pid: 0,
            _pad: 0,
            comm: [0u8; 16],
        });
        let duration_ns = now.saturating_sub(owner.ts_ns);

        emit_conn_record(ctx, CONN_EVENT_CLOSE, now, duration_ns, &owner)?;
        Ok(0)
    }

    /// Build + submit one `ConnRecord` from the `inet_sock_set_state` context.
    /// Shared by the close (tcplife) and established (tier 2b, #304) paths.
    ///
    /// `pid`/`comm` are passed in rather than read from `current()`: both the
    /// SYN-ACK that completes a connect and the FIN that closes it are often
    /// processed in softirq, where `current()` is an unrelated task (#114).
    ///
    /// `owner` arrives by REFERENCE, and this is `#[inline(always)]`, because
    /// BPF passes at most five arguments in r1-r5 and has no r11 to spill to.
    /// Taking `comm: [u8; 16]` by value overflowed that and LLVM emitted a
    /// store through r11, which the verifier rejects outright:
    /// `359: (7b) *(u64 *)(r11 -8) = r1 / R11 is invalid`.
    #[inline(always)]
    fn emit_conn_record(
        ctx: &TracePointContext,
        event: u8,
        now: u64,
        duration_ns: u64,
        owner: &ConnectStart,
    ) -> Result<(), i64> {
        let family: u16 = unsafe { ctx.read_at(SS_FAMILY)? };
        let sport: u16 = unsafe { ctx.read_at(SS_SPORT)? };
        let dport: u16 = unsafe { ctx.read_at(SS_DPORT)? };

        let mut rec = ConnRecord {
            ts_ns: now,
            pid: owner.pid,
            comm: owner.comm,
            family: family as u8,
            event,
            _pad: [0; 2],
            saddr: [0; 16],
            daddr: [0; 16],
            sport,
            dport,
            duration_ns,
            // tcp_sock byte/seg fields need CO-RE; left 0 in the blind build.
            tx_bytes: 0,
            rx_bytes: 0,
            segs_out: 0,
            segs_in: 0,
            retrans: 0,
            _pad2: 0,
        };
        if family == AF_INET6 {
            rec.saddr = unsafe { ctx.read_at(SS_SADDR_V6)? };
            rec.daddr = unsafe { ctx.read_at(SS_DADDR_V6)? };
        } else if family == AF_INET {
            let s: [u8; 4] = unsafe { ctx.read_at(SS_SADDR_V4)? };
            let d: [u8; 4] = unsafe { ctx.read_at(SS_DADDR_V4)? };
            rec.saddr[..4].copy_from_slice(&s);
            rec.daddr[..4].copy_from_slice(&d);
        }

        if let Some(mut entry) = CONNS.reserve::<ConnRecord>(0) {
            entry.write(rec);
            entry.submit(0);
        }
        Ok(())
    }
}

#[cfg(target_arch = "bpf")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
