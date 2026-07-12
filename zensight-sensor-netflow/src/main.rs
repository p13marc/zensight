//! Zenoh sensor for NetFlow/IPFIX telemetry.
//!
//! This sensor receives NetFlow (v5, v7, v9) and IPFIX packets,
//! parses flow records, and publishes them to Zenoh as TelemetryPoints.

mod config;
mod receiver;

use anyhow::Result;
use config::NetFlowSensorConfig;
use zensight_common::serialization::encode;
use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("netflow.json5");

    // Load configuration
    let config = NetFlowSensorConfig::load_from_file(&args.config)?;
    let source = config.netflow.resolved_source();

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("netflow", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

    // On-demand debug-report (`@/artifact`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "netflow",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    // Tier-2 directory snapshots. No-op unless the corresponding kind is enabled.
    let artifacts = runner.config().artifact_limits();
    let runner = runner.with_identity().with_artifacts(vec![
        std::sync::Arc::new(zensight_sensor_core::ReportProducer::new(
            report_source,
            &artifacts.report,
        )) as std::sync::Arc<dyn zensight_sensor_core::ArtifactProducer>,
        std::sync::Arc::new(zensight_sensor_core::SnapshotProducer::new(
            &artifacts.snapshot,
        )),
    ]);

    // Get session and config
    let session = runner.session().clone();
    let netflow_config = runner.config().netflow.clone();

    // Serialization format (from config; default CBOR)
    let format = runner.config().serialization;

    // Start NetFlow listeners
    let mut rx = receiver::start_listeners(&netflow_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start NetFlow listeners: {}", e))?;

    tracing::info!(
        "NetFlow listeners started, publishing to prefix: {}",
        netflow_config.key_prefix
    );

    let key_prefix = zensight_sensor_core::v1::v1_telemetry_prefix(&netflow_config.key_prefix);
    let publish_flows = netflow_config.publish_flows;

    // Build status metadata
    let metadata = serde_json::json!({
        "listeners": netflow_config.listeners.iter().map(|l| &l.bind).collect::<Vec<_>>(),
        "publish_flows": publish_flows,
        "publish_stats": netflow_config.publish_stats,
    });

    // Spawn the flow processing task. Telemetry goes through declared publishers
    // (declare-on-first-use + cache per key, drop QoS), never a one-shot put.
    let registry = zensight_common::PublisherRegistry::new(session.clone());
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
                                if let Err(e) = registry
                                    .put(&key, payload, zensight_common::QosClass::Telemetry)
                                    .await
                                {
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
