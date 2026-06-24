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

use nlink::netlink::{Connection, Route, SockDiag, types::addr::Scope};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};

use crate::events::EventState;
use crate::map::{
    AddressRecord, NeighborRecord, RouteRecord, SocketRecord, SocketSelector, TcRecord,
};

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

/// Run the on-demand detail query channel until the session closes.
///
/// `key_prefix` is the sensor's telemetry prefix (e.g. `zensight/netlink`); the
/// queryables live under `<key_prefix>/@/query/<topic>`. `events` is the shared
/// real-time event ring (#8), served on `@/query/events`.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, events: EventState) {
    let route = match Connection::<Route>::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "query: failed to open netlink route connection");
            return;
        }
    };
    let sockdiag = Connection::<SockDiag>::new().ok();

    let routes_key = format!("{key_prefix}/@/query/routes");
    let neighbors_key = format!("{key_prefix}/@/query/neighbors");
    let sockets_key = format!("{key_prefix}/@/query/sockets");
    let addresses_key = format!("{key_prefix}/@/query/addresses");
    let events_key = format!("{key_prefix}/@/query/events");
    let tc_key = format!("{key_prefix}/@/query/tc");

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
    let tc_q = match session.declare_queryable(&tc_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %tc_key, "query: declare tc failed");
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
                let records = match &sockdiag {
                    Some(sd) => collect_sockets(sd, &sel).await,
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
            q = tc_q.recv_async() => {
                let Ok(query) = q else { return };
                let records = collect_tc(&route).await;
                reply_json(&query, &records).await;
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

async fn collect_sockets(conn: &Connection<SockDiag>, sel: &SocketSelector) -> Vec<SocketRecord> {
    // Mirror the streamed aggregate's extensions (#11) so the drill-down shows
    // congestion algorithm / window and per-socket buffer sizes.
    let filter = SocketFilter::tcp()
        .all_states()
        .with_tcp_info()
        .with_mem_info()
        .with_congestion()
        .build();
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
            let (snd_buf, rcv_buf) = inet
                .mem_info
                .as_ref()
                .map(|m| (m.sndbuf, m.rcvbuf))
                .unwrap_or((0, 0));
            let rec = SocketRecord {
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
            };
            sel.matches(&rec).then_some(rec)
        })
        .collect()
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
