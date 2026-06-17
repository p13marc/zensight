//! gNMI sensor for ZenSight
//!
//! Connects to gNMI-enabled network devices and publishes streaming telemetry to Zenoh.

use tracing::{error, info};

use zensight_sensor_core::{SensorArgs, SensorRunner};
use zensight_sensor_gnmi::{GnmiConfig, GnmiSubscriber};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("gnmi.json5");

    // Load configuration
    let config = GnmiConfig::load_from_file(&args.config)?;

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("gnmi", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let mut runner = runner.with_status_publishing();

    // Get session and config
    let session = runner.session().clone();
    let gnmi_config = runner.config().gnmi.clone();

    info!(
        "Starting gNMI sensor with {} targets",
        gnmi_config.targets.len()
    );

    // Create subscriber tasks for each target
    for target in gnmi_config.targets {
        let subscriber = GnmiSubscriber::new(
            target.clone(),
            gnmi_config.key_prefix.clone(),
            gnmi_config.serialization,
        );
        let session = session.clone();

        runner.spawn(async move {
            if let Err(e) = subscriber.run(session).await {
                error!("Subscriber for {} failed: {}", target.name, e);
            }
        });
    }

    // Build status metadata
    let metadata = serde_json::json!({
        "targets": runner.config().gnmi.targets.iter().map(|t| &t.name).collect::<Vec<_>>(),
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
