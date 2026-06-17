//! ZenSight netlink sensor binary.
//!
//! Reads Linux kernel networking state via netlink and publishes it as
//! telemetry. Linux only.

#[cfg(not(target_os = "linux"))]
compile_error!("zensight-sensor-netlink requires Linux (netlink).");

use anyhow::Result;
use zensight_sensor_core::{Format, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_netlink::Collector;
use zensight_sensor_netlink::config::NetlinkSensorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("netlink.json5");
    let config = NetlinkSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    let hostname = config.netlink.resolved_hostname();

    let runner = SensorRunner::new_with_args("netlink", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let runner = runner.with_status_publishing().with_format(Format::Json);

    let netlink_config = runner.config().netlink.clone();
    let session = runner.session().clone();

    tracing::info!(
        "Netlink sensor running (prefix: {}, interval: {}s, host: {})",
        netlink_config.key_prefix,
        netlink_config.poll_interval_secs,
        hostname
    );

    let collector = Collector::new(
        hostname.clone(),
        netlink_config.clone(),
        session,
        Format::Json,
    );
    let mut runner = runner;
    // Hot-swappable collector toggles, driven by the `collection` command channel.
    let collect_handle = collector.collect_handle();
    // Latest-metric cache shared with the sentinel's metric-threshold expectations.
    let metric_cache = collector.metric_cache();
    runner.spawn(async move {
        collector.run().await;
    });

    // On-demand detail query channel (principle P2): serves full route/neighbor/
    // socket tables to the GUI on demand, without streaming them onto the bus.
    {
        let query_session = runner.session().clone();
        let query_prefix = netlink_config.key_prefix.clone();
        runner.spawn(async move {
            zensight_sensor_netlink::query::run(query_session, query_prefix).await;
        });
    }

    // Dynamic configuration (P4): toggle any collector at runtime, no restart.
    {
        let cmd_session = runner.session().clone();
        let cmd_prefix = netlink_config.key_prefix.clone();
        runner.spawn(async move {
            zensight_sensor_netlink::command::run_collection(cmd_session, cmd_prefix, collect_handle)
                .await;
        });
    }

    // Pillar B — sentinel: evaluate declared expectations and emit alerts, and
    // accept runtime expectation commands from the GUI (always on, so the GUI
    // can author expectations even when none are configured on disk).
    {
        use std::sync::Arc;
        use std::time::Duration;
        use zensight_sensor_core::{AlertReporter, Protocol, serve_alerts_query};
        let exp_cfg = netlink_config.expectations.clone().unwrap_or_default();
        let reporter = Arc::new(
            AlertReporter::new(runner.publisher(), Protocol::Netlink, Format::Json)
                .with_debounce(Duration::from_secs(exp_cfg.default_for_secs)),
        );
        // Late-joiner seed: serve the current firing set to consumers that connect
        // after an alert fired.
        runner.spawn(serve_alerts_query(reporter.clone()));
        let evaluator = zensight_sensor_netlink::Evaluator::new(
            hostname.clone(),
            exp_cfg,
            reporter.clone(),
            metric_cache,
        );
        let handle = evaluator.handle();
        let cmd_session = runner.session().clone();
        let cmd_prefix = netlink_config.key_prefix.clone();
        runner.spawn(async move {
            evaluator.run().await;
        });
        runner.spawn(async move {
            zensight_sensor_netlink::command::run(cmd_session, cmd_prefix, handle).await;
        });
        tracing::info!("Sentinel + expectation command channel enabled");
    }

    let metadata = serde_json::json!({
        "sentinel": true,
        "host": hostname,
        "collect": {
            "interfaces": netlink_config.collect.interfaces,
            "sockets": netlink_config.collect.sockets,
        },
        "poll_interval_secs": netlink_config.poll_interval_secs,
    });

    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
