//! Zenoh bridge for Modbus protocol.
//!
//! This bridge polls Modbus devices (TCP or RTU/serial) and publishes
//! register values to Zenoh as telemetry.

use anyhow::Result;
use tracing::info;
use zenoh_bridge_modbus::config::ModbusBridgeConfig;
use zenoh_bridge_modbus::poller::ModbusPoller;
use zensight_bridge_framework::{BridgeArgs, BridgeRunner};
use zensight_common::serialization::Format;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = BridgeArgs::parse_with_default("modbus.json5");

    // Load configuration
    let config = ModbusBridgeConfig::load_from_file(&args.config)?;

    // Create the bridge runner
    let runner = BridgeRunner::new_with_args("modbus", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let mut runner = runner.with_status_publishing();

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
        "Modbus bridge running with {} device(s)",
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
