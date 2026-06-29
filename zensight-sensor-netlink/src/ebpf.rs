//! Userspace side of the opt-in eBPF module (#114).
//!
//! Loads the bytecode compiled by `build.rs`, attaches the connlat kprobes +
//! the retransmit / tcplife tracepoints, drains the connection ring buffer into
//! a bounded in-memory ring, and exposes readers for the collector (connlat
//! gauges) and the `@/query/{retransmits,connections}` channels.
//!
//! Gated on `feature = "ebpf"` — the rest of the crate stays aya-free. Any
//! load/attach failure (no `CAP_BPF`/`CAP_NET_ADMIN`, unsupported kernel) is
//! returned as an `Err`; the caller logs one warning and the unprivileged
//! baseline is untouched.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    EbpfLoader,
    // The userspace handle for a kernel LRU_HASH map is `HashMap` (aya has no
    // separate userspace `LruHashMap`); its TryFrom accepts the LRU variant.
    maps::{HashMap as AyaHashMap, MapData, PerCpuArray, RingBuf},
    programs::{KProbe, TracePoint},
};
use tokio::io::unix::AsyncFd;
use zensight_sensor_netlink_ebpf_common::{CONNLAT_BUCKETS, ConnRecord, RetransKey};

use crate::map::{ConnView, RetransRecord, connlat_percentiles, top_k_retransmits};

/// Shared, clonable handle to the eBPF-derived state (mirrors `EventState`).
#[derive(Clone)]
pub struct EbpfState {
    inner: Arc<Inner>,
}

struct Inner {
    conns: Mutex<VecDeque<ConnView>>,
    conn_cap: usize,
    retrans: Mutex<AyaHashMap<MapData, RetransKey, u64>>,
    connlat: Mutex<PerCpuArray<MapData, u64>>,
    connlat_prev: Mutex<[u64; CONNLAT_BUCKETS]>,
}

impl EbpfState {
    /// Push a drained connection record, dropping the oldest past capacity.
    fn push_conn(&self, v: ConnView) {
        if let Ok(mut q) = self.inner.conns.lock() {
            if q.len() == self.inner.conn_cap {
                q.pop_front();
            }
            q.push_back(v);
        }
    }

    /// Recent completed-connection records (oldest first), for `@/query/connections`.
    pub fn recent_connections(&self) -> Vec<ConnView> {
        self.inner
            .conns
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Top-K retransmit peers, for `@/query/retransmits`.
    pub fn top_retransmits(&self, k: usize) -> Vec<RetransRecord> {
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

    let mut bpf = EbpfLoader::new()
        .load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            "/zensight-sensor-netlink-ebpf"
        )))
        .context("load eBPF bytecode")?;

    if let Err(e) = aya_log::EbpfLogger::init(&mut bpf) {
        tracing::debug!(error = %e, "eBPF: aya-log init skipped");
    }

    // connlat: entry kprobes + matching return kprobes (the macro marks the
    // `_ret` programs as kretprobes; both attach to the same kernel function).
    attach_kprobe(&mut bpf, "tcp_v4_connect", "tcp_v4_connect")?;
    attach_kprobe(&mut bpf, "tcp_v6_connect", "tcp_v6_connect")?;
    attach_kprobe(&mut bpf, "tcp_v4_connect_ret", "tcp_v4_connect")?;
    attach_kprobe(&mut bpf, "tcp_v6_connect_ret", "tcp_v6_connect")?;
    // retransmit + tcplife tracepoints.
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
        }),
    };
    Ok((bpf, state, ring))
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
        let ring = guard.get_inner_mut();
        while let Some(item) = ring.next() {
            if item.len() >= rec_size {
                // SAFETY: ConnRecord is repr(C) POD; the kernel reserved exactly
                // this layout. read_unaligned tolerates ring-buffer alignment.
                let rec = unsafe { std::ptr::read_unaligned(item.as_ptr() as *const ConnRecord) };
                state.push_conn(ConnView::from_record(&rec));
            }
        }
        guard.clear_ready();
    }
}

fn attach_kprobe(bpf: &mut Ebpf, prog: &str, fn_name: &str) -> Result<()> {
    let p: &mut KProbe = bpf
        .program_mut(prog)
        .with_context(|| format!("program {prog} missing"))?
        .try_into()
        .with_context(|| format!("program {prog} is not a kprobe"))?;
    p.load().with_context(|| format!("load {prog}"))?;
    p.attach(fn_name, 0)
        .with_context(|| format!("attach kprobe {fn_name}"))?;
    Ok(())
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
