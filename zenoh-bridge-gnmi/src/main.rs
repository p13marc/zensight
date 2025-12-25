//! gNMI bridge for ZenSight
//!
//! Connects to gNMI-enabled network devices and publishes streaming telemetry to Zenoh.

use tracing::{error, info};

use zenoh_bridge_gnmi::{GnmiConfig, GnmiSubscriber};
use zensight_bridge_framework::{BridgeArgs, BridgeRunner};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let args = BridgeArgs::parse_with_default("gnmi.json5");

    // Load configuration
    let config = GnmiConfig::load_from_file(&args.config)?;

    // Create the bridge runner
    let runner = BridgeRunner::new_with_args("gnmi", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let mut runner = runner.with_status_publishing();

    // Get session and config
    let session = runner.session().clone();
    let gnmi_config = runner.config().gnmi.clone();

    info!(
        "Starting gNMI bridge with {} targets",
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
