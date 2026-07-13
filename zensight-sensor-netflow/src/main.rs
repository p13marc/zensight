//! Zenoh sensor for NetFlow/IPFIX telemetry.
//!
//! Receives NetFlow (v5, v7, v9) and IPFIX packets, folds the flow records
//! into per-exporter rollup counters on the telemetry class
//! (`{exporter}/{flows,bytes,packets}_total` + `{exporter}/by_proto/*` —
//! RFC 11 §3; per-flow-pair keys are the population the convention forbids),
//! and serves the raw records pull-only from a bounded ring on the `flows`
//! read procedure.

mod config;
mod receiver;
mod rollup;

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
    let publish_stats = netflow_config.publish_stats;
    // Rollup cadence: `aggregation_interval_secs`, defaulting to 30 s when
    // unset/0 (the field predates the rollup design, where 0 meant "no
    // aggregation" — rollups ARE the telemetry now, so 0 keeps the default).
    let rollup_period = match netflow_config.aggregation_interval_secs {
        0 => 30,
        s => s,
    };

    // Build status metadata
    let metadata = serde_json::json!({
        "listeners": netflow_config.listeners.iter().map(|l| &l.bind).collect::<Vec<_>>(),
        "publish_flows": publish_flows,
        "publish_stats": publish_stats,
        "rollup_period_secs": rollup_period,
    });

    // The bounded flow ring behind the `flows` read procedure (RFC 11 §3:
    // raw records are pull-only detail, never streamed).
    let ring = rollup::new_ring();
    if publish_flows {
        let flows_key = zensight_common::command::query_key(&netflow_config.key_prefix, "flows");
        tokio::spawn(rollup::serve_flows(
            session.clone(),
            flows_key,
            ring.clone(),
        ));
    }

    // Intake: fold each record into the rollups + the ring; publish the
    // rollup counters on a cadence through declared publishers.
    let registry = zensight_common::PublisherRegistry::new(session.clone());
    let mut runner = runner;
    runner.spawn(async move {
        let mut rollups = rollup::Rollups::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(rollup_period.max(1)));
        // The first tick fires immediately; skip it so the first publication
        // carries a full period of data.
        tick.tick().await;

        loop {
            tokio::select! {
                Some(record) = rx.recv() => {
                    rollups.ingest(&record);
                    if publish_flows {
                        rollup::push(&ring, record);
                    }
                }
                _ = tick.tick(), if publish_stats => {
                    let now = zensight_common::current_timestamp_millis();
                    for point in rollups.points(now) {
                        let key = format!("{key_prefix}/{}", point.metric);
                        match encode(&point, format) {
                            Ok(payload) => {
                                if let Err(e) = registry
                                    .put(&key, payload, zensight_common::QosClass::Telemetry)
                                    .await
                                {
                                    tracing::error!("Failed to publish to {}: {}", key, e);
                                }
                            }
                            Err(e) => tracing::error!("Failed to serialize rollup: {}", e),
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
