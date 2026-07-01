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

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "netlink",
        hostname.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    // Tier-2 directory snapshots (`@/snapshot`). No-op unless `snapshot.enabled`.
    let runner = runner
        .with_report(report_source)
        .with_snapshot(hostname.clone());

    let netlink_config = runner.config().netlink.clone();
    let session = runner.session().clone();

    tracing::info!(
        "Netlink sensor running (prefix: {}, interval: {}s, host: {})",
        netlink_config.key_prefix,
        netlink_config.poll_interval_secs,
        hostname
    );

    let mut runner = runner;

    // Opt-in eBPF module (#114): load BEFORE building the collector so the
    // connect-latency gauges flow through its publish path (→ MetricCache →
    // sentinel). Load/attach failure → one warning, unprivileged baseline
    // unchanged. The loaded `Ebpf` is leaked (mem::forget) so the programs stay
    // attached for the process lifetime; the kernel detaches them on exit.
    #[cfg(feature = "ebpf")]
    let mut ebpf_state: Option<zensight_sensor_netlink::ebpf::EbpfState> = None;
    #[cfg(feature = "ebpf")]
    if netlink_config.collect.ebpf {
        match zensight_sensor_netlink::ebpf::load(netlink_config.ebpf.conn_ring_capacity) {
            Ok((bpf, state, ring)) => {
                tracing::info!("eBPF module loaded (connlat + retransmits + tcplife)");
                let drain_state = state.clone();
                runner.spawn(async move {
                    zensight_sensor_netlink::ebpf::drain_ring(ring, drain_state).await;
                });
                let q_session = runner.session().clone();
                let q_prefix = netlink_config.key_prefix.clone();
                let q_state = state.clone();
                let top_k = netlink_config.ebpf.retransmit_top_k;
                runner.spawn(async move {
                    zensight_sensor_netlink::query::run_ebpf_queries(
                        q_session, q_prefix, q_state, top_k,
                    )
                    .await;
                });
                std::mem::forget(bpf);
                ebpf_state = Some(state);
            }
            Err(e) => {
                tracing::warn!(error = %e, "eBPF load failed (needs CAP_BPF/CAP_NET_ADMIN); baseline unchanged");
            }
        }
    }

    // Alert reporter shared by the expectation sentinel (Pillar B) and the XFRM
    // lifecycle sentinel (#267). Built here so it can be attached to the collector
    // before `run()` moves it.
    use std::sync::Arc;
    use std::time::Duration;
    use zensight_sensor_core::{AlertReporter, Protocol};
    let exp_cfg = netlink_config.expectations.clone().unwrap_or_default();
    let reporter = Arc::new(
        AlertReporter::new(runner.publisher(), Protocol::Netlink, Format::Json)
            .with_debounce(Duration::from_secs(exp_cfg.default_for_secs)),
    );

    let collector = Collector::new(
        hostname.clone(),
        netlink_config.clone(),
        session,
        Format::Json,
    )
    .with_health(runner.health());
    #[cfg(feature = "ebpf")]
    let collector = collector.with_ebpf(ebpf_state);
    // XFRM lifecycle sentinel (#267): only meaningful when both the event stream
    // and IPsec collection are on.
    let collector = if netlink_config.collect.events && netlink_config.collect.xfrm {
        use zensight_sensor_netlink::{XfrmSentinel, XfrmSentinelConfig};
        collector.with_xfrm_sentinel(XfrmSentinel::new(
            hostname.clone(),
            reporter.clone(),
            XfrmSentinelConfig::default(),
        ))
    } else {
        collector
    };
    // Hot-swappable collector toggles, driven by the `collection` command channel.
    let collect_handle = collector.collect_handle();
    // Latest-metric cache shared with the sentinel's metric-threshold expectations.
    let metric_cache = collector.metric_cache();
    // Real-time event ring (served on @/query/events) + the sentinel wake signal
    // (instant re-eval on a relevant RTNETLINK event), grabbed before run() moves
    // the collector (#8).
    let event_state = collector.event_state();
    let route_history = collector.route_history();
    let sentinel_wake = collector.sentinel_wake();
    runner.spawn(async move {
        collector.run().await;
    });

    // On-demand detail query channel (principle P2): serves full route/neighbor/
    // socket/address tables + the recent-events ring to the GUI on demand,
    // without streaming them onto the bus.
    {
        let query_session = runner.session().clone();
        let query_prefix = netlink_config.key_prefix.clone();
        let query_events = event_state.clone();
        let query_routes = route_history.clone();
        runner.spawn(async move {
            zensight_sensor_netlink::query::run(
                query_session,
                query_prefix,
                query_events,
                query_routes,
            )
            .await;
        });
    }

    // Dynamic configuration (P4): toggle any collector at runtime, no restart.
    {
        let cmd_session = runner.session().clone();
        let cmd_prefix = netlink_config.key_prefix.clone();
        runner.spawn(async move {
            zensight_sensor_netlink::command::run_collection(
                cmd_session,
                cmd_prefix,
                collect_handle,
            )
            .await;
        });
    }

    // Pillar B — sentinel: evaluate declared expectations and emit alerts, and
    // accept runtime expectation commands from the GUI (always on, so the GUI
    // can author expectations even when none are configured on disk).
    {
        use zensight_sensor_core::serve_alerts_query;
        // `reporter` + `exp_cfg` were built above (shared with the XFRM sentinel).
        // Late-joiner seed: serve the current firing set to consumers that connect
        // after an alert fired.
        runner.spawn(serve_alerts_query(reporter.clone()));
        let evaluator = zensight_sensor_netlink::Evaluator::new(
            hostname.clone(),
            exp_cfg,
            reporter.clone(),
            metric_cache,
        )
        .with_wake(sentinel_wake);
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
            "ebpf": netlink_config.collect.ebpf,
        },
        "poll_interval_secs": netlink_config.poll_interval_secs,
    });

    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
