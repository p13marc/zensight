//! The `@rpc/parallax/streams` catalogue queryable (late-joiner seed).
//!
//! The GUI calls `zensight/v1/<origin>/@rpc/parallax/streams` when a user
//! opens the parallax device view; the reply is the full
//! `Vec<StreamDescriptor>` catalogue as JSON, with `active` stamped from the
//! session actor's open set. High-cardinality media never rides this channel
//! — it's a small, on-demand table (principle P2).

use std::sync::Arc;

use zensight_common::command::query_key;
use zensight_common::stream::TierSpec;

use crate::catalog::Catalog;
use crate::session::SessionHandle;

/// Run the streams catalogue queryable until the session closes.
///
/// `producer` is the producer name (`"parallax"`); the key is
/// origin-scoped (`zensight/v1/<origin>/@rpc/parallax/streams`). `tiers` is the
/// sensor's tier ladder, stamped into each descriptor (filtered per source).
pub async fn run(
    session: Arc<zenoh::Session>,
    producer: String,
    catalog: Arc<Catalog>,
    tiers: Vec<TierSpec>,
    handle: SessionHandle,
) {
    let key = query_key(&producer, "streams");
    let queryable = match zensight_common::served::serve_queryable(&session, &key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "streams: failed to declare catalogue queryable");
            return;
        }
    };
    tracing::info!(key = %key, streams = catalog.entries().len(), "streams: catalogue queryable ready");

    while let Ok(query) = queryable.recv_async().await {
        // Stamp `active` from the actor's open set (empty if the actor died).
        let open = handle.open_streams().await;
        let descriptors = catalog.descriptors(&open, &tiers);
        match serde_json::to_vec(&descriptors) {
            Ok(payload) => {
                if let Err(e) = query.reply(key.as_str(), payload).await {
                    tracing::warn!(error = %e, "streams: failed to reply to catalogue query");
                }
            }
            Err(e) => tracing::warn!(error = %e, "streams: failed to serialize catalogue"),
        }
    }
    tracing::warn!("streams: catalogue queryable ended");
}
