//! The historian binary (#906, epic #898).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};

use zensight_historian::config::{HistorianSensorConfig, resolve_store_path};
use zensight_historian::{PRODUCER, ingest, query};

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("historian.json5");
    let config = HistorianSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let source = config.resolved_source();
    let hc = config.historian.clone();

    let mut runner = SensorRunner::new_with_args(PRODUCER, source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();

    // ── the store ────────────────────────────────────────────────────────
    //
    // Degrading to memory-only is never fatal: a historian that cannot open
    // its file still answers live questions from the hot ring, and saying so
    // once at startup beats refusing to run at all. It is a loud warning, not
    // a silent one — an operator who wanted durable history and got a ring
    // must be able to find out without reading the source.
    let path = resolve_store_path(hc.store.path.as_deref());
    let persistent = match &path {
        Some(p) => {
            match zensight_store::PersistentStore::open_with_cache(p, hc.store.cache_bytes) {
                Ok(s) => {
                    tracing::info!(path = %p.display(), "historian: opened the history store");
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e,
                    "historian: could not open the history store; running MEMORY-ONLY — the \
                     hot ring will answer live questions and nothing will survive a restart");
                    None
                }
            }
        }
        None => {
            tracing::warn!(
                "historian: no state directory available (set historian.store.path, or run \
                 under systemd with StateDirectory=); running MEMORY-ONLY"
            );
            None
        }
    };
    let store: ingest::SharedStore = Arc::new(std::sync::Mutex::new(
        zensight_store::MetricStore::new(hc.store.hot_secs, persistent),
    ));

    let counters = Arc::new(ingest::IngestCounters::default());
    let last_prune_ms = Arc::new(AtomicU64::new(0));
    let shedding = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── the governor (#811/#812) ─────────────────────────────────────────
    //
    // All three steps, because this is the component that holds a database on
    // a 1–2 GB VM. The hot ring is the evictable table; ingest is the
    // degradable work.
    {
        let governor = runner.governor();
        let s = store.clone();
        governor.register_table(zensight_sensor_core::governor::TableHandle {
            name: "hot_ring".to_string(),
            stats: Box::new({
                let s = s.clone();
                move || {
                    let g = s.lock().unwrap_or_else(|e| e.into_inner());
                    zensight_common::health::TableStats {
                        name: "hot_ring".to_string(),
                        entries: g.hot_sample_count() as u64,
                        bytes: Some(g.hot_sample_count() as u64 * HOT_SAMPLE_BYTES),
                        capacity_entries: None,
                        capacity_bytes: None,
                    }
                }
            }),
            evict: Some(Box::new(move |want| {
                // Halve the ring rather than free a byte count: the ring is
                // per-series and bounded by capacity, so "free N bytes" is
                // only expressible as "hold fewer seconds".
                let mut g = s.lock().unwrap_or_else(|e| e.into_inner());
                let before = g.hot_sample_count();
                g.halve_hot_capacity();
                let freed = (before - g.hot_sample_count()) as u64 * HOT_SAMPLE_BYTES;
                tracing::info!(want, freed, "historian: halved the hot ring under pressure");
                zensight_sensor_core::governor::EvictOutcome {
                    entries: (before - g.hot_sample_count()) as u64,
                    bytes: freed,
                }
            })),
        });
        let shed = shedding.clone();
        governor.register_degradable(
            "ingest",
            Box::new(move |on| {
                // Bools first: a 0/1 step series is the cheapest history to
                // lose and the easiest to re-derive — the alert that made it
                // interesting is on the bus anyway.
                shed.store(on, std::sync::atomic::Ordering::Relaxed);
                if on {
                    tracing::warn!("historian: shedding boolean series under memory pressure");
                } else {
                    tracing::info!("historian: no longer shedding");
                }
            }),
        );
    }

    // The health document reports the store, so `self_stats` says what the
    // tiers hold rather than only what the process weighs.
    {
        let s = store.clone();
        runner.health().register_table_stats(Box::new(move || {
            let g = s.lock().unwrap_or_else(|e| e.into_inner());
            vec![zensight_common::health::TableStats {
                name: "series".to_string(),
                entries: g.interner().len() as u64,
                bytes: None,
                capacity_entries: None,
                capacity_bytes: None,
            }]
        }));
    }

    // ── procedures, BEFORE run() ─────────────────────────────────────────
    //
    // `run_with_metadata` waits for registry coverage (RFC 08 §6.1) with a
    // two-second grace and debug-panics if the slice advertises a procedure
    // nothing declared. Serving after `run()` would race that, and "alive ⇒
    // callable" is the property the conformance judges check.
    let ctx = zensight_sensor_core::v1::for_producer(PRODUCER);
    let stats_ctx = query::StatsContext {
        historian: ctx.origin().chunk().to_string(),
        store: store.clone(),
        counters: counters.clone(),
        last_prune_ms: last_prune_ms.clone(),
    };
    query::serve_stats(runner.session().clone(), &ctx, stats_ctx)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    query::range::serve_range(
        runner.session().clone(),
        ctx.clone(),
        store.clone(),
        ctx.origin().chunk().to_string(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    query::range::serve_series(runner.session().clone(), ctx.clone(), store.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // `serve_unavailable` owns its reply loops and runs until the session
    // closes, so it is spawned rather than awaited — the netring pattern
    // (`main.rs:468`). The declaration itself happens on the first poll, well
    // inside `DECLARATION_GRACE`, so registry coverage still sees the keys.
    for (procedure, issue) in query::UNIMPLEMENTED {
        runner.spawn(query::serve_unimplemented(
            runner.session().clone(),
            ctx.clone(),
            procedure,
            issue,
        ));
    }

    // ── the loops ────────────────────────────────────────────────────────
    runner.spawn(ingest::run(
        runner.session().clone(),
        hc.key_expr.clone(),
        store.clone(),
        counters.clone(),
        shedding.clone(),
        shutdown_rx.clone(),
    ));
    runner.spawn(ingest::flush_loop(
        store.clone(),
        Duration::from_secs(hc.store.flush_interval_secs),
        shutdown_rx.clone(),
    ));
    runner.spawn(ingest::prune_loop(
        store.clone(),
        Duration::from_secs(hc.store.prune_interval_secs),
        last_prune_ms.clone(),
        shutdown_rx.clone(),
    ));

    tracing::info!(
        key_expr = %hc.key_expr,
        store = %path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "memory-only".into()),
        hot_secs = hc.store.hot_secs,
        minute_days = hc.store.retention.minute_days,
        hour_days = hc.store.retention.hour_days,
        budget_mb = hc.resources.budget_rss_mb.unwrap_or(0),
        "historian running — it ingests fleet telemetry and answers questions about it. It \
         publishes NO telemetry of its own (RFC 04 §1.1) and has no write surface."
    );

    let result = runner
        .run_with_metadata(Some(serde_json::json!({
            "key_expr": hc.key_expr,
            "durable": path.is_some(),
            "retention_minute_days": hc.store.retention.minute_days,
            "retention_hour_days": hc.store.retention.hour_days,
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));

    // Stop the loops and take one last flush: a service told to stop should
    // not discard samples it has already accepted.
    let _ = shutdown_tx.send(true);
    ingest::flush_once(&store).await;
    result
}

/// Rough bytes per in-memory sample, for the governor's accounting: a
/// `Sample` is 16 bytes, and the `VecDeque` slot plus per-series overhead
/// rounds it to 24. An estimate, and labelled as one — the governor needs a
/// number that moves with the ring, not an exact heap measurement.
const HOT_SAMPLE_BYTES: u64 = 24;
