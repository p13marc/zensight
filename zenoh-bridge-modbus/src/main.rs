//! Zenoh bridge for Modbus protocol.
//!
//! This bridge polls Modbus devices (TCP or RTU/serial) and publishes
//! register values to Zenoh as telemetry.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info};
use zenoh_bridge_modbus::config::ModbusBridgeConfig;
use zenoh_bridge_modbus::poller::ModbusPoller;
use zensight_common::LoggingConfig;
use zensight_common::serialization::Format;

/// Zenoh bridge for Modbus protocol (TCP/RTU).
#[derive(Parser, Debug)]
#[command(name = "zenoh-bridge-modbus")]
#[command(about = "Polls Modbus devices and publishes to Zenoh")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON5 format)
    #[arg(short, long, default_value = "modbus.json5")]
    config: PathBuf,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load configuration
    let config = ModbusBridgeConfig::load_from_file(&args.config)
        .with_context(|| format!("Failed to load config from {:?}", args.config))?;

    // Initialize logging
    let log_config = LoggingConfig {
        level: args
            .log_level
            .clone()
            .unwrap_or_else(|| config.logging.level.clone()),
    };
    zensight_common::init_tracing(&log_config)
        .map_err(|e| anyhow::anyhow!("Failed to init tracing: {}", e))?;

    info!("Starting zenoh-bridge-modbus");
    info!("Loaded configuration from {:?}", args.config);

    // Connect to Zenoh
    info!("Connecting to Zenoh...");
    let session = zensight_common::connect(&config.zenoh)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to Zenoh: {}", e))?;
    info!("Connected to Zenoh");

    // Serialization format (default to JSON)
    let format = Format::Json;

    // Start pollers for each device
    let mut tasks = Vec::new();

    for device in &config.modbus.devices {
        let poller = ModbusPoller::new(device.clone(), &config.modbus, session.clone(), format);

        info!(
            "Starting poller for device '{}' ({:?})",
            device.name, device.connection
        );

        tasks.push(tokio::spawn(async move {
            poller.run().await;
        }));
    }

    info!(
        "Modbus bridge running with {} device(s)",
        config.modbus.devices.len()
    );

    // Publish bridge status
    let status_key = format!("{}/@/status", config.modbus.key_prefix);
    let status = serde_json::json!({
        "bridge": "modbus",
        "version": env!("CARGO_PKG_VERSION"),
        "devices": config.modbus.devices.iter().map(|d| &d.name).collect::<Vec<_>>(),
        "status": "running"
    });

    if let Err(e) = session.put(&status_key, status.to_string()).await {
        error!("Failed to publish bridge status: {}", e);
    }

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal");

    // Cancel all poller tasks
    for task in tasks {
        task.abort();
    }

    // Publish offline status
    let status = serde_json::json!({
        "bridge": "modbus",
        "status": "offline"
    });
    let _ = session.put(&status_key, status.to_string()).await;

    session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to close Zenoh session: {}", e))?;
    info!("Modbus bridge stopped");

    Ok(())
}
