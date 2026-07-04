//! Single-instance guard.
//!
//! The correlator is the **single writer** of the entity keyspace. Two running
//! instances would (thanks to deterministic merge) publish identical docs —
//! wrong, but not corrupting — so this guard is a warn-and-exit, not a lock: a
//! second instance GETs the well-known liveliness token, finds the first, and
//! exits rather than silently double-writing.

use std::sync::Arc;
use std::time::Duration;

use zenoh::Session;
use zenoh::liveliness::LivelinessToken;
use zensight_common::correlator_alive_key;

/// Outcome of the single-instance check.
pub enum GuardOutcome {
    /// No other instance seen — holds our own liveliness token (keep it alive
    /// for the process lifetime; dropping it undeclares).
    Acquired(LivelinessToken),
    /// Another correlator already holds the token.
    AlreadyRunning,
}

/// Check for an existing correlator via a liveliness GET; if none, declare our
/// own token. `timeout` bounds the GET so a bus with no other instance doesn't
/// stall startup for zenoh's default 10 s.
pub async fn acquire(session: &Arc<Session>, timeout: Duration) -> anyhow::Result<GuardOutcome> {
    let key = correlator_alive_key();

    // Probe for an existing token.
    match session.liveliness().get(&key).timeout(timeout).await {
        Ok(replies) => {
            while let Ok(reply) = replies.recv_async().await {
                if reply.result().is_ok() {
                    return Ok(GuardOutcome::AlreadyRunning);
                }
            }
        }
        Err(e) => {
            // A failed probe shouldn't hard-block startup, but surface it.
            tracing::warn!(error = %e, "liveliness probe failed; proceeding");
        }
    }

    // Claim the token.
    let token = session
        .liveliness()
        .declare_token(&key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare correlator liveliness token: {e}"))?;
    tracing::info!(key = %key, "correlator liveliness token declared (single-writer)");
    Ok(GuardOutcome::Acquired(token))
}
