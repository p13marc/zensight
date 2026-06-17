//! On-demand flow detail query channel (principle P2).
//!
//! Serves the bounded ring of recent ended-flow records via a Zenoh queryable at
//! `zensight/netring/@/query/flows` — the high-cardinality 5-tuple/volume detail
//! behind the streamed flow aggregates, pulled only when a user drills in (never
//! streamed onto the telemetry bus).

use std::sync::Arc;

use crate::monitor::FlowRing;

/// Run the flow-detail query channel until the session closes. Replies with the
/// recent ended-flow records (most-recent first) as JSON `Vec<FlowRecord>`.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, flows: FlowRing) {
    let key = format!("{key_prefix}/@/query/flows");
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
