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

/// Retry `fetch_once` while it comes back empty, until `deadline` elapses.
///
/// **Why one GET is not enough, even after `await_peer` (#1045).** `await_peer`
/// returns as soon as the session has a neighbour, and that proves a *link* —
/// not a route to a queryable. Zenoh declares queryables to a new session after
/// the link comes up, so there is a window in which a session is connected,
/// `@catalog` is running, and a GET still reaches nobody. `fetch` returns
/// `vec![]` for that and for a genuinely empty fleet, and the caller cannot
/// tell them apart. #1039 made `apply` refuse the ambiguity loudly instead of
/// publishing nothing under exit 0; this makes it stop hitting the ambiguity
/// for a reason that resolves itself in under a second.
///
/// **Empty stays a legitimate answer.** A fleet with no hosts still comes back
/// empty — after the deadline, having actually looked. That is the cost of the
/// distinction and it is paid only by deployments that have nothing to compile
/// for, which are the ones about to be told so.
///
/// It is written over a closure rather than a `Session` so the window it exists
/// for can be tested without a bus: a source that answers empty twice and then
/// non-empty is exactly the sequence CI hits.
pub async fn settle<F, Fut>(
    mut fetch_once: F,
    deadline: std::time::Duration,
    poll: std::time::Duration,
) -> Vec<HostEntity>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Vec<HostEntity>>,
{
    let give_up = tokio::time::Instant::now() + deadline;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let fleet = fetch_once().await;
        if !fleet.is_empty() {
            if attempts > 1 {
                tracing::info!(
                    attempts,
                    hosts = fleet.len(),
                    "@catalog answered after the session settled"
                );
            }
            return fleet;
        }
        if tokio::time::Instant::now() + poll >= give_up {
            tracing::debug!(
                attempts,
                deadline_ms = deadline.as_millis() as u64,
                "@catalog stayed empty for the whole settle window"
            );
            return fleet;
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn entity(id: &str) -> HostEntity {
        HostEntity {
            entity_id: id.to_string(),
            aliases: Vec::new(),
            host_id: Some(id.to_string()),
            boot_id: None,
            ips: Vec::new(),
            macs: Vec::new(),
            container_ids: Vec::new(),
            origins: vec![id.to_string()],
            hostname: None,
            fqdn: None,
            names: Vec::new(),
            vendor: None,
            platform: None,
            members: Vec::new(),
            status: None,
            last_updated: 0,
        }
    }

    #[tokio::test]
    async fn an_answer_that_arrives_late_is_still_an_answer() {
        // The #1045 window: the session has a link, the queryable is not yet
        // visible on it, and one GET would have concluded "no hosts".
        let calls = Mutex::new(0u32);
        let fleet = settle(
            || async {
                let mut n = calls.lock().unwrap();
                *n += 1;
                if *n < 3 {
                    Vec::new()
                } else {
                    vec![entity("h-1")]
                }
            },
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(20),
        )
        .await;
        assert_eq!(fleet.len(), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            3,
            "should stop as soon as it answers"
        );
    }

    #[tokio::test]
    async fn a_first_answer_is_taken_without_waiting() {
        // The ordinary path must cost nothing: a settled session answers on
        // attempt one and the deadline is never involved.
        let calls = Mutex::new(0u32);
        let fleet = settle(
            || async {
                *calls.lock().unwrap() += 1;
                vec![entity("h-1")]
            },
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(20),
        )
        .await;
        assert_eq!(fleet.len(), 1);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_genuinely_empty_fleet_still_comes_back_empty() {
        // The distinction this makes must not become a way of never saying no.
        // An empty fleet is a real answer; it just has to be waited for.
        let calls = Mutex::new(0u32);
        let fleet = settle(
            || async {
                *calls.lock().unwrap() += 1;
                Vec::new()
            },
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(fleet.is_empty());
        // It retried rather than giving up after one look, and it stopped
        // rather than spinning: with a 200 ms deadline and a 50 ms poll that is
        // a handful of attempts, not one and not forever.
        let n = *calls.lock().unwrap();
        assert!((2..=5).contains(&n), "attempts = {n}");
    }
}
