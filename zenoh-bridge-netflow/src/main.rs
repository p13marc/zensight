//! Zenoh bridge for NetFlow/IPFIX telemetry.
//!
//! This bridge receives NetFlow (v5, v7, v9) and IPFIX packets,
//! parses flow records, and publishes them to Zenoh as TelemetryPoints.

mod config;
mod receiver;

use anyhow::Result;
use config::NetFlowBridgeConfig;
use zensight_bridge_framework::{BridgeArgs, BridgeRunner};
use zensight_common::serialization::{Format, encode};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = BridgeArgs::parse_with_default("netflow.json5");

    // Load configuration
    let config = NetFlowBridgeConfig::load_from_file(&args.config)?;

    // Create the bridge runner
    let runner = BridgeRunner::new_with_args("netflow", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

    // Get session and config
    let session = runner.session().clone();
    let netflow_config = runner.config().netflow.clone();

    // Serialization format (default to JSON)
    let format = Format::Json;

    // Start NetFlow listeners
    let mut rx = receiver::start_listeners(&netflow_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start NetFlow listeners: {}", e))?;

    tracing::info!(
        "NetFlow listeners started, publishing to prefix: {}",
        netflow_config.key_prefix
    );

    let key_prefix = netflow_config.key_prefix.clone();
    let publish_flows = netflow_config.publish_flows;

    // Build status metadata
    let metadata = serde_json::json!({
        "listeners": netflow_config.listeners.iter().map(|l| &l.bind).collect::<Vec<_>>(),
        "publish_flows": publish_flows,
        "publish_stats": netflow_config.publish_stats,
    });

    // Spawn the flow processing task
    let session_clone = session.clone();
    let mut runner = runner;
    runner.spawn(async move {
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
                                if let Err(e) = session_clone.put(&key, payload).await {
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
