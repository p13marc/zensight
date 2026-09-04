//! Reading the fleet from `@catalog` (#938).
//!
//! The catalog is the **only** oracle for what a host is. This daemon has no
//! opinion of its own and must not grow one: it did not run the union-find,
//! and a second answer to "which host is this" is how two components come to
//! configure the same machine differently.

use std::sync::Arc;

use zenoh::Session;
use zensight_common::HostEntity;

/// Fetch the current entity set.
///
/// A GET, not a subscription, because this is the **level-triggered** read the
/// compile pass is built on: it must be able to say "this is the fleet, right
/// now, completely". A subscriber's view is whatever arrived since it started,
/// which for a compiler that deletes documents is the difference between "this
/// host is gone" and "I have not heard from it yet" — and the second must
/// never be read as the first.
///
/// `QueryTarget::All` because several catalog instances may answer during a
/// handover (RFC 05 §2.1 fan-in), and **`ConsolidationMode::None`** so that
/// every one of those answers reaches this loop.
///
/// The consolidation matters more than it looks. Zenoh's default collapses
/// replies **by key expression**, keeping whichever arrived — so with two
/// catalogs mid-handover answering for the same entity, the document that
/// survives would be chosen by the network. That is fine for a UI that upserts
/// into a store; it is not fine here, because the compiled output is
/// content-hashed and published only on a diff. A fleet that differed between
/// two passes for no reason but arrival order would rewrite the documents of
/// every affected host, on a schedule nobody could predict.
///
/// With consolidation off, every reply arrives and `last_updated` decides —
/// deterministically, from the payload, the same rule the catalog itself uses.
pub async fn fetch(session: &Arc<Session>, timeout: std::time::Duration) -> Vec<HostEntity> {
    let key = zensight_common::keyexpr::entities_query_key();
    let Ok(replies) = session
        .get(&key)
        .target(zenoh::query::QueryTarget::All)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .timeout(timeout)
        .await
    else {
        tracing::warn!(key = %key, "catalog entity GET failed");
        return Vec::new();
    };

    let mut by_id: std::collections::BTreeMap<String, HostEntity> = Default::default();
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result()
            && let Ok(e) = zensight_common::decode_auto::<HostEntity>(&sample.payload().to_bytes())
        {
            // Newest wins between two catalogs mid-handover; on a tie, the
            // one that arrived first, so the result does not depend on the
            // order the network happened to deliver equal answers in.
            match by_id.get(&e.entity_id) {
                Some(prev) if prev.last_updated >= e.last_updated => {}
                _ => {
                    by_id.insert(e.entity_id.clone(), e);
                }
            }
        }
    }
    by_id.into_values().collect()
}
