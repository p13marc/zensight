//! On-demand detail query channel (principle P2).
//!
//! Declares Zenoh queryables the GUI calls when a user drills into a host. Each
//! reply is a fresh, full table (routes / neighbors / sockets) serialized as
//! JSON — none of this high-cardinality detail is ever streamed onto the
//! telemetry bus. Replies are built from live nlink dumps at query time.
//!
//! Keys (mirroring the alerts queryable in `zensight-sensor-core`):
//! - `zensight/netlink/@/query/routes`    → `Vec<RouteRecord>`
//! - `zensight/netlink/@/query/neighbors` → `Vec<NeighborRecord>`
//! - `zensight/netlink/@/query/sockets?state=&port=` → `Vec<SocketRecord>`
//! - `zensight/netlink/@/query/addresses` → `Vec<AddressRecord>` (#10)
//! - `zensight/netlink/@/query/events`    → `Vec<EventRecord>` (#8, recent ring)

use std::sync::Arc;

use nlink::netlink::{
    Connection, Nftables, Route, SockDiag, Xfrm,
    nftables::types::Family as NftFamily,
    types::addr::Scope,
    xfrm::{IpsecProtocol, SecurityAssociation, XfrmMode},
};
use nlink::sockdiag::{
    CgroupPathMap, SocketFilter, SocketInfo, SocketOwnerMap, SocketState, TcpState,
};

use crate::collector::CollectHandle;
use crate::config::CollectConfig;
use crate::events::EventState;
use crate::map::{
    AddressRecord, NeighborRecord, NftRuleRecord, RouteRecord, SocketRecord, SocketSelector,
    TcRecord, XfrmSaRecord,
};
use crate::route_history::RouteHistory;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

/// Run the on-demand detail query channel until the session closes.
///
/// `key_prefix` is the sensor's telemetry prefix (e.g. `zensight/netlink`); the
/// queryables live under `<key_prefix>/@/query/<topic>`. `events` is the shared
/// real-time event ring (#8), served on `@/query/events`; `route_history` is the
/// default-route flap ring (#111), served on `@/query/route_changes`. `collect`
/// is the live collector-toggle handle (shared with the poll loop and the
/// runtime `collection` command channel) so the socket→process attribution
/// (#304) honors the same hot-swappable `socket_processes` toggle.
pub async fn run(
    session: Arc<zenoh::Session>,
    key_prefix: String,
    events: EventState,
    route_history: RouteHistory,
    collect: CollectHandle,
) {
    let route = match Connection::<Route>::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "query: failed to open netlink route connection");
            return;
        }
    };
    let sockdiag = Connection::<SockDiag>::new().ok();
    let xfrm = Connection::<Xfrm>::new().ok();
    let nft = Connection::<Nftables>::new().ok();

    let routes_key = zensight_common::command::query_key(&key_prefix, "routes");
    let neighbors_key = zensight_common::command::query_key(&key_prefix, "neighbors");
    let sockets_key = zensight_common::command::query_key(&key_prefix, "sockets");
    let addresses_key = zensight_common::command::query_key(&key_prefix, "addresses");
    let events_key = zensight_common::command::query_key(&key_prefix, "events");
    let route_changes_key = zensight_common::command::query_key(&key_prefix, "route_changes");
    let tc_key = zensight_common::command::query_key(&key_prefix, "tc");
    let xfrm_key = zensight_common::command::query_key(&key_prefix, "xfrm");
    let nft_key = zensight_common::command::query_key(&key_prefix, "nft");

    let routes_q = match session.declare_queryable(&routes_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %routes_key, "query: declare routes failed");
            return;
        }
    };
    let neighbors_q = match session.declare_queryable(&neighbors_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %neighbors_key, "query: declare neighbors failed");
            return;
        }
    };
    let sockets_q = match session.declare_queryable(&sockets_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %sockets_key, "query: declare sockets failed");
            return;
        }
    };
    let addresses_q = match session.declare_queryable(&addresses_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %addresses_key, "query: declare addresses failed");
            return;
        }
    };
    let events_q = match session.declare_queryable(&events_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %events_key, "query: declare events failed");
            return;
        }
    };
    let route_changes_q = match session.declare_queryable(&route_changes_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %route_changes_key, "query: declare route_changes failed");
            return;
        }
    };
    let tc_q = match session.declare_queryable(&tc_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %tc_key, "query: declare tc failed");
            return;
        }
    };
    let xfrm_q = match session.declare_queryable(&xfrm_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %xfrm_key, "query: declare xfrm failed");
            return;
        }
    };
    let nft_q = match session.declare_queryable(&nft_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %nft_key, "query: declare nft failed");
            return;
        }
    };
    tracing::info!(
        routes = %routes_key, neighbors = %neighbors_key, sockets = %sockets_key,
        addresses = %addresses_key, events = %events_key,
        "on-demand detail query channel ready"
    );

    loop {
        tokio::select! {
            q = routes_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = collect_routes(&route).await;
                reply_json(&query, &records).await;
            }
            q = neighbors_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = collect_neighbors(&route).await;
                reply_json(&query, &records).await;
            }
            q = sockets_q.recv_async() => {
                let Ok(query) = q else { return };
                let sel = SocketSelector::parse(query.parameters().as_str());
                // Snapshot the live toggles at query time (#304) — a runtime
                // `socket_processes` toggle takes effect on the next query.
                let toggles = collect.snapshot().await;
                let records = match &sockdiag {
                    Some(sd) => collect_sockets(sd, &sel, &toggles).await,
                    None => Vec::new(),
                };
                reply_json(&query, &records).await;
            }
            q = addresses_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = collect_addresses(&route).await;
                reply_json(&query, &records).await;
            }
            q = events_q.recv_async() => {
                let Ok(query) = q else { return };
                reply_json(&query, &events.recent()).await;
            }
            q = route_changes_q.recv_async() => {
                let Ok(query) = q else { return };
                reply_json(&query, &route_history.recent()).await;
            }
            q = tc_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = collect_tc(&route).await;
                reply_json(&query, &records).await;
            }
            q = xfrm_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = match &xfrm {
                    Some(x) => collect_xfrm(x).await,
                    None => Vec::new(),
                };
                reply_json(&query, &records).await;
            }
            q = nft_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = match &nft {
                    Some(n) => collect_nft(n).await,
                    None => Vec::new(),
                };
                reply_json(&query, &records).await;
            }
        }
    }
}

/// Serve the opt-in eBPF detail queryables (#114) until the session closes:
/// `@/query/retransmits` (top-K retransmit peers) and `@/query/connections`
/// (recent tcplife records). Spawned only when the `ebpf` feature is built and
/// the module loaded; reads the shared [`EbpfState`](crate::ebpf::EbpfState).
#[cfg(feature = "ebpf")]
pub async fn run_ebpf_queries(
    session: Arc<zenoh::Session>,
    key_prefix: String,
    ebpf: crate::ebpf::EbpfState,
    top_k: usize,
) {
    let retransmits_key = zensight_common::command::query_key(&key_prefix, "retransmits");
    let connections_key = zensight_common::command::query_key(&key_prefix, "connections");
    let retransmits_q = match session.declare_queryable(&retransmits_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %retransmits_key, "query: declare retransmits failed");
            return;
        }
    };
    let connections_q = match session.declare_queryable(&connections_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %connections_key, "query: declare connections failed");
            return;
        }
    };
    tracing::info!(
        retransmits = %retransmits_key, connections = %connections_key,
        "eBPF detail query channel ready"
    );
    loop {
        tokio::select! {
            q = retransmits_q.recv_async() => {
                let Ok(query) = q else { return };
                reply_json(&query, &ebpf.top_retransmits(top_k)).await;
            }
            q = connections_q.recv_async() => {
                let Ok(query) = q else { return };
                reply_json(&query, &ebpf.recent_connections()).await;
            }
        }
    }
}

/// Serialize `records` as JSON and reply on the query's own key.
async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, records: &T) {
    match serde_json::to_vec(records) {
        Ok(payload) => {
            if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                tracing::warn!(error = %e, "query: reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "query: serialize failed"),
    }
}

async fn collect_routes(conn: &Connection<Route>) -> Vec<RouteRecord> {
    let routes = match conn.get_routes().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "query: get_routes failed");
            return Vec::new();
        }
    };
    routes
        .iter()
        .map(|rt| {
            let dst = if rt.is_default() {
                "default".to_string()
            } else if let Some(d) = rt.destination() {
                format!("{}/{}", d, rt.dst_len())
            } else {
                format!("/{}", rt.dst_len())
            };
            RouteRecord {
                family: fam(rt.family()),
                dst,
                gateway: rt.gateway().map(|g| g.to_string()),
                oif: rt.oif(),
                priority: rt.priority(),
                protocol: format!("{:?}", rt.protocol()).to_lowercase(),
                scope: format!("{:?}", rt.scope()).to_lowercase(),
                table: rt.table_id(),
            }
        })
        .collect()
}

async fn collect_neighbors(conn: &Connection<Route>) -> Vec<NeighborRecord> {
    let neighbors = match conn.get_neighbors().await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "query: get_neighbors failed");
            return Vec::new();
        }
    };
    neighbors
        .iter()
        .map(|nb| NeighborRecord {
            family: fam(nb.family()),
            ip: nb.destination().map(|d| d.to_string()),
            mac: nb.mac_address(),
            ifindex: nb.ifindex(),
            state: format!("{:?}", nb.state()).to_lowercase(),
            is_router: nb.is_router(),
        })
        .collect()
}

/// Build the full nftables rule inventory (#14): every rule across every table,
/// with table/chain/family/handle and any comment. Served via `@/query/nft`.
async fn collect_nft(conn: &Connection<Nftables>) -> Vec<NftRuleRecord> {
    let tables = match conn.list_tables().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "query: nft list_tables failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for t in &tables {
        let family = nft_family_label(t.family);
        if let Ok(rules) = conn.list_rules(&t.name, t.family).await {
            for r in &rules {
                // Decode the per-rule packet/byte counter (#115); absent counter
                // statement → zeros.
                let ctr = crate::map::decode_nft_counter(&r.expression_bytes).unwrap_or_default();
                out.push(NftRuleRecord {
                    family: family.to_string(),
                    table: r.table.clone(),
                    chain: r.chain.clone(),
                    handle: r.handle,
                    comment: r.comment.clone(),
                    packets: ctr.packets,
                    bytes: ctr.bytes,
                });
            }
        }
    }
    out
}

fn nft_family_label(f: NftFamily) -> &'static str {
    match f {
        NftFamily::Ip => "ip",
        NftFamily::Ip6 => "ip6",
        NftFamily::Inet => "inet",
        NftFamily::Arp => "arp",
        NftFamily::Bridge => "bridge",
        NftFamily::Netdev => "netdev",
        _ => "other",
    }
}

/// Build the per-SA IPsec detail (#13). Each record carries src/dst/spi/proto/
/// mode/reqid + processed bytes/packets.
async fn collect_xfrm(conn: &Connection<Xfrm>) -> Vec<XfrmSaRecord> {
    let sas = match conn.get_security_associations().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "query: get_security_associations failed");
            return Vec::new();
        }
    };
    sas.iter()
        .map(|sa: &SecurityAssociation| XfrmSaRecord {
            src: sa.src_addr.map(|a| a.to_string()),
            dst: sa.dst_addr.map(|a| a.to_string()),
            spi: sa.spi,
            proto: ipsec_proto_label(&sa.protocol).to_string(),
            mode: xfrm_mode_label(&sa.mode).to_string(),
            reqid: sa.reqid,
            bytes: sa.bytes,
            packets: sa.packets,
        })
        .collect()
}

fn ipsec_proto_label(p: &IpsecProtocol) -> &'static str {
    match p {
        IpsecProtocol::Esp => "esp",
        IpsecProtocol::Ah => "ah",
        IpsecProtocol::Comp => "comp",
        _ => "other",
    }
}

fn xfrm_mode_label(m: &XfrmMode) -> &'static str {
    match m {
        XfrmMode::Transport => "transport",
        XfrmMode::Tunnel => "tunnel",
        XfrmMode::Beet => "beet",
        // `XfrmMode` is #[non_exhaustive] upstream (also covers `Other`).
        _ => "other",
    }
}

/// Build the full TC tree (#12): every qdisc + class on every interface, with
/// counters/backlog. Served on demand via `@/query/tc`.
async fn collect_tc(conn: &Connection<Route>) -> Vec<TcRecord> {
    let names: std::collections::HashMap<u32, String> = match conn.get_links().await {
        Ok(links) => links
            .into_iter()
            .filter_map(|l| {
                let n = l.name_or("?").to_string();
                (n != "?").then_some((l.ifindex(), n))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "query: tc get_links failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    let mut push = |msgs: Vec<nlink::netlink::messages::TcMessage>, node: &str| {
        for m in &msgs {
            let iface = names
                .get(&m.ifindex())
                .cloned()
                .unwrap_or_else(|| m.ifindex().to_string());
            out.push(TcRecord {
                iface,
                node: node.to_string(),
                kind: m.kind().map(|s| s.to_string()),
                handle: m.handle_str(),
                parent: m.parent_str(),
                bytes: m.bytes(),
                packets: m.packets(),
                drops: m.drops() as u64,
                overlimits: m.overlimits() as u64,
                requeues: m.requeues() as u64,
                backlog_bytes: m.backlog() as u64,
                backlog_pkts: m.qlen() as u64,
            });
        }
    };
    if let Ok(q) = conn.get_qdiscs().await {
        push(q, "qdisc");
    }
    if let Ok(c) = conn.get_classes().await {
        push(c, "class");
    }
    out
}

/// Build the per-address detail (#10) from a live address dump. Each record
/// carries family/ip/prefix/scope/label/ifindex — the GUI mirrors this shape.
async fn collect_addresses(conn: &Connection<Route>) -> Vec<AddressRecord> {
    let addrs = match conn.get_addresses().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "query: get_addresses failed");
            return Vec::new();
        }
    };
    addrs
        .iter()
        .map(|a| AddressRecord {
            family: fam(a.family()),
            ip: a.address().or_else(|| a.local()).map(|ip| ip.to_string()),
            prefix_len: a.prefix_len(),
            scope: scope_label(a.scope()).to_string(),
            label: a.label().map(|s| s.to_string()),
            ifindex: a.ifindex(),
        })
        .collect()
}

/// Lowercase label for an address scope (matches the iproute2 vocabulary).
fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Universe => "global",
        Scope::Site => "site",
        Scope::Link => "link",
        Scope::Host => "host",
        Scope::Nowhere => "nowhere",
        // `Scope` is #[non_exhaustive] upstream.
        _ => "other",
    }
}

/// One `/proc` fd walk + one cgroupfs walk for socket→process attribution
/// (#304), off the async runtime (`spawn_blocking` — the walks are pure fs
/// I/O). Returns `None` (→ reply unattributed) when the host has more numeric
/// `/proc` entries than `max_procs` (query-time cost ceiling) or the blocking
/// task failed.
async fn scan_owner_maps(max_procs: usize) -> Option<(SocketOwnerMap, CgroupPathMap)> {
    let joined = tokio::task::spawn_blocking(move || {
        let procs = count_procs("/proc");
        if procs > max_procs {
            return Err(procs);
        }
        let t0 = std::time::Instant::now();
        let owners = SocketOwnerMap::scan();
        let cgroups = CgroupPathMap::scan();
        Ok((owners, cgroups, t0.elapsed()))
    })
    .await;
    match joined {
        Ok(Ok((owners, cgroups, elapsed))) => {
            tracing::debug!(ms = elapsed.as_millis() as u64, "socket owner scan");
            Some((owners, cgroups))
        }
        Ok(Err(procs)) => {
            tracing::warn!(procs, ceiling = max_procs, "socket process scan skipped");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "socket owner scan task failed");
            None
        }
    }
}

/// Number of numeric (PID) entries under a proc root.
fn count_procs(proc_root: &str) -> usize {
    std::fs::read_dir(proc_root)
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
                })
                .count()
        })
        .unwrap_or(0)
}

/// Join one socket record against the owner/cgroup maps (#304): first owning
/// process (pid/comm/start_time — identity is the `(pid, start_time)` pair)
/// plus the cgroup v2 path resolved from the sockdiag cgroup id. Pure, so the
/// fixture tests exercise the exact production join.
fn apply_attribution(rec: &mut SocketRecord, owners: &SocketOwnerMap, cgroups: &CgroupPathMap) {
    if let Some(p) = owners.resolve(rec.inode).first() {
        rec.pid = Some(p.pid);
        rec.process = Some(p.comm.clone());
        rec.proc_start_time = Some(p.start_time);
    }
    if let Some(id) = rec.cgroup_id {
        rec.cgroup = cgroups
            .resolve_relative(id)
            .map(|p| p.to_string_lossy().into_owned());
    }
}

async fn collect_sockets(
    conn: &Connection<SockDiag>,
    sel: &SocketSelector,
    collect: &CollectConfig,
) -> Vec<SocketRecord> {
    // Socket→process attribution (#304): one amortized /proc + cgroupfs walk
    // per query, joined per-record below. `None` → records stay unattributed.
    let maps = if collect.socket_processes {
        scan_owner_maps(collect.socket_process_max_procs).await
    } else {
        None
    };
    // Mirror the streamed aggregate's extensions (#11) so the drill-down shows
    // congestion algorithm / window and per-socket buffer sizes.
    // Mirror the streamed aggregate's extensions. When the selector names a
    // state, push it into the kernel as a sockdiag state bitmask (nlink 0.23) so
    // the kernel returns only matching sockets instead of dumping every socket
    // for us to filter. The port match stays in user space: its "local OR
    // remote" semantics doesn't map to sockdiag's `sport AND dport` bytecode.
    let builder = SocketFilter::tcp()
        .with_tcp_info()
        .with_mem_info()
        .with_congestion();
    let builder = match sel.state.as_deref().and_then(tcp_state_from_label) {
        Some(st) => builder.states(&[st]),
        None => builder.all_states(),
    };
    let filter = builder.build();
    let socks = match conn.query(&filter).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "query: sockdiag query failed");
            return Vec::new();
        }
    };
    socks
        .iter()
        .filter_map(|s| {
            let SocketInfo::Inet(inet) = s else {
                return None;
            };
            let (rtt_us, retrans, snd_cwnd) = inet
                .tcp_info
                .as_ref()
                .map(|ti| (ti.rtt, ti.retrans, ti.snd_cwnd))
                .unwrap_or((0, 0, 0));
            // Enriched per-socket delivery health (#108). Normalize the kernel's
            // `~0` "unpaced" pacing-rate sentinel to 0 so the GUI shows a clean value.
            let ti = inet.tcp_info.as_ref();
            let delivery_rate = ti.map(|t| t.delivery_rate).unwrap_or(0);
            let pacing_rate = ti
                .map(|t| {
                    if t.pacing_rate == u64::MAX {
                        0
                    } else {
                        t.pacing_rate
                    }
                })
                .unwrap_or(0);
            let bytes_retrans = ti.map(|t| t.bytes_retrans).unwrap_or(0);
            let total_retrans = ti.map(|t| t.total_retrans).unwrap_or(0);
            let rcv_rtt_us = ti.map(|t| t.rcv_rtt).unwrap_or(0);
            let lost = ti.map(|t| t.lost).unwrap_or(0);
            let reord_seen = ti.map(|t| t.reord_seen).unwrap_or(0);
            let (snd_buf, rcv_buf) = inet
                .mem_info
                .as_ref()
                .map(|m| (m.sndbuf, m.rcvbuf))
                .unwrap_or((0, 0));
            let mut rec = SocketRecord {
                local: inet.local.to_string(),
                remote: inet.remote.to_string(),
                state: socket_state_str(&inet.state),
                uid: inet.uid,
                recv_q: inet.recv_q,
                send_q: inet.send_q,
                rtt_us,
                retrans,
                inode: inet.inode,
                congestion: inet.congestion.clone(),
                snd_cwnd,
                snd_buf,
                rcv_buf,
                delivery_rate,
                pacing_rate,
                bytes_retrans,
                total_retrans,
                rcv_rtt_us,
                lost,
                reord_seen,
                // Stable socket id + owning cgroup id ride along even when the
                // /proc attribution scan is off (#304).
                cookie: inet.cookie,
                cgroup_id: inet.cgroup_id,
                cgroup: None,
                pid: None,
                process: None,
                proc_start_time: None,
            };
            if let Some((owners, cgroups)) = &maps {
                apply_attribution(&mut rec, owners, cgroups);
            }
            sel.matches(&rec).then_some(rec)
        })
        .collect()
}

/// Inverse of [`socket_state_str`]: map a canonical lowercase state label back
/// to an nlink [`TcpState`] for kernel-side sockdiag filtering. Unknown labels
/// return `None`, so the caller dumps all states and relies on the user-space
/// [`SocketSelector::matches`] filter.
fn tcp_state_from_label(s: &str) -> Option<TcpState> {
    Some(match s {
        "established" => TcpState::Established,
        "listen" => TcpState::Listen,
        "time_wait" => TcpState::TimeWait,
        "syn_sent" => TcpState::SynSent,
        "close_wait" => TcpState::CloseWait,
        _ => return None,
    })
}

/// Canonical lowercase state string, matching the streamed `sockets/tcp/<state>`
/// aggregate names so GUI filters line up with the summary metrics.
fn socket_state_str(state: &SocketState) -> String {
    match state {
        SocketState::Tcp(TcpState::Established) | SocketState::Established => "established",
        SocketState::Tcp(TcpState::Listen) | SocketState::Listen => "listen",
        SocketState::Tcp(TcpState::TimeWait) => "time_wait",
        SocketState::Tcp(TcpState::SynSent) => "syn_sent",
        SocketState::Tcp(TcpState::CloseWait) => "close_wait",
        other => return format!("{other:?}").to_lowercase(),
    }
    .to_string()
}

/// Map an `AF_*` family byte to the friendly `4`/`6` (0 if neither).
fn fam(family: u8) -> u8 {
    match family {
        AF_INET => 4,
        AF_INET6 => 6,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    /// A blank record with just the attribution inputs set.
    fn rec(inode: u32, cgroup_id: Option<u64>) -> SocketRecord {
        SocketRecord {
            local: "10.0.0.1:5555".into(),
            remote: "1.1.1.1:443".into(),
            state: "established".into(),
            uid: 0,
            recv_q: 0,
            send_q: 0,
            rtt_us: 0,
            retrans: 0,
            inode,
            congestion: None,
            snd_cwnd: 0,
            snd_buf: 0,
            rcv_buf: 0,
            delivery_rate: 0,
            pacing_rate: 0,
            bytes_retrans: 0,
            total_retrans: 0,
            rcv_rtt_us: 0,
            lost: 0,
            reord_seen: 0,
            cookie: 0,
            cgroup_id,
            cgroup: None,
            pid: None,
            process: None,
            proc_start_time: None,
        }
    }

    /// Build a /proc-shaped subtree the way `SocketOwnerMap::scan_with_root`
    /// reads it: `<root>/<pid>/{comm,stat,fd/<n>}` with fd symlinks pointing at
    /// `socket:[inode]` (dangling targets are fine for `read_link`). The stat
    /// line puts `start_time` at field 22 — token index 19 after the last `)`.
    fn fake_proc(root: &Path, pid: i32, comm: &str, start_time: u64, fds: &[(i32, u32)]) {
        let pid_dir = root.join(pid.to_string());
        fs::create_dir_all(pid_dir.join("fd")).unwrap();
        fs::write(pid_dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(
            pid_dir.join("stat"),
            format!(
                "{pid} ({comm}) S 1 {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {start_time} 0 0 0"
            ),
        )
        .unwrap();
        for (fd, inode) in fds {
            std::os::unix::fs::symlink(
                format!("socket:[{inode}]"),
                pid_dir.join("fd").join(fd.to_string()),
            )
            .unwrap();
        }
    }

    /// Fresh temp dir per test (no tempfile dep; cleaned up at the end).
    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zensight-netlink-query-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn attribution_joins_inode_to_process() {
        let root = temp_root("proc");
        // Non-PID entries must be skipped by the scan.
        fs::create_dir_all(root.join("self")).unwrap();
        fake_proc(&root, 100, "nginx", 5000, &[(3, 777)]);
        fake_proc(&root, 200, "sshd", 6000, &[(4, 888)]);

        let owners = SocketOwnerMap::scan_with_root(&root);
        let cgroups = CgroupPathMap::default(); // empty — no cgroup ids here

        let mut r = rec(777, None);
        apply_attribution(&mut r, &owners, &cgroups);
        assert_eq!(r.pid, Some(100));
        assert_eq!(r.process.as_deref(), Some("nginx"));
        assert_eq!(r.proc_start_time, Some(5000));
        assert_eq!(r.cgroup, None);

        let mut r = rec(888, None);
        apply_attribution(&mut r, &owners, &cgroups);
        assert_eq!(r.pid, Some(200));
        assert_eq!(r.process.as_deref(), Some("sshd"));
        assert_eq!(r.proc_start_time, Some(6000));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn attribution_unknown_inode_stays_unattributed() {
        let root = temp_root("proc-miss");
        fake_proc(&root, 100, "nginx", 5000, &[(3, 777)]);

        let owners = SocketOwnerMap::scan_with_root(&root);
        let mut r = rec(999, None);
        apply_attribution(&mut r, &owners, &CgroupPathMap::default());
        assert_eq!(r.pid, None);
        assert_eq!(r.process, None);
        assert_eq!(r.proc_start_time, None);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn attribution_resolves_cgroup_relative_path() {
        use std::os::unix::fs::MetadataExt;

        let root = temp_root("cgroup");
        let leaf = root.join("system.slice").join("sshd.service");
        fs::create_dir_all(&leaf).unwrap();

        let cgroups = CgroupPathMap::scan_with_root(&root);
        let leaf_id = fs::metadata(&leaf).unwrap().ino();

        let mut r = rec(1, Some(leaf_id));
        apply_attribution(&mut r, &SocketOwnerMap::new(), &cgroups);
        assert_eq!(r.cgroup.as_deref(), Some("system.slice/sshd.service"));

        // Unknown cgroup id → None.
        let mut r = rec(1, Some(u64::MAX));
        apply_attribution(&mut r, &SocketOwnerMap::new(), &cgroups);
        assert_eq!(r.cgroup, None);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn count_procs_counts_numeric_entries_only() {
        let root = temp_root("count");
        fs::create_dir_all(root.join("100")).unwrap();
        fs::create_dir_all(root.join("200")).unwrap();
        fs::create_dir_all(root.join("self")).unwrap();
        fs::write(root.join("meminfo"), "x").unwrap();

        assert_eq!(count_procs(root.to_str().unwrap()), 2);
        assert_eq!(count_procs("/nonexistent-proc-root"), 0);

        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn scan_owner_maps_honors_ceiling() {
        // A ceiling of 0 is always exceeded on a live host → skipped scan.
        assert!(scan_owner_maps(0).await.is_none());
    }
}
