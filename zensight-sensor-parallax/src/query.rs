//! The `@/query/streams` catalogue queryable (late-joiner seed).
//!
//! The GUI calls `zensight/parallax/<source>/@/query/streams` when a user
//! opens the parallax device view; the reply is the full
//! `Vec<StreamDescriptor>` catalogue as JSON. High-cardinality media never
//! rides this channel — it's a small, on-demand table (principle P2).

use std::collections::HashSet;
use std::sync::Arc;

use zensight_common::command::query_key;

use crate::catalog::Catalog;

/// Run the streams catalogue queryable until the session closes.
///
/// `host_prefix` is the host-scoped control prefix
/// (`<key_prefix>/<source>`, e.g. `zensight/parallax/hostA`).
pub async fn run(session: Arc<zenoh::Session>, host_prefix: String, catalog: Arc<Catalog>) {
    let key = query_key(&host_prefix, "streams");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "streams: failed to declare catalogue queryable");
            return;
        }
    };
    tracing::info!(key = %key, streams = catalog.entries().len(), "streams: catalogue queryable ready");

    while let Ok(query) = queryable.recv_async().await {
        // No sessions exist yet at this stage of the sensor: nothing is open.
        let open: HashSet<String> = HashSet::new();
        let descriptors = catalog.descriptors(&open);
        match serde_json::to_vec(&descriptors) {
            Ok(payload) => {
                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                    tracing::warn!(error = %e, "streams: failed to reply to catalogue query");
                }
            }
            Err(e) => tracing::warn!(error = %e, "streams: failed to serialize catalogue"),
        }
    }
    tracing::warn!("streams: catalogue queryable ended");
}
