//! Zenoh sensor for Modbus protocol.
//!
//! This sensor polls Modbus devices (TCP or RTU/serial) and publishes
//! register values to Zenoh as telemetry.

use anyhow::Result;
use tracing::info;
use zensight_common::serialization::Format;
use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};
use zensight_sensor_modbus::config::ModbusSensorConfig;
use zensight_sensor_modbus::poller::ModbusPoller;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("modbus.json5");

    // Load configuration
    let config = ModbusSensorConfig::load_from_file(&args.config)?;
    let source = config.modbus.resolved_source();

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("modbus", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing

    // On-demand debug-report (the artifact channel): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "modbus",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    // Tier-2 directory snapshots. No-op unless enabled in the config.
    let artifacts = runner.config().artifact_limits();
    let mut runner = runner.with_identity().with_artifacts(vec![
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
    let modbus_config = runner.config().modbus.clone();

    // Serialization format (default to JSON)
    let format = Format::Json;

    // This sensor's FIRST alerting surface (#931). It had none — no
    // `AlertReporter`, no `alerts.rs`, no `alert/{alert_key}` subject — so an
    // operator watching modbus metrics had nowhere for a threshold to land.
    // The rules are the operator's; this sensor still asserts nothing of its
    // own. Handing the reporter to the runner is what serves the late-joiner
    // seed and makes the firing set survive a restart (#882).
    let reporter = {
        let mut r = zensight_sensor_core::AlertReporter::new(
            runner.publisher(),
            zensight_common::Protocol::Modbus,
            format,
        );
        if let Some(id) = runner.identity() {
            r = r.with_identity(id);
        }
        std::sync::Arc::new(r)
    };
    runner = runner.with_alert_reporter(reporter.clone());

    // `source` is a label in a `ThresholdRule`, so one rule can name one
    // device or match every device this proxy polls — which is what the shared
    // vocabulary was shaped for, without it needing to know proxies exist.
    let thresholds = zensight_sensor_core::threshold::adopt(
        &mut runner,
        zensight_common::Protocol::Modbus,
        reporter,
        {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::modbus_thresholds(
                zensight_common::PROFILE.host_id(),
            ))
        },
        &[],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Start pollers for each device
    for device in &modbus_config.devices {
        let poller = ModbusPoller::new(device.clone(), &modbus_config, session.clone(), format)
            .with_thresholds(thresholds.clone());

        info!(
            "Starting poller for device '{}' ({:?})",
            device.name, device.connection
        );

        runner.spawn(async move {
            poller.run().await;
        });
    }

    info!(
        "Modbus sensor running with {} device(s)",
        modbus_config.devices.len()
    );

    // Build status metadata
    let metadata = serde_json::json!({
        "devices": modbus_config.devices.iter().map(|d| &d.name).collect::<Vec<_>>(),
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
