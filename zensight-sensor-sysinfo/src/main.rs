//! Zenoh sensor for system monitoring.
//!
//! This sensor collects local system metrics (CPU, memory, disk, network)
//! and publishes them to Zenoh as telemetry.

use anyhow::Result;
use zensight_sensor_core::{Format, Protocol, SensorArgs, SensorConfig, SensorRunner};

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

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "sysinfo",
        hostname.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    let runner = runner.with_report(report_source);

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

    // Threshold-based alerting: drive an AlertReporter → zensight/sysinfo/@/alerts/*
    // for OOM / PSI / disk / FD / thermal / swap saturation (mirrors the other
    // sensors). Late-joining GUIs seed their firing set via serve_alerts_query.
    let mut collector = SystemCollector::new(
        hostname.clone(),
        sysinfo_config.clone(),
        session,
        Format::Json,
    )
    .with_health(runner.health());
    if sysinfo_config.alerts.enabled {
        use std::sync::Arc;
        use std::time::Duration;
        use zensight_sensor_core::{AlertReporter, serve_alerts_query};
        let reporter = Arc::new(
            AlertReporter::new(runner.publisher(), Protocol::Sysinfo, Format::Json)
                .with_debounce(Duration::from_secs(sysinfo_config.alerts.for_secs)),
        );
        runner.spawn(serve_alerts_query(reporter.clone()));
        let evaluator = zensight_sensor_sysinfo::alerts::AlertEvaluator::new(
            hostname.clone(),
            sysinfo_config.alerts.clone(),
            reporter,
        );
        collector = collector.with_alerts(evaluator);
        tracing::info!("Sysinfo threshold alerting enabled");
    }
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
