//! OpenTelemetry exporter for ZenSight telemetry.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::watch;
use tracing::{Level, error, info};
use tracing_subscriber::EnvFilter;

use zensight_exporter_otel::{ExporterConfig, OtelExporter, TelemetrySubscriber};

/// OpenTelemetry exporter for ZenSight telemetry.
#[derive(Parser, Debug)]
#[command(name = "zensight-exporter-otel")]
#[command(about = "Export ZenSight telemetry via OpenTelemetry OTLP")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON5 format).
    #[arg(short, long)]
    config: Option<String>,

    /// OTLP endpoint (overrides config).
    #[arg(long)]
    endpoint: Option<String>,

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

    // Override endpoint from CLI
    if let Some(endpoint) = args.endpoint {
        config.opentelemetry.endpoint = endpoint;
    }

    // Initialize logging
    let log_level = args.log_level.parse().unwrap_or(Level::INFO);
    let filter = EnvFilter::from_default_env()
        .add_directive(format!("zensight_exporter_otel={}", log_level).parse()?)
        .add_directive(format!("zenoh={}", Level::WARN).parse()?)
        .add_directive(format!("opentelemetry={}", Level::WARN).parse()?);

    match config.logging.format {
        zensight_exporter_otel::config::LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        zensight_exporter_otel::config::LogFormat::Text => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    info!("Starting ZenSight OpenTelemetry Exporter");
    info!(
        endpoint = %config.opentelemetry.endpoint,
        protocol = ?config.opentelemetry.protocol,
        export_metrics = config.opentelemetry.export_metrics,
        export_logs = config.opentelemetry.export_logs,
        "Configuration loaded"
    );

    // Create shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Create the OTEL exporter
    let exporter = Arc::new(OtelExporter::new(&config.opentelemetry, &config.filters).await?);

    // Create Zenoh subscriber
    let subscriber = TelemetrySubscriber::new(exporter.clone(), config.zenoh.clone());

    // Start subscriber
    let subscriber_shutdown = shutdown_rx.clone();
    let subscriber_task = tokio::spawn(async move {
        if let Err(e) = subscriber.run(subscriber_shutdown).await {
            error!("Subscriber error: {}", e);
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

    // Wait for subscriber to finish
    let _ = tokio::time::timeout(Duration::from_secs(5), subscriber_task).await;

    // Shutdown OTEL exporter
    exporter.shutdown()?;

    // Print final stats
    let stats = exporter.stats();
    info!(
        points_received = stats.points_received,
        points_filtered = stats.points_filtered,
        metrics_exported = stats.metrics_exported,
        logs_exported = stats.logs_exported,
        "Final statistics"
    );

    info!("Exporter stopped");
    Ok(())
}
