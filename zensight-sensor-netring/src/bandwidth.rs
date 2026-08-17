//! Wire-level bandwidth-by-process serving (#318, opt-in).
//!
//! Two tasks back the attribution tier whose hot-path hook lives in
//! [`crate::owner_map`]:
//!
//! * [`run_refresh`] — on a fixed cadence, dumps the kernel socket table
//!   (sock_diag, TCP+UDP, v4+v6) and scans `/proc` for the inode→process join,
//!   rebuilds the flow→owner [`OwnerTable`](crate::owner_map::OwnerTable), and
//!   hot-swaps it into the shared `ArcSwap` the capture hook loads. All the
//!   scanning is here, OFF netring's synchronous attribution hook.
//! * [`run_query`] — serves the latest per-owner wire-L2 rows on
//!   `@rpc/netring/bandwidth` (query-only, principle P2), the same
//!   `BandwidthRecord` contract the netlink sock_diag tier serves.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use flowscope::L4Proto;
use nlink::netlink::{Connection, SockDiag};
use nlink::sockdiag::{Protocol, SocketFilter, SocketInfo, SocketOwnerMap};

use crate::monitor::OwnerBandwidth;
use crate::owner_map::{OwnerIdent, OwnerTable, SlotRegistry, SocketRow};

/// Default top-N when the query carries no `?top=` selector.
const DEFAULT_TOP_N: usize = 100;

/// Off-hook socket-table refresh loop (#318): keeps the shared flow→owner table
/// current so the capture hook's lookups attribute live flows. Runs until the
/// sensor stops. A failed sock_diag connection is fatal to attribution (logged,
/// task exits); a failed dump on one tick is skipped (kept for the next).
pub async fn run_refresh(table: Arc<ArcSwap<OwnerTable>>, period_secs: u64) {
    let conn = match Connection::<SockDiag>::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "netring bandwidth: sock_diag unavailable; attribution disabled");
            return;
        }
    };
    // Persistent, append-only slot registry: a slot handed to the hook stays
    // valid for the sensor's lifetime even as the flow map is rebuilt each tick.
    let mut registry = SlotRegistry::new();
    let mut tick = tokio::time::interval(Duration::from_secs(period_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let sockets = dump_sockets(&conn).await;
        if sockets.is_empty() {
            continue;
        }
        // The `/proc` fd walk is blocking I/O — keep it off the async reactor.
        let owners = match tokio::task::spawn_blocking(SocketOwnerMap::scan).await {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(error = %e, "netring bandwidth: /proc scan task failed");
                continue;
            }
        };
        let fresh = registry.build_table(sockets, |inode| {
            owners.resolve(inode).first().map(|p| OwnerIdent {
                pid: p.pid,
                start_time: p.start_time,
                comm: p.comm.clone(),
            })
        });
        tracing::debug!(
            flows = fresh.flow_count(),
            "netring bandwidth: owner table refreshed"
        );
        table.store(Arc::new(fresh));
    }
}

/// Dump every inet socket (TCP + UDP, both families) to a flat [`SocketRow`]
/// list — 5-tuple + inode only, no `tcp_info` / cgroup extensions (the join
/// needs neither). A per-protocol failure is logged and skipped.
async fn dump_sockets(conn: &Connection<SockDiag>) -> Vec<SocketRow> {
    let mut rows = Vec::new();
    for (proto, filter) in [
        (L4Proto::Tcp, SocketFilter::tcp().all_states().build()),
        (L4Proto::Udp, SocketFilter::udp().all_states().build()),
    ] {
        match conn.query(&filter).await {
            Ok(socks) => rows.extend(socks.iter().filter_map(|s| {
                let SocketInfo::Inet(inet) = s else {
                    return None;
                };
                // Only TCP/UDP participate in netring's owner bandwidth.
                if !matches!(inet.protocol, Protocol::Tcp | Protocol::Udp) {
                    return None;
                }
                Some(SocketRow {
                    proto,
                    local: inet.local,
                    remote: inet.remote,
                    inode: inet.inode,
                })
            })),
            Err(e) => {
                tracing::debug!(error = %e, ?proto, "netring bandwidth: sock_diag dump failed")
            }
        }
    }
    rows
}

/// Serve the latest wire-level per-process bandwidth rows on
/// `@rpc/netring/bandwidth` as JSON `Vec<BandwidthRecord>`, newest
/// snapshot, trimmed to `?top=N` (default 100).
pub async fn run_query(session: Arc<zenoh::Session>, producer: String, shared: OwnerBandwidth) {
    let key = zensight_common::command::query_key(&producer, "bandwidth");
    let queryable = match zensight_common::served::serve_queryable(&session, &key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare bandwidth failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand wire-bandwidth query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let top = query
            .parameters()
            .get("top")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_TOP_N);
        let mut records: Vec<_> = match shared.records.lock() {
            Ok(r) => r.clone(),
            Err(_) => Vec::new(),
        };
        records.truncate(top);
        match serde_json::to_vec(&records) {
            Ok(payload) => {
                if let Err(e) = query.reply(key.as_str(), payload).await {
                    tracing::warn!(error = %e, "query: bandwidth reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "query: bandwidth serialize failed"),
        }
    }
}
