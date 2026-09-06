//! The historian's read procedures.
//!
//! `stats` is served here, `range`/`series` in [`range`] and `timeline` in
//! [`timeline`] — every procedure the registry declares (#908 was the last).
//!
//! Nothing is `serve_unavailable` any more, and the list that tracked what was
//! is gone with it. RFC 08 §6.1 says a build must serve what it advertises and
//! `check_registry_coverage` fails the startup when it does not, so a
//! procedure added to the TOML without a server is caught two seconds after
//! the mistake rather than the first time someone GETs a key that was never
//! there.

pub mod range;
pub mod timeline;

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
    /// Prune passes in which the ceiling, not the retention, removed history
    /// (#1064).
    pub ceiling_prunes: Arc<AtomicU64>,
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

    let (rows_by_tier, db_bytes, oldest_ts, stored_bytes) = match handle {
        Some(h) => tokio::task::spawn_blocking(move || {
            let rows = Tier::ALL
                .iter()
                .map(|t| TierRows {
                    tier: tier_name(*t).to_string(),
                    rows: h.tier_rows(*t).unwrap_or(0),
                })
                .collect::<Vec<_>>();
            (
                rows,
                h.db_bytes(),
                h.oldest_bucket_ms().ok().flatten(),
                h.stored_bytes().ok(),
            )
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
        stored_bytes,
        ceiling_prunes_total: Some(ctx.ceiling_prunes.load(Ordering::Relaxed)),
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
