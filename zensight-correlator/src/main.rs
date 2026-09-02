//! ZenSight identity correlator daemon.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use zensight_common::config::LoggingConfig;
use zensight_common::{catalog_rpc_key, entities_query_key, names_query_key};

use zensight_correlator::config::CorrelatorConfig;
use zensight_correlator::engine::{CorrelatorState, Engine};
use zensight_correlator::guard::{self, GuardOutcome};
use zensight_correlator::{pdns, publisher, query, subscriber};

/// Cross-sensor identity correlation service for ZenSight.
#[derive(Parser, Debug)]
#[command(name = "zensight-correlator")]
#[command(about = "Merge host evidence into the single-writer entity keyspace")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON5 format).
    #[arg(short, long)]
    config: Option<String>,

    /// Run with synthetic evidence instead of subscribing to the bus (GUI dev).
    #[arg(long)]
    demo: bool,
}

/// How long to wait for the spawned tasks to declare their queryables before
/// asserting `alive` (RFC 04 §5: alive ⇒ callable).
///
/// Mirrors `zensight_sensor_core`'s `DECLARATION_GRACE` and for the same
/// reason: far beyond any local `declare_queryable`, short enough not to delay
/// presence noticeably.
const DECLARATION_GRACE: Duration = Duration::from_secs(2);

/// Bound on the single-instance liveliness probe.
const GUARD_TIMEOUT: Duration = Duration::from_secs(3);

/// Bound on the engine→subscriber channel (backpressure on an evidence flood).
const ENGINE_CHANNEL_CAP: usize = 4096;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = match &args.config {
        Some(path) => CorrelatorConfig::load_from_file(path)?,
        None => CorrelatorConfig::default(),
    };

    init_tracing(&config.logging);
    info!(demo = args.demo, "starting ZenSight correlator");

    // Connect to Zenoh.
    let session = Arc::new(
        zensight_common::session::connect(&config.zenoh)
            .await
            .map_err(|e| anyhow::anyhow!("failed to connect to Zenoh: {e}"))?,
    );

    // Single-writer guard. This wins the election and takes the claim token;
    // it deliberately does NOT declare `alive` — that happens below, once the
    // queryables are serving. See guard.rs, "Election and presence are two
    // steps, on purpose".
    let _claim = match guard::acquire(&session, GUARD_TIMEOUT).await? {
        GuardOutcome::Acquired(claim) => claim,
        GuardOutcome::AlreadyRunning => {
            error!("another correlator instance is already running; exiting");
            std::process::exit(1);
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAP);
    let (op_tx, op_rx) = mpsc::channel::<zensight_correlator::EntityOp>(ENGINE_CHANNEL_CAP);
    // Durable historical passive-DNS records (@catalog/state/pdns, #310).
    let (pdns_tx, pdns_rx) = mpsc::channel::<zensight_common::PdnsRecord>(ENGINE_CHANNEL_CAP);

    // Shared correlation state (engine mutates; queryables read).
    let state = Arc::new(Mutex::new(CorrelatorState::new(config.clone())));

    // Engine.
    let (edge_tx, edge_rx) =
        mpsc::channel::<zensight_correlator::edges::EdgeOp>(ENGINE_CHANNEL_CAP);
    let engine = Engine::new(state.clone(), rx, op_tx)
        .with_pdns(pdns_tx)
        .with_edges(edge_tx);
    let engine_shutdown = shutdown_rx.clone();
    let engine_task = tokio::spawn(async move {
        if let Err(e) = engine.run(engine_shutdown).await {
            error!(error = %e, "engine error");
        }
    });

    // Entity publisher (drains ops → cached AdvancedPublishers + tombstones).
    let pub_session = session.clone();
    let pub_shutdown = shutdown_rx.clone();
    let serialization = config.serialization;
    let publish_task = tokio::spawn(async move {
        if let Err(e) = publisher::run(pub_session, serialization, op_rx, pub_shutdown).await {
            error!(error = %e, "publisher error");
        }
    });

    // Edge publisher (#917): the catalog's resolved relationship graph on
    // @catalog/state/edge/*, with the same lifecycle as entities.
    let edge_task = {
        let s = session.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = publisher::run_edges(s, serialization, edge_rx, sh).await {
                error!(error = %e, "edge publisher error");
            }
        })
    };

    // Historical passive-DNS publisher: durable IP↔name records on
    // @catalog/state/pdns (#310), meant to be captured by a router-hosted
    // storage backend.
    let pdns_task = {
        let s = session.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = pdns::run(s, serialization, pdns_rx, sh).await {
                error!(error = %e, "pdns publisher error");
            }
        })
    };

    // Late-joiner queryables (entities seed + on-demand names).
    let entities_task = {
        let s = session.clone();
        let st = state.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = query::serve_entities(s, st, sh).await {
                error!(error = %e, "entities queryable error");
            }
        })
    };
    let edges_query_task = {
        let s = session.clone();
        let st = state.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = query::serve_edges(s, st, sh).await {
                error!(error = %e, "edges queryable error");
            }
        })
    };
    let names_task = {
        let s = session.clone();
        let st = state.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = query::serve_names(s, st, sh).await {
                error!(error = %e, "names queryable error");
            }
        })
    };

    let introspect_task = {
        let s = session.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = query::serve_introspect(s, sh).await {
                error!(error = %e, "introspect queryable error");
            }
        })
    };

    let describe_task = {
        let s = session.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = query::serve_describe(s, sh).await {
                error!(error = %e, "describe queryable error");
            }
        })
    };

    // Operator identity assertions (#473): link/unlink. Served whether or not
    // they are enabled — a gated procedure that *replies* "gated" tells an
    // operator the feature exists; one that isn't declared just times out.
    let assertion_task = {
        let s = session.clone();
        let st = state.clone();
        let sh = shutdown_rx.clone();
        let allowed = config.allow_operator_assertions;
        tokio::spawn(async move {
            if let Err(e) = query::serve_assertions(s, st, serialization, allowed, sh).await {
                error!(error = %e, "assertion queryable error");
            }
        })
    };

    // Input source: real evidence subscribers, or (in --demo) a synthetic feed
    // driving the exact same engine/store/publisher pipeline.
    let input_task = if args.demo {
        let feed_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            zensight_correlator::demo::feed(tx, feed_shutdown).await;
        })
    } else {
        let sub_session = session.clone();
        let sub_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = subscriber::run(sub_session, tx, sub_shutdown).await {
                error!(error = %e, "subscriber error");
            }
        })
    };

    // Presence, last (RFC 04 §5: `alive` ⇒ callable).
    //
    // Every queryable above is declared inside a spawned task, so reading the
    // served set once here would race them — the same problem, and the same
    // bounded wait, as `SensorRunner`'s `await_registry_coverage`
    // (`DECLARATION_GRACE`, #648). The correlator is not a `SensorRunner`, so
    // it never inherited that discipline: it used to assert `alive` inside the
    // election, before a single queryable existed. On a loaded two-lane CI
    // runner that window is wide enough for a judge's introspect sweep to land
    // inside it, and `zensight-conformance` caught exactly that — which is the
    // gate doing its job.
    // The catalog's callable surface, by the exact keys it declares.
    //
    // NOT `await_registry_coverage`: that helper derives the serve-side
    // spelling from *this host's* origin (`v1/h-…/@rpc/catalog/names`), which
    // is right for a sensor and wrong here — the catalog serves on the
    // `@catalog` SERVICE origin. Point it at this producer and it reports every
    // procedure as unserved while the log says they are ready, then
    // debug-panics. `await_served` takes concrete keys and makes no assumption
    // about how they were spelled.
    let callable = [
        entities_query_key(),
        names_query_key(),
        catalog_rpc_key("introspect"),
        catalog_rpc_key("describe"),
        catalog_rpc_key("link"),
        catalog_rpc_key("unlink"),
    ];
    let missing = zensight_common::served::await_served(&callable, DECLARATION_GRACE).await;
    if !missing.is_empty() {
        // Not fatal: presence with a partial surface is still better than a
        // catalog the fleet cannot see at all, and the conformance judge will
        // say so plainly if it matters. But it must not pass silently.
        error!(
            missing = ?missing,
            "declaring `alive` with queryables still undeclared after {DECLARATION_GRACE:?} —              RFC 04 §5 says alive means callable, so this window is a promise this              process cannot yet keep"
        );
    }
    let _alive = match guard::declare_alive(&session).await {
        Ok(token) => Some(token),
        // A broken liveliness path must not stop the catalog, exactly as it
        // must not stop a sensor's telemetry.
        Err(e) => {
            error!(error = %e, "failed to declare the catalog alive token");
            None
        }
    };

    // Wait for a termination signal.
    wait_for_shutdown().await;
    info!("shutting down");
    let _ = shutdown_tx.send(true);

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = input_task.await;
        let _ = engine_task.await;
        let _ = publish_task.await;
        let _ = edge_task.await;
        let _ = pdns_task.await;
        let _ = entities_task.await;
        let _ = names_task.await;
        let _ = edges_query_task.await;
        let _ = introspect_task.await;
        let _ = describe_task.await;
        let _ = assertion_task.await;
    })
    .await;

    session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("failed to close Zenoh session: {e}"))?;
    info!("correlator stopped");
    Ok(())
}

/// Initialize tracing from the logging config, quieting zenoh internals.
fn init_tracing(logging: &LoggingConfig) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("zensight_correlator={},zenoh=warn", logging.level))
    });
    match logging.format {
        zensight_common::LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        zensight_common::LogFormat::Text => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}

/// Block until Ctrl-C or SIGTERM.
async fn wait_for_shutdown() {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            #[cfg(unix)]
            {
                let mut sigterm = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                ).unwrap();
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {}
    }
}
