//! Zenoh bridge for Syslog telemetry.
//!
//! This bridge receives syslog messages via UDP and TCP,
//! parses them (RFC 3164 and RFC 5424 formats), and publishes
//! them to Zenoh as TelemetryPoints.

mod config;
mod parser;
mod receiver;

use anyhow::{Context, Result};
use clap::Parser;
use config::SyslogBridgeConfig;
use std::path::PathBuf;
use zensight_common::LoggingConfig;
use zensight_common::serialization::{Format, encode};

/// Zenoh bridge for Syslog telemetry.
#[derive(Parser, Debug)]
#[command(name = "zenoh-bridge-syslog")]
#[command(about = "Receives syslog messages and publishes them to Zenoh")]
struct Args {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "syslog.json5")]
    config: PathBuf,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load configuration
    let config = SyslogBridgeConfig::load_from_file(&args.config)
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

    tracing::info!("Starting zenoh-bridge-syslog");
    tracing::info!("Config loaded from {:?}", args.config);

    // Connect to Zenoh
    let session = zensight_common::connect(&config.zenoh)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to Zenoh: {}", e))?;

    tracing::info!("Connected to Zenoh");

    // Determine serialization format (default to JSON)
    let format = Format::Json;

    // Start syslog listeners
    let mut rx = receiver::start_listeners(&config.syslog)
        .await
        .context("Failed to start syslog listeners")?;

    tracing::info!(
        "Syslog listeners started, publishing to prefix: {}",
        config.syslog.key_prefix
    );

    // Process incoming messages
    let key_prefix = config.syslog.key_prefix.clone();
    let include_raw = config.syslog.include_raw_message;

    loop {
        tokio::select! {
            Some(received) = rx.recv() => {
                // Convert to telemetry point
                let point = receiver::to_telemetry_point(&received, include_raw);

                // Build key expression
                let key = receiver::build_key_expr(&key_prefix, &received);

                // Serialize and publish
                match encode(&point, format) {
                    Ok(payload) => {
                        if let Err(e) = session.put(&key, payload).await {
                            tracing::error!("Failed to publish to {}: {}", key, e);
                        } else {
                            tracing::debug!(
                                "Published: {} from {} [{}]",
                                key,
                                received.resolved_hostname,
                                received.message.severity.as_str()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to serialize telemetry: {}", e);
                    }
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
