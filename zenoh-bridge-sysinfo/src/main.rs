//! Zenoh bridge for system monitoring.
//!
//! This bridge collects local system metrics (CPU, memory, disk, network)
//! and publishes them to Zenoh as telemetry.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zenoh_bridge_sysinfo::collector::SystemCollector;
use zenoh_bridge_sysinfo::config::SysinfoBridgeConfig;
use zensight_common::serialization::Format;
use zensight_common::LoggingConfig;

/// Zenoh bridge for system monitoring.
#[derive(Parser, Debug)]
#[command(name = "zenoh-bridge-sysinfo")]
#[command(about = "Collects system metrics and publishes to Zenoh")]
#[command(version)]
struct Args {
    /// Path to configuration file (JSON5 format).
    #[arg(short, long, default_value = "sysinfo.json5")]
    config: PathBuf,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load configuration
    let config = SysinfoBridgeConfig::load_from_file(&args.config)
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

    info!("Starting zenoh-bridge-sysinfo");
    info!("Config loaded from {:?}", args.config);

    // Resolve hostname
    let hostname = config.get_hostname();
    info!("Hostname: {}", hostname);

    // Connect to Zenoh
    info!("Connecting to Zenoh...");
    let session = zensight_common::connect(&config.zenoh)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to Zenoh: {}", e))?;
    info!("Connected to Zenoh");

    // Serialization format
    let format = Format::Json;

    // Publish bridge status
    let status_key = format!("{}/@/status", config.sysinfo.key_prefix);
    let status = serde_json::json!({
        "bridge": "sysinfo",
        "version": env!("CARGO_PKG_VERSION"),
        "hostname": hostname,
        "collect": {
            "cpu": config.sysinfo.collect.cpu,
            "memory": config.sysinfo.collect.memory,
            "disk": config.sysinfo.collect.disk,
            "network": config.sysinfo.collect.network,
            "system": config.sysinfo.collect.system,
            "processes": config.sysinfo.collect.processes,
        },
        "poll_interval_secs": config.sysinfo.poll_interval_secs,
        "status": "running"
    });

    if let Err(e) = session.put(&status_key, status.to_string()).await {
        tracing::error!("Failed to publish bridge status: {}", e);
    }

    info!(
        "Sysinfo bridge running (prefix: {}, interval: {}s)",
        config.sysinfo.key_prefix, config.sysinfo.poll_interval_secs
    );

    // Start the collector
    let collector = SystemCollector::new(hostname, config.sysinfo.clone(), session.clone(), format);

    // Run collector in a task
    let collector_handle = tokio::spawn(async move {
        collector.run().await;
    });

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal");

    // Cancel collector
    collector_handle.abort();

    // Publish offline status
    let status = serde_json::json!({
        "bridge": "sysinfo",
        "status": "offline"
    });
    let _ = session.put(&status_key, status.to_string()).await;

    session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to close Zenoh session: {}", e))?;
    info!("Sysinfo bridge stopped");

    Ok(())
}
