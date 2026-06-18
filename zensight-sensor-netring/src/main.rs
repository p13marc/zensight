//! ZenSight netring sensor binary. Linux only.

#[cfg(not(target_os = "linux"))]
compile_error!("zensight-sensor-netring requires Linux (AF_PACKET/AF_XDP).");

use std::sync::Arc;

use anyhow::Result;
use zensight_sensor_core::{
    AlertReporter, Format, Protocol, SensorArgs, SensorConfig, SensorRunner,
};

use zensight_sensor_netring::config::NetringSensorConfig;
use zensight_sensor_netring::{monitor, publish};

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("netring.json5");
    let config = NetringSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    let sensor_id = config.netring.resolved_sensor_id();

    let runner = SensorRunner::new_with_args("netring", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let runner = runner.with_status_publishing().with_format(Format::Json);

    let mut cfg = runner.config().netring.clone();
    cfg.sensor_id = sensor_id.clone();
    let session = runner.session().clone();
    let key_prefix = cfg.key_prefix.clone();

    tracing::info!(
        "Netring sensor running (prefix: {}, sensor: {}, source: {})",
        key_prefix,
        sensor_id,
        cfg.pcap.clone().unwrap_or_else(|| cfg.interfaces.join(","))
    );

    let reporter = Arc::new(AlertReporter::new(
        runner.publisher(),
        Protocol::Netring,
        Format::Json,
    ));

    // Build the netring monitor + drain channels (+ telemetry-channel keepalive).
    let (mon, channels, keepalive) =
        monitor::build(&cfg).map_err(|e| anyhow::anyhow!("{}", e))?;

    let is_pcap = cfg.pcap.is_some();
    let flow_period = cfg.bandwidth_period_secs;
    let mut runner = runner;

    // Late-joiner seed: serve the current firing set to consumers that connect
    // after an anomaly fired.
    runner.spawn(zensight_sensor_core::serve_alerts_query(reporter.clone()));

    // On-demand query channels (P2): recent-flow ring + TLS asset inventory.
    {
        let q_session = runner.session().clone();
        let q_prefix = key_prefix.clone();
        let flows = channels.flow_records.clone();
        runner.spawn(zensight_sensor_netring::query::run(q_session, q_prefix, flows));

        let t_session = runner.session().clone();
        let t_prefix = key_prefix.clone();
        let inventory = channels.tls_inventory.clone();
        runner.spawn(zensight_sensor_netring::query::run_tls(
            t_session, t_prefix, inventory,
        ));
    }

    // Drain task (telemetry + anomalies + periodic flow aggregates).
    let health = runner.health();
    runner.spawn(publish::run_drains(
        channels,
        session,
        key_prefix,
        sensor_id,
        Format::Json,
        reporter,
        flow_period,
        health,
    ));

    // Monitor run loop: pcap replay (bounded) or live capture (until signal).
    // Holds the telemetry-channel keepalive so the drain sees the channel close
    // (and flushes its final aggregate) only when the monitor actually stops.
    runner.spawn(async move {
        let _keepalive = keepalive;
        let result = if is_pcap {
            mon.replay().await
        } else {
            mon.run_until_signal().await
        };
        if let Err(e) = result {
            tracing::error!(error = %e, "netring monitor stopped");
        }
    });

    let metadata = serde_json::json!({
        "sensor_id": cfg.sensor_id,
        "source": if is_pcap { "pcap" } else { "capture" },
        "collect": { "bandwidth": cfg.collect.bandwidth, "flows": cfg.collect.flows },
        "anomalies": { "port_scan": cfg.anomalies.port_scan },
    });

    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
