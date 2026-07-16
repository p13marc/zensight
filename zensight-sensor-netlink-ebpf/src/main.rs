//! eBPF kernel programs for the netlink connlat / retransmit / tcplife module
//! (#114).
//!
//! Three data sources:
//! * **connlat** — `tcp_v4_connect` / `tcp_v6_connect` kprobe stamps a start time
//!   keyed by the calling task (`pid_tgid`); the matching kretprobe measures the
//!   delta into a log2 histogram (`CONNLAT_HIST`). NOTE (R1): this measures the
//!   `connect()` *call path*, not the full SYN→SYN-ACK handshake; the canonical
//!   `tcpconnlat` finishes in `tcp_rcv_state_process`. Documented in the PR.
//! * **retransmits** — `tcp:tcp_retransmit_skb` tracepoint increments a per-peer
//!   LRU counter (`RETRANS`, kernel-side bounded eviction; top-K done userspace).
//! * **tcplife** — `sock:inet_sock_set_state` tracepoint stamps a birth time on
//!   the transition into `TCP_ESTABLISHED` and, on `TCP_CLOSE`, submits a
//!   `ConnRecord` (pid/comm/peer/duration) to a ring buffer.
//!
//! ## Status: offsets validated, connlat still wrong (#114)
//!
//! The tracepoint field offsets below were validated against
//! `/sys/kernel/btf/vmlinux` on 7.1.3-200.fc44 (2026-07-16) — four of them were
//! off by one and are now fixed; see the constants for the detail. They remain
//! **kernel-version dependent**, so the `btf_offsets` test in
//! `zensight-sensor-netlink-ebpf-common` re-checks them against the running
//! kernel's BTF. ("Switch to CO-RE", as an earlier note here suggested, is not
//! available: CO-RE field relocations come from clang's
//! `__builtin_preserve_access_index`, which rustc/bpf-linker do not emit. The
//! portable fix would be `aya::EbpfLoader::set_global` offset injection.)
//!
//! Two known defects remain, which is why `collect.ebpf` stays off for netlink
//! while sysinfo's is on:
//!
//! * **connlat measures the wrong thing.** `tcp_v4_connect()` builds and sends
//!   the SYN and returns immediately — the handshake wait happens in
//!   `inet_stream_connect()` afterwards, and for a non-blocking socket there is
//!   no wait at all. So the kretprobe delta is SYN-construction time: a few µs
//!   regardless of whether the peer is on loopback or another continent. Unlike
//!   a bad offset this never looks empty, it looks like a suspiciously fast
//!   network. The fix is to drive connlat off `inet_sock_set_state` (stash on
//!   CLOSE→SYN_SENT, which runs in `connect(2)` syscall context; measure on
//!   →ESTABLISHED), which also fixes the pid attribution noted at `try_set_state`
//!   and needs no new offsets.
//! * **tcp_sock byte/seg fields are left 0** pending CO-RE, so `ConnRecord`'s
//!   `tx_bytes`/`rx_bytes`/`segs_*` are always zero — which reads as "idle
//!   connection", not "not measured".
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
        macros::{kprobe, kretprobe, map, tracepoint},
        maps::{HashMap, LruHashMap, PerCpuArray, RingBuf},
        programs::{ProbeContext, RetProbeContext, TracePointContext},
    };
    use zensight_sensor_netlink_ebpf_common::{
        connlat_bucket, ConnRecord, RetransKey, CONNLAT_BUCKETS, CONN_EVENT_CLOSE,
        CONN_EVENT_ESTABLISHED,
    };

    // -- connlat -------------------------------------------------------------
    // pid_tgid -> connect() entry timestamp (ns)
    #[map]
    static CONNECT_START: HashMap<u64, u64> = HashMap::with_max_entries(10240, 0);
    #[map]
    static CONNLAT_HIST: PerCpuArray<u64> =
        PerCpuArray::with_max_entries(CONNLAT_BUCKETS as u32, 0);

    // -- retransmits (per-peer, LRU-evicted in kernel; top-K done userspace) --
    #[map]
    static RETRANS: LruHashMap<RetransKey, u64> = LruHashMap::with_max_entries(4096, 0);

    // -- tcplife -------------------------------------------------------------
    // skaddr -> birth timestamp (ns), set on transition into ESTABLISHED.
    #[map]
    static BIRTH: HashMap<u64, u64> = HashMap::with_max_entries(10240, 0);
    // Completed-connection records drained by userspace (256 KiB).
    #[map]
    static CONNS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

    const TCP_ESTABLISHED: i32 = 1;
    const TCP_SYN_SENT: i32 = 2;
    const TCP_CLOSE: i32 = 7;
    const AF_INET: u16 = 2;
    const AF_INET6: u16 = 10;

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

    // -- connlat programs ----------------------------------------------------
    #[kprobe]
    pub fn tcp_v4_connect(_ctx: ProbeContext) -> u32 {
        connect_enter()
    }
    #[kprobe]
    pub fn tcp_v6_connect(_ctx: ProbeContext) -> u32 {
        connect_enter()
    }

    fn connect_enter() -> u32 {
        let pid_tgid = bpf_get_current_pid_tgid();
        let now = unsafe { bpf_ktime_get_ns() };
        let _ = CONNECT_START.insert(&pid_tgid, &now, 0);
        0
    }

    #[kretprobe]
    pub fn tcp_v4_connect_ret(_ctx: RetProbeContext) -> u32 {
        connect_return()
    }
    #[kretprobe]
    pub fn tcp_v6_connect_ret(_ctx: RetProbeContext) -> u32 {
        connect_return()
    }

    fn connect_return() -> u32 {
        let pid_tgid = bpf_get_current_pid_tgid();
        // SAFETY: pointer from a successful map lookup.
        if let Some(&start) = unsafe { CONNECT_START.get(&pid_tgid) } {
            let now = unsafe { bpf_ktime_get_ns() };
            let us = now.saturating_sub(start) / 1_000;
            let idx = connlat_bucket(us);
            if let Some(slot) = CONNLAT_HIST.get_ptr_mut(idx) {
                // SAFETY: idx < CONNLAT_BUCKETS; slot owned by this CPU.
                unsafe {
                    *slot += 1;
                }
            }
            let _ = CONNECT_START.remove(&pid_tgid);
        }
        0
    }

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
        let newstate: i32 = unsafe { ctx.read_at(SS_NEWSTATE)? };
        let oldstate: i32 = unsafe { ctx.read_at(SS_OLDSTATE)? };
        let skaddr: u64 = unsafe { ctx.read_at(SS_SKADDR)? };

        if newstate == TCP_ESTABLISHED {
            let now = unsafe { bpf_ktime_get_ns() };
            let _ = BIRTH.insert(&skaddr, &now, 0);
            // Tier 2b (#304): emit a live-attribution record for CLIENT-side
            // connects only (SYN_SENT → ESTABLISHED). Inbound/accepted sockets
            // (SYN_RECV → ESTABLISHED) are deliberately excluded: that
            // transition fires in softirq context, where
            // bpf_get_current_pid_tgid() is whatever task happens to be
            // running — NOT the owning process. Server sockets stay on the
            // /proc-scan path. CAVEAT (needs host validation, like #114): the
            // SYN-ACK that completes a client connect is ALSO processed in
            // softirq; whether current() here is reliably the connecting task
            // must be verified on a real kernel — if not, the hardened variant
            // is to stash pid/comm at the CLOSE → SYN_SENT transition (which
            // runs in the connect(2) syscall context) and emit that identity.
            if oldstate == TCP_SYN_SENT {
                emit_conn_record(ctx, CONN_EVENT_ESTABLISHED, now, 0)?;
            }
            return Ok(0);
        }
        if newstate != TCP_CLOSE {
            return Ok(0);
        }

        // Close: emit a record if we saw this socket established.
        let now = unsafe { bpf_ktime_get_ns() };
        let birth = unsafe { BIRTH.get(&skaddr) }.copied();
        let duration_ns = birth.map(|b| now.saturating_sub(b)).unwrap_or(0);
        let _ = BIRTH.remove(&skaddr);

        emit_conn_record(ctx, CONN_EVENT_CLOSE, now, duration_ns)?;
        Ok(0)
    }

    /// Build + submit one `ConnRecord` from the `inet_sock_set_state` context.
    /// Shared by the close (tcplife) and established (tier 2b, #304) paths.
    fn emit_conn_record(
        ctx: &TracePointContext,
        event: u8,
        now: u64,
        duration_ns: u64,
    ) -> Result<(), i64> {
        let family: u16 = unsafe { ctx.read_at(SS_FAMILY)? };
        let sport: u16 = unsafe { ctx.read_at(SS_SPORT)? };
        let dport: u16 = unsafe { ctx.read_at(SS_DPORT)? };
        let pid = (bpf_get_current_pid_tgid() >> 32) as u32;
        let comm = bpf_get_current_comm().unwrap_or([0u8; 16]);

        let mut rec = ConnRecord {
            ts_ns: now,
            pid,
            comm,
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
