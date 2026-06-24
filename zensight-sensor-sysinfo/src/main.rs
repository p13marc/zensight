//! Zenoh sensor for system monitoring.
//!
//! This sensor collects local system metrics (CPU, memory, disk, network)
//! and publishes them to Zenoh as telemetry.

use anyhow::Result;
use zensight_sensor_core::{Format, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_sysinfo::collector::SystemCollector;
use zensight_sensor_sysinfo::config::SysinfoSensorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("sysinfo.json5");

    // Load configuration using the framework's SensorConfig trait
    let config = SysinfoSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Resolve hostname
    let hostname = config.get_hostname();

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("sysinfo", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing and set format
    let runner = runner.with_status_publishing().with_format(Format::Json);

    // Get the config and publisher for the collector
    let sysinfo_config = runner.config().sysinfo.clone();
    let session = runner.session().clone();

    tracing::info!(
        "Sysinfo sensor running (prefix: {}, interval: {}s, hostname: {})",
        sysinfo_config.key_prefix,
        sysinfo_config.poll_interval_secs,
        hostname
    );

    // Spawn the collector task
    let mut runner = runner;

    // Spawn the on-demand per-process detail query channel (P2): the per-pid
    // firehose is served only on query, never streamed onto the telemetry bus.
    if sysinfo_config.collect.process_query {
        let q_session = session.clone();
        let q_prefix = sysinfo_config.key_prefix.clone();
        let q_host = hostname.clone();
        runner.spawn(async move {
            zensight_sensor_sysinfo::query::run(q_session, q_prefix, q_host).await;
        });
    }

    // Create and spawn the collector
    let collector = SystemCollector::new(hostname, sysinfo_config, session, Format::Json)
        .with_health(runner.health());
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
