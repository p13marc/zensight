//! Zenoh sensor for Modbus protocol.
//!
//! This sensor polls Modbus devices (TCP or RTU/serial) and publishes
//! register values to Zenoh as telemetry.

use anyhow::Result;
use tracing::info;
use zensight_common::serialization::Format;
use zensight_sensor_core::{SensorArgs, SensorRunner};
use zensight_sensor_modbus::config::ModbusSensorConfig;
use zensight_sensor_modbus::poller::ModbusPoller;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("modbus.json5");

    // Load configuration
    let config = ModbusSensorConfig::load_from_file(&args.config)?;

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("modbus", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "modbus",
        report_host,
        runner.config().clone(),
        runner.health(),
    ));
    let mut runner = runner.with_report(report_source);

    // Get session and config
    let session = runner.session().clone();
    let modbus_config = runner.config().modbus.clone();

    // Serialization format (default to JSON)
    let format = Format::Json;

    // Start pollers for each device
    for device in &modbus_config.devices {
        let poller = ModbusPoller::new(device.clone(), &modbus_config, session.clone(), format);

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
