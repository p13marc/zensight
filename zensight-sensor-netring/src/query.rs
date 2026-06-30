//! On-demand flow detail query channel (principle P2).
//!
//! Serves the bounded ring of recent ended-flow records via a Zenoh queryable at
//! `zensight/netring/@/query/flows` — the high-cardinality 5-tuple/volume detail
//! behind the streamed flow aggregates, pulled only when a user drills in (never
//! streamed onto the telemetry bus).

use std::sync::Arc;

use crate::map;
use crate::monitor::{
    AssetInventory, DnsInventory, ElephantRing, FlowRing, HttpInventory, Ja4hInventory, MatrixHist,
    QuicInventory, SshInventory, TalkerHist, TlsInventory,
};

/// Default top-N for the talker / domain / host channels when no `?top=` query
/// selector is supplied.
const DEFAULT_TOP_N: usize = 50;

/// Parse a `?top=N` selector from a query's parameters; falls back to the default.
fn top_n(query: &zenoh::query::Query) -> usize {
    query
        .parameters()
        .get("top")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TOP_N)
}

/// Run the flow-detail query channel until the session closes. Replies with the
/// recent ended-flow records (most-recent first) as JSON `Vec<FlowRecord>`.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, flows: FlowRing) {
    let key = zensight_common::command::query_key(&key_prefix, "flows");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare flows failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand flow query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        // Snapshot newest-first without holding the lock across the await.
        let records: Vec<_> = {
            match flows.lock() {
                Ok(r) => r.iter().rev().cloned().collect(),
                Err(_) => Vec::new(),
            }
        };
        match serde_json::to_vec(&records) {
            Ok(payload) => {
                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                    tracing::warn!(error = %e, "query: flows reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "query: flows serialize failed"),
        }
    }
}

/// Run the TLS asset-inventory query channel: `zensight/netring/@/query/tls`
/// replies with the passive fingerprint inventory (most-seen first) as JSON
/// `Vec<TlsRecord>`.
pub async fn run_tls(session: Arc<zenoh::Session>, key_prefix: String, inventory: TlsInventory) {
    let key = zensight_common::command::query_key(&key_prefix, "tls");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare tls failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand TLS inventory query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let mut records: Vec<_> = match inventory.lock() {
            Ok(inv) => inv.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        records.sort_by_key(|r| std::cmp::Reverse(r.count));
        match serde_json::to_vec(&records) {
            Ok(payload) => {
                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                    tracing::warn!(error = %e, "query: tls reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "query: tls serialize failed"),
        }
    }
}

/// Run the QUIC SNI/ALPN inventory query channel: `zensight/netring/@/query/quic`
/// replies with the passive QUIC Initial inventory (most-seen first) as JSON
/// `Vec<QuicRecord>` (issue #72).
pub async fn run_quic(session: Arc<zenoh::Session>, key_prefix: String, inventory: QuicInventory) {
    let key = zensight_common::command::query_key(&key_prefix, "quic");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare quic failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand QUIC inventory query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let mut records: Vec<_> = match inventory.lock() {
            Ok(inv) => inv.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        records.sort_by_key(|r| std::cmp::Reverse(r.count));
        reply(&query, &records, "quic").await;
    }
}

/// Run the SSH/HASSH inventory query channel: `zensight/netring/@/query/ssh`
/// replies with the passive HASSH inventory (most-seen first) as JSON
/// `Vec<SshRecord>` (issue #72).
pub async fn run_ssh(session: Arc<zenoh::Session>, key_prefix: String, inventory: SshInventory) {
    let key = zensight_common::command::query_key(&key_prefix, "ssh");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare ssh failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand SSH inventory query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let mut records: Vec<_> = match inventory.lock() {
            Ok(inv) => inv.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        records.sort_by_key(|r| std::cmp::Reverse(r.count));
        reply(&query, &records, "ssh").await;
    }
}

/// Run the passive asset-inventory query channel: `zensight/netring/@/query/assets`
/// replies with the discovered assets (most-recently-seen first) as JSON
/// `Vec<AssetRecord>` (issue #70).
pub async fn run_assets(
    session: Arc<zenoh::Session>,
    key_prefix: String,
    inventory: AssetInventory,
) {
    let key = zensight_common::command::query_key(&key_prefix, "assets");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare assets failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand asset inventory query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let mut records: Vec<_> = match inventory.lock() {
            Ok(inv) => inv.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        records.sort_by_key(|r| std::cmp::Reverse(r.last_seen));
        reply(&query, &records, "assets").await;
    }
}

/// Run the top-talkers query channel: `zensight/netring/@/query/talkers?top=N`
/// replies with the top-N destinations by byte volume as JSON `Vec<TalkerRecord>`.
pub async fn run_talkers(session: Arc<zenoh::Session>, key_prefix: String, hist: TalkerHist) {
    let key = zensight_common::command::query_key(&key_prefix, "talkers");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare talkers failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand top-talkers query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let top = top_n(&query);
        let records = match hist.lock() {
            Ok(h) => map::top_talkers(&h, top),
            Err(_) => Vec::new(),
        };
        reply(&query, &records, "talkers").await;
    }
}

/// Run the traffic-matrix query channel (#122):
/// `zensight/netring/@/query/matrix?top=N` replies with the top-N `(src,dst)` pairs
/// by byte volume as JSON `Vec<MatrixRecord>` — the service-map data.
pub async fn run_matrix(session: Arc<zenoh::Session>, key_prefix: String, hist: MatrixHist) {
    let key = zensight_common::command::query_key(&key_prefix, "matrix");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare matrix failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand traffic-matrix query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let top = top_n(&query);
        let records = match hist.lock() {
            Ok(h) => map::traffic_matrix(&h, top),
            Err(_) => Vec::new(),
        };
        reply(&query, &records, "matrix").await;
    }
}

/// Run the elephant-flows query channel: `zensight/netring/@/query/elephant_flows`
/// replies with the recent largest flows (biggest first) as `Vec<ElephantRecord>`.
pub async fn run_elephants(session: Arc<zenoh::Session>, key_prefix: String, ring: ElephantRing) {
    let key = zensight_common::command::query_key(&key_prefix, "elephant_flows");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare elephant_flows failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand elephant-flows query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let mut records: Vec<_> = match ring.lock() {
            Ok(r) => r.iter().cloned().collect(),
            Err(_) => Vec::new(),
        };
        records.sort_by_key(|r| std::cmp::Reverse(r.bytes));
        reply(&query, &records, "elephant_flows").await;
    }
}

/// Run the top-DNS-domains query channel: `zensight/netring/@/query/dns?top=N`
/// replies with the top-N SLDs by query count as JSON `Vec<DnsRecord>`.
pub async fn run_dns(session: Arc<zenoh::Session>, key_prefix: String, inventory: DnsInventory) {
    let key = zensight_common::command::query_key(&key_prefix, "dns");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare dns failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand DNS query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let top = top_n(&query);
        let records = match inventory.lock() {
            Ok(inv) => map::top_dns_records(&inv, top),
            Err(_) => Vec::new(),
        };
        reply(&query, &records, "dns").await;
    }
}

/// Run the top-HTTP-hosts query channel: `zensight/netring/@/query/http?top=N`
/// replies with the top-N hosts by request count as JSON `Vec<HttpHostRecord>`.
pub async fn run_http(session: Arc<zenoh::Session>, key_prefix: String, inventory: HttpInventory) {
    let key = zensight_common::command::query_key(&key_prefix, "http");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare http failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand HTTP query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let top = top_n(&query);
        let records = match inventory.lock() {
            Ok(inv) => map::top_http_hosts(&inv, top),
            Err(_) => Vec::new(),
        };
        reply(&query, &records, "http").await;
    }
}

/// Run the JA4H fingerprint query channel: `zensight/netring/@/query/ja4h?top=N`
/// replies with the top-N JA4H fingerprints by hit count as JSON `Vec<Ja4hRecord>`
/// (#124). The inventory stays empty unless the sensor was built with
/// `--features ja4plus` and `collect.http_fp` is set.
pub async fn run_ja4h(session: Arc<zenoh::Session>, key_prefix: String, inventory: Ja4hInventory) {
    let key = zensight_common::command::query_key(&key_prefix, "ja4h");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare ja4h failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand JA4H query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let top = top_n(&query);
        let records = match inventory.lock() {
            Ok(inv) => map::top_ja4h(&inv, top),
            Err(_) => Vec::new(),
        };
        reply(&query, &records, "ja4h").await;
    }
}

/// Run the canonical IPFIX query channel (#223): `zensight/netring/@/query/ipfix`
/// replies with the recent ended flows as IANA-IE-keyed records
/// (`FlowRecord::to_ipfix_record`) — per-direction deltas (IE 1/2), totals
/// (IE 85/86), `flowEndReason` (IE 136) + the un-collapsed shadow, and the
/// Community ID — as JSON. Only spawned when built with `--features ipfix` and
/// `collect.ipfix`.
#[cfg(feature = "ipfix")]
pub async fn run_ipfix(
    session: Arc<zenoh::Session>,
    key_prefix: String,
    records: crate::monitor::IpfixRing,
) {
    let key = zensight_common::command::query_key(&key_prefix, "ipfix");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare ipfix failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand IPFIX query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        // Snapshot newest-first, then map each to the canonical IANA-IE record.
        let ie: Vec<_> = match records.lock() {
            Ok(r) => r.iter().rev().map(|rec| rec.to_ipfix_record()).collect(),
            Err(_) => Vec::new(),
        };
        reply(&query, &ie, "ipfix").await;
    }
}

/// Serialize `records` to JSON and reply; logs (does not panic) on failure.
async fn reply<T: serde::Serialize>(query: &zenoh::query::Query, records: &T, label: &str) {
    match serde_json::to_vec(records) {
        Ok(payload) => {
            if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                tracing::warn!(error = %e, channel = label, "query reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, channel = label, "query serialize failed"),
    }
}
