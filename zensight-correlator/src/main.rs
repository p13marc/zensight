//! ZenSight identity correlator daemon.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use zensight_common::config::LoggingConfig;

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

    // Single-writer guard.
    let _tokens = match guard::acquire(&session, GUARD_TIMEOUT).await? {
        GuardOutcome::Acquired(claim, alive) => (claim, alive),
        GuardOutcome::AlreadyRunning => {
            error!("another correlator instance is already running; exiting");
            std::process::exit(1);
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAP);
    let (op_tx, op_rx) = mpsc::channel::<zensight_correlator::EntityOp>(ENGINE_CHANNEL_CAP);
    // Durable historical passive-DNS records (@pdns, #310).
    let (pdns_tx, pdns_rx) = mpsc::channel::<zensight_common::PdnsRecord>(ENGINE_CHANNEL_CAP);

    // Shared correlation state (engine mutates; queryables read).
    let state = Arc::new(Mutex::new(CorrelatorState::new(config.clone())));

    // Engine.
    let engine = Engine::new(state.clone(), rx, op_tx).with_pdns(pdns_tx);
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

    // Historical passive-DNS publisher: durable IP↔name records on @pdns (#310),
    // meant to be captured by a router-hosted storage backend.
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
        let status_from_liveness = config.status_from_liveness;
        tokio::spawn(async move {
            if let Err(e) =
                subscriber::run(sub_session, tx, status_from_liveness, sub_shutdown).await
            {
                error!(error = %e, "subscriber error");
            }
        })
    };

    // Wait for a termination signal.
    wait_for_shutdown().await;
    info!("shutting down");
    let _ = shutdown_tx.send(true);

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = input_task.await;
        let _ = engine_task.await;
        let _ = publish_task.await;
        let _ = pdns_task.await;
        let _ = entities_task.await;
        let _ = names_task.await;
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
