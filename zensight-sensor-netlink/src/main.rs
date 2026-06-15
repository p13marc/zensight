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
    runner.spawn(async move {
        collector.run().await;
    });

    let metadata = serde_json::json!({
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
