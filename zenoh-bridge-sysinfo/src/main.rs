//! Zenoh bridge for system monitoring.
//!
//! This bridge collects local system metrics (CPU, memory, disk, network)
//! and publishes them to Zenoh as telemetry.

use anyhow::Result;
use zensight_bridge_framework::{BridgeArgs, BridgeConfig, BridgeRunner, Format};

use zenoh_bridge_sysinfo::collector::SystemCollector;
use zenoh_bridge_sysinfo::config::SysinfoBridgeConfig;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = BridgeArgs::parse_with_default("sysinfo.json5");

    // Load configuration using the framework's BridgeConfig trait
    let config = SysinfoBridgeConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Resolve hostname
    let hostname = config.get_hostname();

    // Create the bridge runner
    let runner = BridgeRunner::new_with_args("sysinfo", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing and set format
    let runner = runner.with_status_publishing().with_format(Format::Json);

    // Get the config and publisher for the collector
    let sysinfo_config = runner.config().sysinfo.clone();
    let session = runner.session().clone();

    tracing::info!(
        "Sysinfo bridge running (prefix: {}, interval: {}s, hostname: {})",
        sysinfo_config.key_prefix,
        sysinfo_config.poll_interval_secs,
        hostname
    );

    // Create and spawn the collector
    let collector = SystemCollector::new(hostname, sysinfo_config, session, Format::Json);

    // Spawn the collector task
    let mut runner = runner;
    runner.spawn(async move {
        collector.run().await;
    });

    // Build status metadata
    let metadata = serde_json::json!({
        "hostname": runner.config().get_hostname(),
        "collect": {
            "cpu": runner.config().sysinfo.collect.cpu,
            "cpu_times": runner.config().sysinfo.collect.cpu_times,
            "memory": runner.config().sysinfo.collect.memory,
            "disk": runner.config().sysinfo.collect.disk,
            "disk_io": runner.config().sysinfo.collect.disk_io,
            "network": runner.config().sysinfo.collect.network,
            "system": runner.config().sysinfo.collect.system,
            "temperatures": runner.config().sysinfo.collect.temperatures,
            "tcp_states": runner.config().sysinfo.collect.tcp_states,
            "processes": runner.config().sysinfo.collect.processes,
        },
        "poll_interval_secs": runner.config().sysinfo.poll_interval_secs,
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
