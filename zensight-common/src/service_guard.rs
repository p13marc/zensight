//! Single-writer ownership for a service origin — the RFC 06 §5.3 claim
//! protocol, shared by `@catalog` and `@desired` (#1104, #1105).
//!
//! Zenoh liveliness tokens are presence, not locks: two sessions can hold one
//! key. Exclusivity is therefore an explicit protocol, modelled on D-Bus
//! well-known-name ownership:
//!
//! 1. **Claim** — declare a liveliness token at `…/state/claim/<zid>`.
//! 2. **Defer to the incumbent** — if a live `…/state/alive` token already
//!    exists, somebody owns this origin. Stand by.
//! 3. **Elect** — with no incumbent, the lexically-lowest live claim wins.
//!    Deterministic and coordinator-free, so simultaneous starts converge
//!    without exchanging messages.
//! 4. **Stand by** — a loser does not exit. It waits for the owner's `alive`
//!    token to drop and campaigns again.
//!
//! # Incumbent-first, and no stepping down
//!
//! The first version elected purely on zid and ran **once**, at startup. Three
//! things followed, and all three are what this module exists to fix:
//!
//! - a second instance started later with a lexically lower zid computed
//!   itself the winner, declared `alive`, and published — while the incumbent
//!   kept publishing too. Every seed GET then returned two replies, and for
//!   `@desired` every sensor flapped between two configurations, silently,
//!   because each instance seeds its `published` diff map from storage and so
//!   reads the other's write as a change to rewrite;
//! - losers **exited**, so killing the winner left no service at all until a
//!   supervisor happened to restart a loser whose zid sorted right;
//! - a claim-set query that timed out logged "assuming sole candidate" and
//!   made the caller a winner, so a slow bus elected everybody.
//!
//! A live incumbent now wins regardless of zid, and the incumbent never steps
//! down for a lower claim. Lowest-zid is a **tie-break for simultaneous
//! starts**, not a standing entitlement: making a lower zid able to displace a
//! running owner would hand the deployment a churn source with no upper bound,
//! and there is nothing better about the instance whose session id sorts first.
//!
//! # Election and presence are two steps, on purpose
//!
//! [`ServiceGuard::campaign`] wins the election and returns the claim token. It
//! does **not** declare `alive` — [`ServiceGuard::declare_alive`] does, and the
//! caller must not call it until its queryables are actually serving. RFC 04 §5
//! is `alive ⇒ callable`: asserting presence is a promise to answer, and the
//! two used to happen together, so the correlator promised to answer before it
//! had declared a single queryable. `zensight-conformance` caught exactly that
//! on a loaded CI runner.

use std::sync::Arc;
use std::time::Duration;

use zenoh::Session;
use zenoh::liveliness::LivelinessToken;

/// How often a standby asks whether the owner is still there.
pub const STANDBY_POLL: Duration = Duration::from_secs(2);

/// How many times a campaign retries an unreadable claim set before standing
/// by. Each attempt costs `timeout`.
const CLAIM_QUERY_ATTEMPTS: u32 = 3;

/// Why a token could not be declared. `zensight-common` carries `thiserror`
/// rather than `anyhow`, and a caller that uses `anyhow` converts for free.
#[derive(Debug, thiserror::Error)]
#[error("{origin}: could not declare the {what} token: {source}")]
pub struct GuardError {
    pub origin: &'static str,
    pub what: &'static str,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

/// Where one service origin's ownership lives.
pub struct ServiceGuard {
    session: Arc<Session>,
    /// This session's own claim key.
    claim_key: String,
    /// The claim-set selector.
    claims_wildcard: String,
    /// The `alive` token that means "somebody owns this and can answer".
    alive_key: String,
    /// This session's zid, lowercased — the election's tie-break value.
    zid: String,
    /// For log lines: `@catalog`, `@desired`.
    origin: &'static str,
}

/// Where this instance stands after a campaign.
pub enum Standing {
    /// We own the origin. Holds the claim token: keep it for as long as
    /// ownership lasts, since dropping it undeclares the claim.
    Owner(LivelinessToken),
    /// Somebody else owns it. `owner` is the incumbent's claim chunk, or
    /// `None` when the claim set could not be read at all.
    StandBy { owner: Option<String> },
}

impl ServiceGuard {
    /// The catalog's guard (`@catalog`).
    pub fn catalog(session: Arc<Session>) -> Self {
        let zid = session.zid().to_string().to_ascii_lowercase();
        Self {
            claim_key: crate::keyexpr::catalog_claim_key(&zid),
            claims_wildcard: crate::keyexpr::catalog_claims_wildcard(),
            alive_key: crate::keyexpr::correlator_alive_key(),
            zid,
            origin: "@catalog",
            session,
        }
    }

    /// The desired-state compiler's guard (`@desired`).
    pub fn desired(session: Arc<Session>) -> Self {
        let zid = session.zid().to_string().to_ascii_lowercase();
        Self {
            claim_key: crate::keyexpr::desired_claim_key(&zid),
            claims_wildcard: crate::keyexpr::desired_claims_wildcard(),
            alive_key: crate::keyexpr::desired_alive_key(),
            zid,
            origin: "@desired",
            session,
        }
    }

    /// This session's zid, as the election compares it.
    pub fn zid(&self) -> &str {
        &self.zid
    }

    /// The origin this guard owns, for messages.
    pub fn origin(&self) -> &'static str {
        self.origin
    }

    /// Whoever currently holds the origin's `alive` token, if anyone.
    ///
    /// This is the incumbent question, and it is asked of `alive` rather than
    /// of the claim set on purpose: a claim says "I would like to own this",
    /// `alive` says "I own this and I can answer".
    pub async fn incumbent(&self, timeout: Duration) -> Option<String> {
        let replies = self
            .session
            .liveliness()
            .get(self.alive_key.as_str())
            .timeout(timeout)
            .await
            .ok()?;
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                return Some(sample.key_expr().to_string());
            }
        }
        None
    }

    /// Run the claim protocol once.
    ///
    /// Declares this session's claim, defers to a live incumbent, and
    /// otherwise elects on the lowest live claim chunk.
    pub async fn campaign(&self, timeout: Duration) -> Result<Standing, GuardError> {
        // 1. Claim, before asking anything: a campaign that queried first
        // could see an empty set, then race another instance doing the same.
        let claim_token = self
            .session
            .liveliness()
            .declare_token(self.claim_key.as_str())
            .await
            .map_err(|e| GuardError {
                origin: self.origin,
                what: "claim",
                source: e,
            })?;

        // 2. An incumbent outranks any zid.
        if let Some(owner) = self.incumbent(timeout).await {
            tracing::info!(
                origin = self.origin, %owner, ours = %self.zid,
                "another instance already owns this origin — standing by"
            );
            drop(claim_token);
            return Ok(Standing::StandBy { owner: Some(owner) });
        }

        // 3. No incumbent: elect on the claim set.
        let Some(claims) = self.claim_set(timeout).await else {
            // "I could not read the claim set" is NOT "I am the only
            // candidate" — that reading is how a slow bus elected everybody.
            // Standing by is self-healing: the caller re-campaigns, and if
            // there really is nobody the next attempt sees our own claim.
            tracing::warn!(
                origin = self.origin,
                "claim set unreadable after {CLAIM_QUERY_ATTEMPTS} attempt(s) — standing by \
                 rather than assuming sole candidacy"
            );
            drop(claim_token);
            return Ok(Standing::StandBy { owner: None });
        };

        let lowest = claims.iter().min();
        if let Some(winner) = lowest
            && winner != &self.zid
        {
            tracing::info!(
                origin = self.origin, winner = %winner, ours = %self.zid,
                "election lost on zid — standing by"
            );
            drop(claim_token);
            return Ok(Standing::StandBy {
                owner: Some(winner.clone()),
            });
        }

        tracing::info!(
            origin = self.origin, claim = %self.claim_key,
            "election won — this instance owns the origin"
        );
        Ok(Standing::Owner(claim_token))
    }

    /// The live claim chunks, or `None` when the set could not be read.
    ///
    /// Retried, because the difference between "nobody else is here" and "the
    /// bus did not answer in time" is the whole of the third bug: a single
    /// failed probe used to be read as sole candidacy.
    async fn claim_set(&self, timeout: Duration) -> Option<Vec<String>> {
        for attempt in 1..=CLAIM_QUERY_ATTEMPTS {
            match self
                .session
                .liveliness()
                .get(self.claims_wildcard.as_str())
                .timeout(timeout)
                .await
            {
                Ok(replies) => {
                    let mut claims = Vec::new();
                    while let Ok(reply) = replies.recv_async().await {
                        if let Ok(sample) = reply.result()
                            && let Some(chunk) = sample.key_expr().as_str().rsplit('/').next()
                        {
                            claims.push(chunk.to_string());
                        }
                    }
                    // Our own claim is declared, so an answer that does not
                    // contain it is an answer we did not really get.
                    if claims.iter().any(|c| c == &self.zid) {
                        return Some(claims);
                    }
                    tracing::debug!(
                        origin = self.origin,
                        attempt,
                        "claim set came back without our own claim; retrying"
                    );
                }
                Err(e) => tracing::debug!(
                    origin = self.origin, attempt, error = %e,
                    "claim-set query failed; retrying"
                ),
            }
        }
        None
    }

    /// Block until nobody holds the origin's `alive` token.
    ///
    /// What a standby does instead of exiting. Returns as soon as the owner is
    /// gone, so takeover costs one poll interval rather than a supervisor
    /// restart and a favourable zid.
    pub async fn wait_for_vacancy(&self, timeout: Duration, poll: Duration) {
        loop {
            if self.incumbent(timeout).await.is_none() {
                tracing::info!(
                    origin = self.origin,
                    "the owner's presence is gone — campaigning again"
                );
                return;
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Declare the owner's `alive` token (RFC 04 §5).
    ///
    /// **Call this last**, after every queryable is serving: `alive` means
    /// *callable*, and a producer that says so before it can answer is lying
    /// for the width of that window.
    pub async fn declare_alive(&self) -> Result<LivelinessToken, GuardError> {
        self.session
            .liveliness()
            .declare_token(self.alive_key.as_str())
            .await
            .map_err(|e| GuardError {
                origin: self.origin,
                what: "alive",
                source: e,
            })
    }
}

#[cfg(test)]
mod tests {

    /// The two origins do not share a claim space — a `@desired` instance must
    /// not be able to lose an election to the catalog.
    #[test]
    fn the_two_service_origins_have_separate_claim_spaces() {
        let zid = "abc123";
        assert_ne!(
            crate::keyexpr::catalog_claim_key(zid),
            crate::keyexpr::desired_claim_key(zid)
        );
        assert!(crate::keyexpr::catalog_claim_key(zid).starts_with("v1/@catalog/"));
        assert!(crate::keyexpr::desired_claim_key(zid).starts_with("v1/@desired/"));
    }

    /// A claim key is matched by its own wildcard, or the election reads an
    /// empty set forever.
    #[test]
    fn a_claim_is_matched_by_its_claim_set_selector() {
        for (key, sel) in [
            (
                crate::keyexpr::catalog_claim_key("ABC123"),
                crate::keyexpr::catalog_claims_wildcard(),
            ),
            (
                crate::keyexpr::desired_claim_key("ABC123"),
                crate::keyexpr::desired_claims_wildcard(),
            ),
        ] {
            let ke = zenoh::key_expr::KeyExpr::try_from(sel.clone()).unwrap();
            assert!(
                ke.intersects(&zenoh::key_expr::KeyExpr::try_from(key.clone()).unwrap()),
                "{sel} does not match {key}"
            );
        }
    }

    /// The zid is lowercased on the wire, so the election compares what it
    /// declared. `Chunk::slug` escapes rather than folds case, so an
    /// upper-case zid would claim one key and search for another.
    #[test]
    fn a_claim_key_is_lowercase() {
        assert!(crate::keyexpr::catalog_claim_key("ABCdef").ends_with("abcdef"));
        assert!(crate::keyexpr::desired_claim_key("ABCdef").ends_with("abcdef"));
    }
}
