//! The historian's read procedures.
//!
//! `stats` is served here, because it describes the ingest this crate performs
//! and #911 needs it to say whether the retention defaults survive a real
//! fleet. `range`, `series` and `timeline` are **declared** by the registry
//! (#905) and not yet built (#907, #908), so they are served as
//! `error/unsupported` through [`zensight_common::served::serve_unavailable`].
//!
//! That is not a formality. RFC 08 §6.1 says a build must serve what it
//! advertises, and `check_registry_coverage` debug-panics when it does not —
//! but the reason the rule exists is the caller's: a key nobody declared times
//! out, and a timeout is indistinguishable from a slow fleet, a dropped
//! sample, or a wrong key. `error/unsupported` is the third answer that says
//! *declared, not built* rather than *no data*, and it arrives immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use zensight_common::history::{HistorianStats, TierRows};
use zensight_common::rpc::{RpcError, RpcRequest, RpcResult};
use zensight_store::Tier;

use crate::ingest::{IngestCounters, SharedStore};

/// Everything `stats` needs that is not in the store itself.
#[derive(Clone)]
pub struct StatsContext {
    /// This historian's own origin, so a caller merging several replies can
    /// say which said what.
    pub historian: String,
    pub store: SharedStore,
    pub counters: Arc<IngestCounters>,
    pub last_prune_ms: Arc<AtomicU64>,
}

/// Serve `@rpc/historian/stats`.
pub async fn serve_stats(
    session: Arc<zenoh::Session>,
    ctx: &zensight_sensor_core::v1::V1Context,
    stats: StatsContext,
) -> zensight_sensor_core::Result<tokio::task::JoinHandle<()>> {
    zensight_sensor_core::rpc::serve(session, ctx, &["stats"], move |_req: RpcRequest| {
        let stats = stats.clone();
        async move { collect(stats).await }
    })
    .await
}

/// Gather the store's numbers. The redb walks run off the runtime.
async fn collect(ctx: StatsContext) -> RpcResult {
    let (handle, series) = {
        let s = ctx.store.lock().unwrap_or_else(|e| e.into_inner());
        (s.persistent(), s.interner().len() as u64)
    };

    let (rows_by_tier, db_bytes, oldest_ts) = match handle {
        Some(h) => tokio::task::spawn_blocking(move || {
            let rows = Tier::ALL
                .iter()
                .map(|t| TierRows {
                    tier: tier_name(*t).to_string(),
                    rows: h.tier_rows(*t).unwrap_or(0),
                })
                .collect::<Vec<_>>();
            (rows, h.db_bytes(), h.oldest_bucket_ms().ok().flatten())
        })
        .await
        .map_err(|e| RpcError::new("error/historian/stats", format!("stats task failed: {e}")))?,
        // Memory-only: the tiers are empty and the file is not there. Reported
        // as zeroes rather than omitted, because "no database" and "an empty
        // database" are both answers and neither is silence.
        None => (
            Tier::ALL
                .iter()
                .map(|t| TierRows {
                    tier: tier_name(*t).to_string(),
                    rows: 0,
                })
                .collect(),
            0,
            None,
        ),
    };

    let reply = HistorianStats {
        historian: ctx.historian.clone(),
        series,
        rows_by_tier,
        db_bytes,
        dropped_total: ctx.counters.dropped_total(),
        last_prune_ms: match ctx.last_prune_ms.load(Ordering::Relaxed) {
            0 => None, // no prune has run yet — absent, not "instant"
            ms => Some(ms),
        },
        oldest_ts,
    };
    serde_json::to_vec(&reply)
        .map_err(|e| RpcError::new("error/historian/stats", format!("encode failed: {e}")))
}

/// The wire token for a tier.
fn tier_name(t: Tier) -> &'static str {
    match t {
        Tier::Second => "second",
        Tier::Minute => "minute",
        Tier::Hour => "hour",
    }
}

/// The procedures this build declares but does not implement yet, with the
/// issue that will.
pub const UNIMPLEMENTED: &[(&str, &str)] =
    &[("range", "#907"), ("series", "#907"), ("timeline", "#908")];

/// Declare one not-yet-implemented procedure, so that "not built" is an answer
/// rather than a timeout.
///
/// **One procedure per call, and each one spawned.**
/// [`zensight_common::served::serve_unavailable`] owns its reply loop and runs
/// until the session closes, so a loop that awaited it per procedure would
/// declare the first key and never reach the second. That is not a
/// hypothetical: it is what this did first, and `await_registry_coverage`
/// caught it at startup with "registry advertises procedures series, timeline
/// that this build does not serve" — the RFC 08 §6.1 check doing exactly its
/// job, two minutes after the mistake rather than the first time someone
/// GET a key that was never there.
pub async fn serve_unimplemented(
    session: Arc<zenoh::Session>,
    ctx: zensight_sensor_core::v1::V1Context,
    procedure: &'static str,
    issue: &'static str,
) {
    let key = match ctx.rpc_key(&[procedure]) {
        Ok(k) => k.to_string(),
        Err(e) => {
            tracing::warn!(procedure, error = %e, "historian: cannot build procedure key");
            return;
        }
    };
    let err = RpcError::unsupported(format!(
        "historian/{procedure} is declared by this build's registry slice but not \
         implemented in it yet ({issue}). This is an answer, not an outage: the data may \
         well be here, and a later build will serve it on this same key."
    ));
    zensight_common::served::serve_unavailable(session, vec![key], err).await;
}
