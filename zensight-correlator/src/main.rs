//! ZenSight identity correlator daemon.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use zensight_common::config::LoggingConfig;

use zensight_correlator::config::CorrelatorConfig;
use zensight_correlator::engine::Engine;
use zensight_correlator::guard::{self, GuardOutcome};
use zensight_correlator::subscriber;

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
    info!("starting ZenSight correlator");

    if args.demo {
        // Wired in commit 5.
        info!("demo mode not yet implemented");
        return Ok(());
    }

    // Connect to Zenoh.
    let session = Arc::new(
        zensight_common::session::connect(&config.zenoh)
            .await
            .map_err(|e| anyhow::anyhow!("failed to connect to Zenoh: {e}"))?,
    );

    // Single-writer guard.
    let _token = match guard::acquire(&session, GUARD_TIMEOUT).await? {
        GuardOutcome::Acquired(token) => token,
        GuardOutcome::AlreadyRunning => {
            error!("another correlator instance is already running; exiting");
            std::process::exit(1);
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAP);
    let (op_tx, mut op_rx) = mpsc::channel::<zensight_correlator::EntityOp>(ENGINE_CHANNEL_CAP);

    // Engine.
    let engine = Engine::new(config.clone(), rx, op_tx);
    let engine_shutdown = shutdown_rx.clone();
    let engine_task = tokio::spawn(async move {
        if let Err(e) = engine.run(engine_shutdown).await {
            error!(error = %e, "engine error");
        }
    });

    // Entity-op consumer. Commit 4 replaces this logging drain with the real
    // Zenoh entity publisher + queryables.
    let publish_task = tokio::spawn(async move {
        while let Some(op) = op_rx.recv().await {
            match op {
                zensight_correlator::EntityOp::Upsert(e) => {
                    info!(entity_id = %e.entity_id, members = e.members.len(), "entity upsert")
                }
                zensight_correlator::EntityOp::Tombstone(id) => {
                    info!(entity_id = %id, "entity tombstone")
                }
            }
        }
    });

    // Subscribers.
    let sub_session = session.clone();
    let sub_shutdown = shutdown_rx.clone();
    let status_from_liveness = config.status_from_liveness;
    let sub_task = tokio::spawn(async move {
        if let Err(e) = subscriber::run(sub_session, tx, status_from_liveness, sub_shutdown).await {
            error!(error = %e, "subscriber error");
        }
    });

    // Wait for a termination signal.
    wait_for_shutdown().await;
    info!("shutting down");
    let _ = shutdown_tx.send(true);

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = sub_task.await;
        let _ = engine_task.await;
        let _ = publish_task.await;
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
