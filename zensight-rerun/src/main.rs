//! Rerun visualization adapter for ZenSight telemetry (evaluation, epic #415).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{Level, error, info};
use tracing_subscriber::EnvFilter;

use zensight_rerun::{RerunConfig, RerunMode, RerunSink, SinkWorker};

/// Rerun visualization adapter for ZenSight telemetry (EVALUATION — #415).
#[derive(Parser, Debug)]
#[command(name = "zensight-rerun")]
#[command(about = "Visualize ZenSight telemetry in Rerun (live gRPC and/or .rrd recording)")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON5 format).
    #[arg(short, long)]
    config: Option<String>,

    /// Sink mode: live | record | both (overrides config).
    #[arg(long)]
    mode: Option<RerunMode>,

    /// Output .rrd path (overrides config; required for record/both).
    #[arg(long)]
    rrd_path: Option<PathBuf>,

    /// Rerun viewer/proxy gRPC URL (overrides config).
    #[arg(long)]
    viewer_url: Option<String>,

    /// Explicit Rerun recording id (overrides config).
    #[arg(long)]
    recording_id: Option<String>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Isolated Zenoh session: scouting off, explicit endpoints only.
    #[arg(long)]
    isolate: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load configuration + apply CLI overrides.
    let mut config = if let Some(config_path) = &args.config {
        RerunConfig::load_from_file(config_path)?
    } else {
        RerunConfig::default()
    };
    if let Some(mode) = args.mode {
        config.rerun.mode = mode;
    }
    if let Some(rrd_path) = args.rrd_path {
        config.rerun.rrd_path = Some(rrd_path);
    }
    if let Some(viewer_url) = args.viewer_url {
        config.rerun.viewer_url = viewer_url;
    }
    if let Some(recording_id) = args.recording_id {
        config.rerun.recording_id = Some(recording_id);
    }
    if args.isolate {
        config.isolate = true;
    }
    config.validate()?;

    // Honor ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN} explicitly (the launcher's
    // rendezvous pins). The isolated session path reads no env itself (so
    // tests/demos stay hermetic) — applying the overrides up front covers it;
    // the non-isolated path re-applies them inside the common connect helper
    // (idempotent).
    config.zenoh = config.zenoh.with_env_overrides();

    // Initialize logging.
    let log_level = args.log_level.parse().unwrap_or(Level::INFO);
    let filter = EnvFilter::from_default_env()
        .add_directive(format!("zensight_rerun={log_level}").parse()?)
        .add_directive(format!("zenoh={}", Level::WARN).parse()?)
        .add_directive(format!("rerun={}", Level::WARN).parse()?);
    match config.logging.format {
        zensight_common::config::LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        zensight_common::config::LogFormat::Text => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    info!("Starting ZenSight Rerun adapter (evaluation, epic #415)");
    info!(
        mode = config.rerun.mode.as_str(),
        viewer_url = %config.rerun.viewer_url,
        rrd_path = ?config.rerun.rrd_path,
        application_id = %config.rerun.application_id,
        recording_id = ?config.rerun.recording_id,
        isolate = config.isolate,
        "Configuration loaded"
    );

    // Rerun sink first: fail fast on a bad path/URL before touching Zenoh.
    let sink = RerunSink::new(&config.rerun)?;

    let session =
        Arc::new(zensight_rerun::session::open_session(&config.zenoh, config.isolate).await?);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx_telemetry, rx_telemetry) = mpsc::channel(zensight_rerun::sink::TELEMETRY_QUEUE);
    let (tx_control, rx_control) = mpsc::channel(zensight_rerun::sink::CONTROL_QUEUE);

    let worker = SinkWorker::new(
        rx_telemetry,
        rx_control,
        Box::new(sink),
        config.rerun.counters,
        config.sampling.clone(),
    );
    let mut worker_task = tokio::spawn(worker.run(shutdown_rx.clone()));

    let mut subscriber_task = tokio::spawn(zensight_rerun::subscriber::run_with_session(
        session,
        config.filters.clone(),
        tx_telemetry,
        tx_control,
        shutdown_rx,
    ));

    // Wait for Ctrl+C / SIGTERM — or either task exiting early (e.g. a bad
    // filters.key_expr failing declare_subscriber). Ignoring task exits here
    // would leave a zombie process and mask the real cause behind a
    // shutdown-channel send error.
    let mut subscriber_result = None;
    let mut worker_result = None;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = async {
            #[cfg(unix)]
            {
                let mut sigterm = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate()
                ).unwrap();
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {
            info!("Received SIGTERM, shutting down...");
        }
        result = &mut subscriber_task => {
            error!("Subscriber task exited unexpectedly, shutting down...");
            subscriber_result = Some(result);
        }
        result = &mut worker_task => {
            error!("Worker task exited unexpectedly, shutting down...");
            worker_result = Some(result);
        }
    }

    // Non-fatal: if both tasks already exited, the receivers are gone and
    // send fails — the interesting error is in the join results below.
    if shutdown_tx.send(true).is_err() {
        info!("Shutdown receivers already gone");
    }

    // Join the subscriber (unless the select above already did) and remember
    // its error so the process exits non-zero on an early failure.
    let mut exit_error: Option<anyhow::Error> = None;
    let subscriber_result = match subscriber_result {
        Some(result) => Some(result),
        None => tokio::time::timeout(Duration::from_secs(5), &mut subscriber_task)
            .await
            .map_err(|_| error!("Subscriber shutdown timed out"))
            .ok(),
    };
    match subscriber_result {
        Some(Ok(Ok(stats))) => info!(
            telemetry_received = stats
                .telemetry_received
                .load(std::sync::atomic::Ordering::Relaxed),
            telemetry_dropped = stats
                .telemetry_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            "Subscriber finished"
        ),
        Some(Ok(Err(e))) => {
            error!("Subscriber error: {e}");
            exit_error = Some(e);
        }
        Some(Err(e)) => {
            error!("Subscriber task panicked: {e}");
            exit_error = Some(e.into());
        }
        None => {}
    }

    // The worker flushes the Rerun stream on exit.
    let worker_result = match worker_result {
        Some(result) => Some(result),
        None => tokio::time::timeout(Duration::from_secs(10), &mut worker_task)
            .await
            .map_err(|_| error!("Worker shutdown timed out"))
            .ok(),
    };
    match worker_result {
        Some(Ok(stats)) => info!(
            metrics = stats.metrics_published,
            events = stats.events_published,
            alerts = stats.alerts_published,
            entities = stats.entities_published,
            ignored_binary = stats.ignored_binary,
            sink_errors = stats.sink_errors,
            "Final statistics"
        ),
        Some(Err(e)) => {
            error!("Worker task panicked: {e}");
            if exit_error.is_none() {
                exit_error = Some(e.into());
            }
        }
        None => {}
    }

    match exit_error {
        Some(e) => Err(e),
        None => {
            info!("Adapter stopped");
            Ok(())
        }
    }
}
