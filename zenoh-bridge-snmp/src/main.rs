mod config;
mod oid;
mod poller;
mod trap;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal;

use zensight_common::{connect, init_tracing};

use crate::config::SnmpBridgeConfig;
use crate::poller::SnmpPoller;
use crate::trap::TrapReceiver;

/// Zenoh bridge for SNMP telemetry.
#[derive(Parser, Debug)]
#[command(name = "zenoh-bridge-snmp")]
#[command(about = "Bridge SNMP telemetry to Zenoh", long_about = None)]
struct Args {
    /// Path to the configuration file (JSON5 format).
    #[arg(short, long, default_value = "snmp.json5")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load configuration
    let config = SnmpBridgeConfig::load(&args.config)
        .with_context(|| format!("Failed to load config from {:?}", args.config))?;

    // Initialize tracing
    init_tracing(&config.logging).context("Failed to initialize tracing")?;

    tracing::info!(
        config = ?args.config,
        devices = config.snmp.devices.len(),
        "Starting zenoh-bridge-snmp"
    );

    // Connect to Zenoh
    let session = Arc::new(
        connect(&config.zenoh)
            .await
            .context("Failed to connect to Zenoh")?,
    );

    // Spawn device pollers
    let mut tasks = Vec::new();

    for device in config.snmp.devices.clone() {
        let poller = SnmpPoller::new(
            device,
            session.clone(),
            &config.snmp.key_prefix,
            &config.snmp.oid_names,
            &config.snmp.oid_groups,
            config.serialization,
        );

        tasks.push(tokio::spawn(async move {
            poller.run().await;
        }));
    }

    // Spawn trap receiver if enabled
    if config.snmp.trap_listener.enabled {
        let trap_receiver = TrapReceiver::new(
            &config.snmp.trap_listener.bind,
            session.clone(),
            &config.snmp.key_prefix,
            &config.snmp.oid_names,
            config.serialization,
        );

        tasks.push(tokio::spawn(async move {
            if let Err(e) = trap_receiver.run().await {
                tracing::error!(error = %e, "Trap receiver failed");
            }
        }));
    }

    tracing::info!("Bridge running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    signal::ctrl_c().await?;

    tracing::info!("Shutting down...");

    // Abort all tasks
    for task in tasks {
        task.abort();
    }

    // Close Zenoh session
    if let Err(e) = session.close().await {
        tracing::warn!(error = %e, "Error closing Zenoh session");
    }

    tracing::info!("Goodbye!");

    Ok(())
}
