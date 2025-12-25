//! gNMI bridge for ZenSight
//!
//! Connects to gNMI-enabled network devices and publishes streaming telemetry to Zenoh.

use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};

use zenoh_bridge_gnmi::{GnmiConfig, GnmiSubscriber};
use zensight_common::{LoggingConfig, init_tracing};

/// gNMI to Zenoh bridge
#[derive(Parser, Debug)]
#[command(name = "zenoh-bridge-gnmi")]
#[command(about = "Bridge gNMI streaming telemetry to Zenoh")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "gnmi.json5")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load configuration
    let config = GnmiConfig::load_from_file(&args.config)?;

    // Initialize logging
    let logging_config = LoggingConfig {
        level: config.logging.level.clone(),
    };
    init_tracing(&logging_config)?;

    info!(
        "Starting gNMI bridge with {} targets",
        config.gnmi.targets.len()
    );

    // Connect to Zenoh
    let session = Arc::new(zensight_common::connect(&config.zenoh).await?);
    info!("Connected to Zenoh");

    // Create subscriber tasks for each target
    let mut tasks = Vec::new();

    for target in config.gnmi.targets {
        let subscriber = GnmiSubscriber::new(
            target.clone(),
            config.gnmi.key_prefix.clone(),
            config.gnmi.serialization,
        );
        let session = session.clone();

        let task = tokio::spawn(async move {
            if let Err(e) = subscriber.run(session).await {
                error!("Subscriber for {} failed: {}", target.name, e);
            }
        });

        tasks.push(task);
    }

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal");

    // Cancel all tasks
    for task in tasks {
        task.abort();
    }

    // Close Zenoh session
    session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to close session: {}", e))?;
    info!("gNMI bridge shutdown complete");

    Ok(())
}
