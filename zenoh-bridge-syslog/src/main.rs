//! Zenoh bridge for Syslog telemetry.
//!
//! This bridge receives syslog messages via UDP and TCP,
//! parses them (RFC 3164 and RFC 5424 formats), and publishes
//! them to Zenoh as TelemetryPoints.

mod config;
mod parser;
mod receiver;

use anyhow::Result;
use config::SyslogBridgeConfig;
use zensight_bridge_framework::{BridgeArgs, BridgeRunner};
use zensight_common::serialization::{Format, encode};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = BridgeArgs::parse_with_default("syslog.json5");

    // Load configuration
    let config = SyslogBridgeConfig::load_from_file(&args.config)?;

    // Create the bridge runner
    let runner = BridgeRunner::new_with_args("syslog", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

    // Get session and config for the receiver
    let session = runner.session().clone();
    let syslog_config = runner.config().syslog.clone();

    // Determine serialization format (default to JSON)
    let format = Format::Json;

    // Start syslog listeners
    let mut rx = receiver::start_listeners(&syslog_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start syslog listeners: {}", e))?;

    tracing::info!(
        "Syslog listeners started, publishing to prefix: {}",
        syslog_config.key_prefix
    );

    // Process incoming messages
    let key_prefix = syslog_config.key_prefix.clone();
    let include_raw = syslog_config.include_raw_message;

    // Build status metadata
    let metadata = serde_json::json!({
        "listeners": syslog_config.listeners.iter().map(|l| {
            format!("{}://{}", l.protocol, l.bind)
        }).collect::<Vec<_>>(),
        "include_raw_message": include_raw,
    });

    // Spawn the message processing task
    let session_clone = session.clone();
    let mut runner = runner;
    runner.spawn(async move {
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
                            if let Err(e) = session_clone.put(&key, payload).await {
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
                else => break,
            }
        }
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
