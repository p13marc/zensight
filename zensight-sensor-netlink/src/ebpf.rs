//! Userspace side of the opt-in eBPF module (#114).
//!
//! Loads the bytecode compiled by `build.rs`, attaches the connlat kprobes +
//! the retransmit / tcplife tracepoints, drains the connection ring buffer into
//! a bounded in-memory ring, and exposes readers for the collector (connlat
//! gauges) and the `@rpc/netlink/{retransmits,connections}` channels.
//!
//! Gated on `feature = "ebpf"` — the rest of the crate stays aya-free. Any
//! load/attach failure (no `CAP_BPF`/`CAP_PERFMON`, unsupported kernel) is
//! returned as an `Err`; the caller logs one warning and the unprivileged
//! baseline is untouched.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    EbpfLoader,
    // The userspace handle for a kernel LRU_HASH map is `HashMap` (aya has no
    // separate userspace `LruHashMap`); its TryFrom accepts the LRU variant.
    maps::{HashMap as AyaHashMap, MapData, PerCpuArray, RingBuf},
    programs::TracePoint,
};
use tokio::io::unix::AsyncFd;
use zensight_sensor_netlink_ebpf_common::{
    CONN_EVENT_ESTABLISHED, CONNLAT_BUCKETS, ConnRecord, RetransKey, TCP_SOCK_MEMBERS,
    TcpSockOffsets,
};

use crate::map::{
    ConnectionRecord, RetransmitRecord, conn_record_view, connlat_percentiles, top_k_retransmits,
};

/// TCP 4-tuple key for the recent-close map (#304): (local ip, local port,
/// remote ip, remote port). Addresses are the `Ipv4Addr`/`Ipv6Addr` `Display`
/// strings — the same formatting `map::fmt_addr` (kernel record side) and
/// `SocketAddr::ip().to_string()` (sockdiag side) produce, so lookups join.
type ConnKey = (String, u16, String, u16);

/// Cap on the recent-close map — bounds memory on churn-heavy hosts.
const CLOSE_MAP_CAP: usize = 16384;
/// Entries older than this are swept; comfortably covers the 60 s TIME_WAIT
/// window the map exists to attribute.
const CLOSE_TTL: Duration = Duration::from_secs(120);
/// Cap on the live-connection map (#304, tier 2b).
const LIVE_MAP_CAP: usize = 16384;
/// Live entries whose close event was missed (ring overrun, load between
/// events) must not leak forever; the trade-off is that a connection living
/// longer than this loses its live-map attribution (the /proc scan usually
/// still covers it — the live map only serves sockets that scan missed).
const LIVE_TTL: Duration = Duration::from_secs(6 * 3600);

/// Bounded connection → owner map keyed by 4-tuple (#304).
///
/// Two instances live in [`EbpfState`]:
/// * `closed` (tier 2a) — fed at TCP_CLOSE from the tcplife ring; consulted by
///   `@rpc/netlink/sockets` for closing-state sockets (TIME_WAIT/CLOSE_WAIT/
///   FIN_WAIT*/LAST_ACK/CLOSING) that the `/proc` fd scan can never attribute —
///   their fd is already gone by the time the socket lingers in those states.
/// * `live` (tier 2b) — fed at client-connect ESTABLISHED, removed at close;
///   consulted for established sockets the `/proc` scan missed (e.g. other
///   users' processes when the sensor runs unprivileged).
///
/// Standalone (no kernel maps) so the eviction/TTL logic is unit-testable.
struct OwnerMap {
    map: Mutex<HashMap<ConnKey, (u32, String, Instant)>>,
    cap: usize,
    ttl: Duration,
}

impl OwnerMap {
    fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            cap,
            ttl,
        }
    }

    /// Record who owned a just-closed connection, sweeping expired entries and
    /// keeping the map bounded.
    fn note(&self, v: &ConnectionRecord) {
        let Ok(mut map) = self.map.lock() else {
            return;
        };
        let now = Instant::now();
        if map.len() >= self.cap {
            let ttl = self.ttl;
            map.retain(|_, (_, _, ts)| now.duration_since(*ts) < ttl);
            if map.len() >= self.cap {
                // Still full after the sweep (churn burst): drop one arbitrary
                // entry rather than grow unbounded.
                if let Some(k) = map.keys().next().cloned() {
                    map.remove(&k);
                }
            }
        }
        map.insert(
            (v.local.clone(), v.lport, v.remote.clone(), v.rport),
            (v.pid, v.comm.clone(), now),
        );
    }

    /// Forget a connection (its close event arrived — tier 2b live map).
    fn remove(&self, v: &ConnectionRecord) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(&(v.local.clone(), v.lport, v.remote.clone(), v.rport));
        }
    }

    /// `(pid, comm)` of a connection noted within the TTL.
    fn owner(
        &self,
        local_ip: &str,
        lport: u16,
        remote_ip: &str,
        rport: u16,
    ) -> Option<(u32, String)> {
        let map = self.map.lock().ok()?;
        let key = (local_ip.to_string(), lport, remote_ip.to_string(), rport);
        map.get(&key)
            .filter(|(_, _, ts)| ts.elapsed() < self.ttl)
            .map(|(pid, comm, _)| (*pid, comm.clone()))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// Shared, clonable handle to the eBPF-derived state (mirrors `EventState`).
#[derive(Clone)]
pub struct EbpfState {
    inner: Arc<Inner>,
}

struct Inner {
    conns: Mutex<VecDeque<ConnectionRecord>>,
    conn_cap: usize,
    retrans: Mutex<AyaHashMap<MapData, RetransKey, u64>>,
    connlat: Mutex<PerCpuArray<MapData, u64>>,
    connlat_prev: Mutex<[u64; CONNLAT_BUCKETS]>,
    /// Recently-closed connections → owning (pid, comm) (#304, tier 2a).
    closed: OwnerMap,
    /// Live client connections → owning (pid, comm) (#304, tier 2b): inserted
    /// on an ESTABLISHED event, removed (and pushed to `closed`) at close.
    live: OwnerMap,
}

impl EbpfState {
    /// Route one drained kernel record (#304): ESTABLISHED events feed the
    /// live map (tier 2b); close events retire the live entry, feed the
    /// recent-close map (tier 2a) and land in the tcplife ring.
    fn ingest(&self, rec: &ConnRecord, anchor_ms: i64) {
        let v = conn_record_view(rec, anchor_ms);
        if rec.event == CONN_EVENT_ESTABLISHED {
            self.inner.live.note(&v);
        } else {
            self.inner.live.remove(&v);
            self.push_conn(v);
        }
    }

    /// Push a completed-connection record, dropping the oldest past capacity,
    /// and remember its owner in the recent-close map (#304).
    fn push_conn(&self, v: ConnectionRecord) {
        self.inner.closed.note(&v);
        if let Ok(mut q) = self.inner.conns.lock() {
            if q.len() == self.inner.conn_cap {
                q.pop_front();
            }
            q.push_back(v);
        }
    }

    /// Owner of a recently-closed connection by 4-tuple, if the tcplife ring
    /// saw it close within the TTL (#304, tier 2a). Returns `(pid, comm)`.
    pub fn recent_close_owner(
        &self,
        local_ip: &str,
        lport: u16,
        remote_ip: &str,
        rport: u16,
    ) -> Option<(u32, String)> {
        self.inner.closed.owner(local_ip, lport, remote_ip, rport)
    }

    /// Owner of a live client connection by 4-tuple, if an ESTABLISHED event
    /// was seen and no close has retired it (#304, tier 2b). `(pid, comm)`.
    pub fn live_conn_owner(
        &self,
        local_ip: &str,
        lport: u16,
        remote_ip: &str,
        rport: u16,
    ) -> Option<(u32, String)> {
        self.inner.live.owner(local_ip, lport, remote_ip, rport)
    }

    /// Recent completed-connection records (oldest first), for `@rpc/netlink/connections`.
    pub fn recent_connections(&self) -> Vec<ConnectionRecord> {
        self.inner
            .conns
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Top-K retransmit peers, for `@rpc/netlink/retransmits`.
    pub fn top_retransmits(&self, k: usize) -> Vec<RetransmitRecord> {
        let snapshot: Vec<(RetransKey, u64)> = match self.inner.retrans.lock() {
            Ok(map) => map.iter().filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        };
        top_k_retransmits(&snapshot, k)
    }

    /// Windowed connect-latency p50/p95 (µs) since the last call.
    pub fn read_connlat(&self) -> (u64, u64) {
        let mut cur = [0u64; CONNLAT_BUCKETS];
        if let Ok(arr) = self.inner.connlat.lock() {
            for (i, slot) in cur.iter_mut().enumerate() {
                if let Ok(vals) = arr.get(&(i as u32), 0) {
                    *slot = vals.iter().copied().sum();
                }
            }
        }
        let mut delta = [0u64; CONNLAT_BUCKETS];
        if let Ok(mut prev) = self.inner.connlat_prev.lock() {
            for i in 0..CONNLAT_BUCKETS {
                delta[i] = cur[i].saturating_sub(prev[i]);
            }
            *prev = cur;
        }
        connlat_percentiles(&delta)
    }
}

/// Load + attach the eBPF programs. Returns the live `Ebpf` (keep it alive for
/// the process lifetime — drop = detach), the shared state, and the connection
/// ring buffer to be drained by [`drain_ring`].
pub fn load(conn_ring_capacity: usize) -> Result<(Ebpf, EbpfState, RingBuf<MapData>)> {
    bump_memlock();

    let mut loader = EbpfLoader::new();
    let offsets = resolve_tcp_sock_offsets();
    // `must_exist: true` on purpose. If a future bpf-linker stops emitting this
    // symbol, that must be a loud load failure — the alternative is every byte
    // counter silently reading 0, which is exactly the state #681 exists to fix
    // and is indistinguishable from an idle host.
    let offset_bytes = offsets_to_bytes(&offsets);
    let mut bpf = loader
        .set_global("TCP_SOCK_OFFSETS", &offset_bytes[..], true)
        .load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            // The bin-target name of the kernel crate (differs from its package
            // name to dodge an aya-build target-dir/artifact path collision).
            "/zensight-netlink-ebpf-prog"
        )))
        .context("load eBPF bytecode")?;

    if let Err(e) = aya_log::EbpfLogger::init(&mut bpf) {
        tracing::debug!(error = %e, "eBPF: aya-log init skipped");
    }

    // Two tracepoints, and that is the whole attach surface: connlat used to
    // need four kprobes on tcp_v[46]_connect, and now rides the state machine
    // inet_sock_set_state already reports (#114). One fewer failure mode too —
    // a kernel with IPv6 unbuilt has no tcp_v6_connect symbol, which used to
    // abort the entire load.
    attach_tp(&mut bpf, "tcp_retransmit_skb", "tcp", "tcp_retransmit_skb")?;
    attach_tp(
        &mut bpf,
        "inet_sock_set_state",
        "sock",
        "inet_sock_set_state",
    )?;

    let retrans: AyaHashMap<MapData, RetransKey, u64> =
        AyaHashMap::try_from(bpf.take_map("RETRANS").context("RETRANS map missing")?)?;
    let connlat: PerCpuArray<MapData, u64> = PerCpuArray::try_from(
        bpf.take_map("CONNLAT_HIST")
            .context("CONNLAT_HIST map missing")?,
    )?;
    let ring: RingBuf<MapData> =
        RingBuf::try_from(bpf.take_map("CONNS").context("CONNS map missing")?)?;

    let state = EbpfState {
        inner: Arc::new(Inner {
            conns: Mutex::new(VecDeque::with_capacity(conn_ring_capacity)),
            conn_cap: conn_ring_capacity.max(1),
            retrans: Mutex::new(retrans),
            connlat: Mutex::new(connlat),
            connlat_prev: Mutex::new([0u64; CONNLAT_BUCKETS]),
            closed: OwnerMap::new(CLOSE_MAP_CAP, CLOSE_TTL),
            live: OwnerMap::new(LIVE_MAP_CAP, LIVE_TTL),
        }),
    };
    Ok((bpf, state, ring))
}

/// Resolve this kernel's `struct tcp_sock` counter offsets from its own BTF.
///
/// CO-RE would normally do this at load time; it is unavailable to us because
/// its relocations come from clang's `__builtin_preserve_access_index`, which
/// rustc/bpf-linker do not emit (#681). BTF is the same `offsetof` the kernel
/// was built with, and `/sys/kernel/btf/vmlinux` is world-readable, so this
/// needs no privilege beyond what loading already requires.
///
/// Any failure yields `valid: 0` and one warning naming what was missing. The
/// kernel side then leaves the counters at 0, which the wire type defines as
/// "not measured" — a documented absence rather than a plausible lie.
#[cfg(feature = "ebpf")]
fn resolve_tcp_sock_offsets() -> TcpSockOffsets {
    let Some(blob) = zensight_btf::read_vmlinux() else {
        tracing::warn!(
            path = zensight_btf::VMLINUX_PATH,
            "eBPF: no kernel BTF; tcplife byte/segment counters stay unmeasured (#681)"
        );
        return TcpSockOffsets::default();
    };

    let mut resolved = [0u32; TCP_SOCK_MEMBERS.len()];
    for (slot, member) in resolved.iter_mut().zip(TCP_SOCK_MEMBERS) {
        match zensight_btf::member_offset(&blob, "tcp_sock", member) {
            Some(off) => *slot = off as u32,
            None => {
                // Naming the member matters: "tcp_sock changed" is a kernel
                // bump, and the field that moved is the whole diagnosis.
                tracing::warn!(
                    member,
                    "eBPF: tcp_sock.{member} not in this kernel's BTF; \
                     tcplife byte/segment counters stay unmeasured (#681)"
                );
                return TcpSockOffsets::default();
            }
        }
    }

    let offsets = TcpSockOffsets {
        valid: 1,
        bytes_acked: resolved[0],
        bytes_received: resolved[1],
        segs_out: resolved[2],
        segs_in: resolved[3],
        total_retrans: resolved[4],
    };
    // The line a future kernel bump gets diagnosed from.
    tracing::debug!(
        bytes_acked = offsets.bytes_acked,
        bytes_received = offsets.bytes_received,
        segs_out = offsets.segs_out,
        segs_in = offsets.segs_in,
        total_retrans = offsets.total_retrans,
        "eBPF: resolved tcp_sock offsets from BTF"
    );
    offsets
}

/// `TcpSockOffsets` as the `.rodata` bytes `set_global` patches in.
///
/// `aya-obj` requires the byte length to equal the symbol's size exactly, which
/// is what `tcp_sock_offsets_layout_is_stable` pins on the other side.
#[cfg(feature = "ebpf")]
fn offsets_to_bytes(o: &TcpSockOffsets) -> [u8; core::mem::size_of::<TcpSockOffsets>()] {
    let mut out = [0u8; core::mem::size_of::<TcpSockOffsets>()];
    for (chunk, value) in out.chunks_exact_mut(4).zip([
        o.valid,
        o.bytes_acked,
        o.bytes_received,
        o.segs_out,
        o.segs_in,
        o.total_retrans,
    ]) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    out
}

/// Epoch milliseconds at this host's boot, for converting the kernel's
/// boot-relative `bpf_ktime_get_ns()` stamps (#685).
///
/// `CLOCK_MONOTONIC` deliberately: `bpf_ktime_get_ns` excludes suspended time,
/// and `CLOCK_BOOTTIME` includes it, so the pair must not be mixed. Recomputed
/// per drain wakeup rather than cached at load, so a sensor that runs for weeks
/// does not accumulate the drift between the two clocks.
///
/// Falls back to 0 if `clock_gettime` fails, which yields boot-relative
/// timestamps rather than wrong-but-plausible ones.
fn monotonic_anchor_ms() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC with a valid timespec out-pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    let mono_ms = (ts.tv_sec as i64) * 1_000 + (ts.tv_nsec as i64) / 1_000_000;
    zensight_common::telemetry::current_timestamp_millis().saturating_sub(mono_ms)
}

/// Drain the connection ring buffer into the bounded in-memory ring until the
/// fd closes. Best-effort: malformed/short records are skipped.
pub async fn drain_ring(ring: RingBuf<MapData>, state: EbpfState) {
    let mut afd = match AsyncFd::new(ring) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "eBPF: ring buffer AsyncFd failed");
            return;
        }
    };
    let rec_size = std::mem::size_of::<ConnRecord>();
    loop {
        let mut guard = match afd.readable_mut().await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "eBPF: ring buffer poll failed");
                return;
            }
        };
        // One `clock_gettime` per wakeup, not per record: every record drained
        // in this batch is within milliseconds of the others.
        let anchor_ms = monotonic_anchor_ms();
        let ring = guard.get_inner_mut();
        while let Some(item) = ring.next() {
            if item.len() >= rec_size {
                // SAFETY: ConnRecord is repr(C) POD; the kernel reserved exactly
                // this layout. read_unaligned tolerates ring-buffer alignment.
                let rec = unsafe { std::ptr::read_unaligned(item.as_ptr() as *const ConnRecord) };
                state.ingest(&rec, anchor_ms);
            }
        }
        guard.clear_ready();
    }
}

fn attach_tp(bpf: &mut Ebpf, prog: &str, category: &str, name: &str) -> Result<()> {
    let p: &mut TracePoint = bpf
        .program_mut(prog)
        .with_context(|| format!("program {prog} missing"))?
        .try_into()
        .with_context(|| format!("program {prog} is not a tracepoint"))?;
    p.load().with_context(|| format!("load {prog}"))?;
    p.attach(category, name)
        .with_context(|| format!("attach {category}/{name}"))?;
    Ok(())
}

fn bump_memlock() {
    let lim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: setrlimit with a valid rlimit pointer; failure is ignored.
    unsafe {
        libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim);
    }
}

#[cfg(test)]
mod tests {

    /// The symbol `set_global` patches must exist in the built object, at
    /// exactly the size we hand it.
    ///
    /// This test exists because I got this wrong by hand: I concluded
    /// bpf-linker discarded the static, having grepped a stale object from a
    /// previous day's build rather than the one `aya-build` emits into
    /// `OUT_DIR` (#681). A confident negative from the wrong file cost a
    /// redesign. `patch_map_data` with `must_exist = true` is the same check
    /// `EbpfLoader::set_global` performs at load — symbol present, section
    /// mapped, and `data.len() == symbol.size` exactly — so this fails at
    /// `cargo test` time instead of on a user's host, and needs no privilege.
    #[test]
    fn set_global_target_exists_in_the_built_object() {
        use std::collections::HashMap;

        let bytes =
            aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/zensight-netlink-ebpf-prog"));
        let mut obj = aya_obj::Object::parse(bytes).expect("the built object must parse");

        let offsets = TcpSockOffsets {
            valid: 1,
            bytes_acked: 1840,
            bytes_received: 1784,
            segs_out: 1600,
            segs_in: 1792,
            total_retrans: 2216,
        };
        let data = offsets_to_bytes(&offsets);
        let mut globals: HashMap<&str, (&[u8], bool)> = HashMap::new();
        globals.insert("TCP_SOCK_OFFSETS", (&data[..], true));

        obj.patch_map_data(globals).expect(
            "TCP_SOCK_OFFSETS must be present in .rodata at exactly \
             size_of::<TcpSockOffsets>() — if this fails, either bpf-linker \
             stopped emitting the static or the struct changed size",
        );
    }

    /// Whatever this kernel says, the resolver must either produce a complete
    /// set or none at all — a half-filled `TcpSockOffsets` would read some
    /// fields from the right place and some from garbage.
    #[test]
    fn resolver_is_all_or_nothing() {
        let offs = resolve_tcp_sock_offsets();
        if offs.valid == 0 {
            // No BTF on this host, or the struct moved. Nothing more to check.
            return;
        }
        for (name, value) in TCP_SOCK_MEMBERS.iter().zip([
            offs.bytes_acked,
            offs.bytes_received,
            offs.segs_out,
            offs.segs_in,
            offs.total_retrans,
        ]) {
            assert!(
                value > 0,
                "tcp_sock.{name} resolved to offset 0, which is `sock` itself — \
                 the resolver reported success on an incomplete set"
            );
        }
    }
    use super::*;

    fn conn(local: &str, lport: u16, pid: u32, comm: &str) -> ConnectionRecord {
        ConnectionRecord {
            pid,
            comm: comm.into(),
            family: 4,
            local: local.into(),
            lport,
            remote: "1.1.1.1".into(),
            rport: 443,
            duration_ms: 10,
            ts_unix_ms: 0,
            tx_bytes: 0,
            rx_bytes: 0,
            segs_out: 0,
            segs_in: 0,
            retrans: 0,
            counters_measured: false,
        }
    }

    #[test]
    fn close_map_records_and_resolves_owner() {
        let m = OwnerMap::new(16, Duration::from_secs(60));
        m.note(&conn("10.0.0.1", 5555, 42, "curl"));

        assert_eq!(
            m.owner("10.0.0.1", 5555, "1.1.1.1", 443),
            Some((42, "curl".to_string()))
        );
        // Different tuple → miss.
        assert_eq!(m.owner("10.0.0.1", 5556, "1.1.1.1", 443), None);
        assert_eq!(m.owner("10.0.0.2", 5555, "1.1.1.1", 443), None);
    }

    #[test]
    fn close_map_expires_by_ttl() {
        let m = OwnerMap::new(16, Duration::ZERO);
        m.note(&conn("10.0.0.1", 5555, 42, "curl"));
        // TTL zero → immediately stale.
        assert_eq!(m.owner("10.0.0.1", 5555, "1.1.1.1", 443), None);
    }

    #[test]
    fn close_map_stays_bounded() {
        let m = OwnerMap::new(4, Duration::from_secs(60));
        for port in 0..100u16 {
            m.note(&conn("10.0.0.1", port, 1, "x"));
        }
        assert!(m.len() <= 4);
    }

    /// Tier-2b live-map lifecycle: an ESTABLISHED note is resolvable until the
    /// close event retires it.
    #[test]
    fn owner_map_remove_retires_entry() {
        let m = OwnerMap::new(16, Duration::from_secs(60));
        let c = conn("10.0.0.1", 5555, 42, "curl");
        m.note(&c);
        assert!(m.owner("10.0.0.1", 5555, "1.1.1.1", 443).is_some());
        m.remove(&c);
        assert_eq!(m.owner("10.0.0.1", 5555, "1.1.1.1", 443), None);
    }
}
