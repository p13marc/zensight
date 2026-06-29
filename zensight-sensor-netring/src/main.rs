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

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "netring",
        sensor_id.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    // Tier-2 directory snapshots (`@/snapshot`). No-op unless `snapshot.enabled`.
    // A natural use is pointing a snapshot dir at netring's pcap output directory.
    let runner = runner
        .with_report(report_source)
        .with_snapshot(sensor_id.clone());

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

    // Build the netring monitor + drain channels (+ telemetry-channel keepalive
    // + the runtime detection-tuning handle, #121).
    let (mon, channels, keepalive, detector_handle) =
        monitor::build(&cfg).map_err(|e| anyhow::anyhow!("{}", e))?;

    let is_pcap = cfg.pcap.is_some();
    let flow_period = cfg.bandwidth_period_secs;
    let mut runner = runner;

    // Late-joiner seed: serve the current firing set to consumers that connect
    // after an anomaly fired.
    runner.spawn(zensight_sensor_core::serve_alerts_query(reporter.clone()));

    // Runtime detection-tuning channel (#121): the GUI tunes allowlist /
    // thresholds / per-detector mute without a restart.
    runner.spawn(zensight_sensor_netring::command::run(
        runner.session().clone(),
        key_prefix.clone(),
        detector_handle,
    ));

    // On-demand query channels (P2): recent-flow ring, TLS asset inventory,
    // top-talkers, elephant flows, top DNS domains, top HTTP hosts.
    {
        use zensight_sensor_netring::query;
        let s = runner.session().clone();
        runner.spawn(query::run(
            s.clone(),
            key_prefix.clone(),
            channels.flow_records.clone(),
        ));
        runner.spawn(query::run_tls(
            s.clone(),
            key_prefix.clone(),
            channels.tls_inventory.clone(),
        ));
        if cfg.collect.talkers {
            runner.spawn(query::run_talkers(
                s.clone(),
                key_prefix.clone(),
                channels.talkers.clone(),
            ));
            runner.spawn(query::run_elephants(
                s.clone(),
                key_prefix.clone(),
                channels.elephants.clone(),
            ));
            // Traffic matrix / service map shares the talkers gate (#122).
            runner.spawn(query::run_matrix(
                s.clone(),
                key_prefix.clone(),
                channels.matrix.clone(),
            ));
        }
        if cfg.collect.dns {
            runner.spawn(query::run_dns(
                s.clone(),
                key_prefix.clone(),
                channels.dns.inventory.clone(),
            ));
        }
        if cfg.collect.http {
            runner.spawn(query::run_http(
                s.clone(),
                key_prefix.clone(),
                channels.http.inventory.clone(),
            ));
        }
        if cfg.collect.quic {
            runner.spawn(query::run_quic(
                s.clone(),
                key_prefix.clone(),
                channels.quic.clone(),
            ));
        }
        if cfg.collect.ssh {
            runner.spawn(query::run_ssh(
                s.clone(),
                key_prefix.clone(),
                channels.ssh.clone(),
            ));
        }
        #[cfg(feature = "ja4plus")]
        if cfg.collect.http_fp {
            runner.spawn(query::run_ja4h(
                s.clone(),
                key_prefix.clone(),
                channels.ja4h_fp.clone(),
            ));
        }
        if cfg.collect.assets {
            runner.spawn(query::run_assets(
                s.clone(),
                key_prefix.clone(),
                channels.assets.clone(),
            ));
        }
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
