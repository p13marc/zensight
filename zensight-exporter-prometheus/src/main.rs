//! Prometheus exporter for ZenSight telemetry.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::watch;
use tracing::{Level, error, info};
use tracing_subscriber::EnvFilter;

use zensight_exporter_prometheus::{
    ExporterConfig, HttpServer, MetricCollector, TelemetrySubscriber,
};

/// Prometheus exporter for ZenSight telemetry.
#[derive(Parser, Debug)]
#[command(name = "zensight-exporter-prometheus")]
#[command(about = "Export ZenSight telemetry as Prometheus metrics")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON5 format).
    #[arg(short, long)]
    config: Option<String>,

    /// HTTP listen address (overrides config).
    #[arg(long)]
    listen: Option<String>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load configuration
    let mut config = if let Some(config_path) = &args.config {
        ExporterConfig::load_from_file(config_path)?
    } else {
        ExporterConfig::default()
    };

    // Override listen address from CLI
    if let Some(listen) = args.listen {
        config.prometheus.listen = listen;
    }

    // Initialize logging
    let log_level = args.log_level.parse().unwrap_or(Level::INFO);
    let filter = EnvFilter::from_default_env()
        .add_directive(format!("zensight_exporter_prometheus={}", log_level).parse()?)
        .add_directive(format!("zenoh={}", Level::WARN).parse()?);

    match config.logging.format {
        zensight_exporter_prometheus::config::LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        zensight_exporter_prometheus::config::LogFormat::Text => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    info!("Starting ZenSight Prometheus Exporter");

    // Create shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Create the collector
    let collector = Arc::new(MetricCollector::new(
        config.prometheus.clone(),
        config.aggregation.clone(),
        config.filters.clone(),
    ));

    // Parse listen address
    let listen_addr = config
        .prometheus
        .listen
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid listen address: {}", e))?;

    // Create components
    let subscriber = TelemetrySubscriber::new(collector.clone(), config.zenoh.clone());
    let http_server = HttpServer::new(
        collector.clone(),
        listen_addr,
        config.prometheus.path.clone(),
    );

    // Start cleanup task
    let cleanup_collector = collector.clone();
    let cleanup_interval = Duration::from_secs(config.aggregation.cleanup_interval_secs);
    let mut cleanup_shutdown = shutdown_rx.clone();

    let cleanup_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(cleanup_interval);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    cleanup_collector.cleanup_stale();
                }
                _ = cleanup_shutdown.changed() => {
                    if *cleanup_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });

    // Start subscriber
    let subscriber_shutdown = shutdown_rx.clone();
    let subscriber_task = tokio::spawn(async move {
        if let Err(e) = subscriber.run(subscriber_shutdown).await {
            error!("Subscriber error: {}", e);
        }
    });

    // Start HTTP server
    let http_shutdown = shutdown_rx.clone();
    let http_task = tokio::spawn(async move {
        if let Err(e) = http_server.run(http_shutdown).await {
            error!("HTTP server error: {}", e);
        }
    });

    // Wait for shutdown signal
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
    }

    // Signal shutdown
    shutdown_tx.send(true)?;

    // Wait for tasks to complete
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = subscriber_task.await;
        let _ = http_task.await;
        let _ = cleanup_task.await;
    })
    .await;

    // Print final stats
    let stats = collector.stats();
    info!(
        points_received = stats.points_received,
        points_accepted = stats.points_accepted,
        points_filtered = stats.points_filtered,
        series_count = collector.series_count(),
        "Final statistics"
    );

    info!("Exporter stopped");
    Ok(())
}
