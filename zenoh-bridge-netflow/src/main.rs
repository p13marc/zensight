//! Zenoh bridge for NetFlow/IPFIX telemetry.
//!
//! This bridge receives NetFlow (v5, v7, v9) and IPFIX packets,
//! parses flow records, and publishes them to Zenoh as TelemetryPoints.

mod config;
mod receiver;

use anyhow::{Context, Result};
use clap::Parser;
use config::NetFlowBridgeConfig;
use std::path::PathBuf;
use zensight_common::serialization::{encode, Format};
use zensight_common::LoggingConfig;

/// Zenoh bridge for NetFlow/IPFIX telemetry.
#[derive(Parser, Debug)]
#[command(name = "zenoh-bridge-netflow")]
#[command(about = "Receives NetFlow/IPFIX packets and publishes them to Zenoh")]
struct Args {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "netflow.json5")]
    config: PathBuf,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load configuration
    let config = NetFlowBridgeConfig::load_from_file(&args.config)
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

    tracing::info!("Starting zenoh-bridge-netflow");
    tracing::info!("Config loaded from {:?}", args.config);

    // Connect to Zenoh
    let session = zensight_common::connect(&config.zenoh)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to Zenoh: {}", e))?;

    tracing::info!("Connected to Zenoh");

    // Serialization format (default to JSON)
    let format = Format::Json;

    // Start NetFlow listeners
    let mut rx = receiver::start_listeners(&config.netflow)
        .await
        .context("Failed to start NetFlow listeners")?;

    tracing::info!(
        "NetFlow listeners started, publishing to prefix: {}",
        config.netflow.key_prefix
    );

    let key_prefix = config.netflow.key_prefix.clone();
    let publish_flows = config.netflow.publish_flows;

    // Track statistics
    let mut flow_count: u64 = 0;
    let mut last_stats_time = std::time::Instant::now();

    loop {
        tokio::select! {
            Some(record) = rx.recv() => {
                if publish_flows {
                    // Convert to telemetry point
                    let point = receiver::to_telemetry_point(&record);

                    // Build key expression
                    let key = receiver::build_key_expr(&key_prefix, &record);

                    // Serialize and publish
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = session.put(&key, payload).await {
                                tracing::error!("Failed to publish to {}: {}", key, e);
                            } else {
                                tracing::trace!(
                                    "Published flow: {} from {} v{}",
                                    key,
                                    record.exporter_name,
                                    record.version
                                );
                                flow_count += 1;
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to serialize flow: {}", e);
                        }
                    }
                }

                // Log statistics periodically
                if last_stats_time.elapsed().as_secs() >= 60 {
                    tracing::info!(
                        "Processed {} flows in the last minute",
                        flow_count
                    );
                    flow_count = 0;
                    last_stats_time = std::time::Instant::now();
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received shutdown signal");
                break;
            }
        }
    }

    // Graceful shutdown
    session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to close Zenoh session: {}", e))?;
    tracing::info!("Shutdown complete");

    Ok(())
}
