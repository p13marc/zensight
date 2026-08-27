//! Catalog ownership — the RFC 06 §5.3 claim protocol.
//!
//! Zenoh liveliness tokens are presence, not locks (two sessions can hold
//! one key), so exclusivity is an explicit protocol modelled on D-Bus
//! well-known-name ownership:
//!
//! 1. **Claim**: declare a liveliness token at
//!    `…/@catalog/state/claim/<zid>` (our session id).
//! 2. **Election**: query the claim set; the lexically-lowest claim chunk
//!    wins — deterministic and coordinator-free, so simultaneous starts
//!    converge without messages.
//! 3. Losers exit (this implementation does not queue as standby — a
//!    supervisor restart re-runs the election).
//!
//! Only the elected owner declares `…/@catalog/state/alive` and the catalog
//! publishers/queryables. This yields *eventual* single-writer: a partition
//! can elect two owners, and the catalog's pure-function contract makes the
//! split convergent after heal (RFC 06 §5.3's stated trade).
//!
//! # Election and presence are two steps, on purpose
//!
//! [`acquire`] wins the election and returns the claim token. It does **not**
//! declare `alive` — [`declare_alive`] does, and the caller must not call it
//! until the catalog's queryables are actually serving.
//!
//! RFC 04 §5 is `alive ⇒ callable`: asserting presence is a promise to answer.
//! The two used to happen together, so the correlator promised to answer before
//! it had declared a single queryable. On a fast machine that window is
//! microseconds; on a loaded two-lane CI runner it is wide enough for a judge's
//! introspect sweep to land inside it, and `zensight-conformance` caught
//! exactly that. `zensight-sensor-core`'s runner has always done it in this
//! order for the same reason (`DECLARATION_GRACE`, #648) — the correlator is
//! not a `SensorRunner`, so it never inherited the discipline.

use std::sync::Arc;
use std::time::Duration;

use zenoh::Session;
use zenoh::liveliness::LivelinessToken;
use zensight_common::{catalog_claim_key, catalog_claims_wildcard, correlator_alive_key};

/// Outcome of the ownership election.
pub enum GuardOutcome {
    /// We won: holds the claim token (keep it for the process lifetime;
    /// dropping undeclares). Presence is a separate, later step —
    /// [`declare_alive`].
    Acquired(LivelinessToken),
    /// Another catalog instance won the election.
    AlreadyRunning,
}

/// Run the claim protocol. `timeout` bounds the claim-set query so a bus
/// with no other instance doesn't stall startup for zenoh's default 10 s.
pub async fn acquire(session: &Arc<Session>, timeout: Duration) -> anyhow::Result<GuardOutcome> {
    let zid = session.zid().to_string().to_ascii_lowercase();
    let claim_key = catalog_claim_key(&zid);

    // 1. Claim.
    let claim_token = session
        .liveliness()
        .declare_token(claim_key.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare catalog claim token: {e}"))?;

    // 2. Election: collect the live claim set (ours included) and compare.
    let mut lowest: Option<String> = None;
    match session
        .liveliness()
        .get(catalog_claims_wildcard().as_str())
        .timeout(timeout)
        .await
    {
        Ok(replies) => {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    let key = sample.key_expr().as_str();
                    if let Some(claim) = key.rsplit('/').next() {
                        let claim = claim.to_string();
                        if lowest.as_deref().is_none_or(|cur| claim.as_str() < cur) {
                            lowest = Some(claim);
                        }
                    }
                }
            }
        }
        Err(e) => {
            // A failed probe shouldn't hard-block startup, but surface it.
            tracing::warn!(error = %e, "claim-set query failed; assuming sole candidate");
        }
    }

    if let Some(winner) = lowest
        && winner != zid
    {
        tracing::warn!(winner = %winner, ours = %zid, "catalog election lost — another instance owns @catalog");
        drop(claim_token);
        return Ok(GuardOutcome::AlreadyRunning);
    }

    tracing::info!(claim = %claim_key, "catalog election won — this instance owns @catalog");
    Ok(GuardOutcome::Acquired(claim_token))
}

/// Declare the owner `alive` token (RFC 04 §5) — the roster signal consumers
/// read; the claim tokens are protocol machinery.
///
/// **Call this last**, after every queryable is serving. `alive` means
/// *callable*, and a producer that says so before it can answer is lying for
/// the width of that window. See the module header.
pub async fn declare_alive(session: &Arc<Session>) -> anyhow::Result<LivelinessToken> {
    session
        .liveliness()
        .declare_token(correlator_alive_key().as_str())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare catalog alive token: {e}"))
}
