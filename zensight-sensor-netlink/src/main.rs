//! ZenSight netlink sensor binary.
//!
//! Reads Linux kernel networking state via netlink and publishes it as
//! telemetry. Linux only.

#[cfg(not(target_os = "linux"))]
compile_error!("zensight-sensor-netlink requires Linux (netlink).");

use anyhow::Result;
use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_netlink::Collector;
use zensight_sensor_netlink::config::NetlinkSensorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("netlink.json5");
    let config = NetlinkSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    let source = config.netlink.resolved_source();

    let runner = SensorRunner::new_with_args("netlink", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let format = runner.config().serialization;
    let runner = runner.with_format(format);

    // On-demand artifact channel (`@rpc/netlink/artifact/*`): bundle redacted config + health +
    // counters (report) plus tier-2 directory snapshots. Each kind is a no-op
    // unless enabled in the config's `artifacts` limits.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "netlink",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    let artifacts = runner.config().artifact_limits();
    let runner = runner.with_identity().with_artifacts(vec![
        std::sync::Arc::new(zensight_sensor_core::ReportProducer::new(
            report_source,
            &artifacts.report,
        )) as std::sync::Arc<dyn zensight_sensor_core::ArtifactProducer>,
        std::sync::Arc::new(zensight_sensor_core::SnapshotProducer::new(
            &artifacts.snapshot,
        )),
    ]);

    let netlink_config = runner.config().netlink.clone();
    let session = runner.session().clone();

    tracing::info!(
        "Netlink sensor running (interval: {}s, host: {})",
        netlink_config.poll_interval_secs,
        source
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
                let q_producer = "netlink".to_string();
                let q_state = state.clone();
                let top_k = netlink_config.ebpf.retransmit_top_k;
                runner.spawn(async move {
                    zensight_sensor_netlink::query::run_ebpf_queries(
                        q_session, q_producer, q_state, top_k,
                    )
                    .await;
                });
                std::mem::forget(bpf);
                ebpf_state = Some(state);
            }
            Err(e) => {
                tracing::warn!(error = %e, "eBPF load failed (needs CAP_BPF + CAP_PERFMON, plus CAP_DAC_READ_SEARCH for the tracepoints under a 0700 tracefs); baseline unchanged");
            }
        }
    }
    // Without the feature, `collect.ebpf: true` was silently ignored — the config
    // said one thing and the binary did another with no way to tell. Say so, as
    // sysinfo does.
    #[cfg(not(feature = "ebpf"))]
    if netlink_config.collect.ebpf {
        tracing::warn!(
            "collect.ebpf=true but this binary was built without the `ebpf` feature; ignoring"
        );
    }

    // Alert reporter shared by the expectation sentinel (Pillar B) and the XFRM
    // lifecycle sentinel (#267). Built here so it can be attached to the collector
    // before `run()` moves it.
    use std::sync::Arc;
    use std::time::Duration;
    use zensight_sensor_core::{AlertReporter, Protocol};
    let exp_cfg = netlink_config.expectations.clone().unwrap_or_default();
    let reporter = AlertReporter::new(runner.publisher(), Protocol::Netlink, format)
        .with_debounce(Duration::from_secs(exp_cfg.default_for_secs));
    let reporter = match runner.identity() {
        Some(id) => reporter.with_identity(id),
        None => reporter,
    };
    let reporter = Arc::new(reporter);

    let collector = Collector::new(source.clone(), netlink_config.clone(), session, format)
        .with_health(runner.health());
    #[cfg(feature = "ebpf")]
    let collector = collector.with_ebpf(ebpf_state.clone());
    // wg-quick peer labels (#268): parse configured wg-quick files once at start.
    let collector = if netlink_config.wireguard.wg_quick_configs.is_empty() {
        collector
    } else {
        let labels = zensight_sensor_netlink::collector::load_wg_labels(
            &netlink_config.wireguard.wg_quick_configs,
        );
        tracing::info!(peers = labels.len(), "loaded wg-quick peer labels");
        collector.with_wg_labels(labels)
    };
    // XFRM lifecycle sentinel (#267): only meaningful when both the event stream
    // and IPsec collection are on.
    let collector = if netlink_config.collect.events && netlink_config.collect.xfrm {
        use zensight_sensor_netlink::{XfrmSentinel, XfrmSentinelConfig};
        collector.with_xfrm_sentinel(XfrmSentinel::new(
            source.clone(),
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
    // Real-time event ring (served on @rpc/netlink/events) + the sentinel wake signal
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
        let query_producer = "netlink".to_string();
        let query_source = source.clone();
        let query_events = event_state.clone();
        let query_routes = route_history.clone();
        // Live collector toggles: the sockets queryable reads `socket_processes`
        // at query time so the runtime toggle applies without restart (#304).
        let query_collect = collect_handle.clone();
        // Tier-2a close-map attribution handle (#304): the loaded eBPF state on
        // an `ebpf` build, an always-None placeholder otherwise.
        #[cfg(feature = "ebpf")]
        let query_ebpf = ebpf_state.clone();
        #[cfg(not(feature = "ebpf"))]
        let query_ebpf = None;
        runner.spawn(async move {
            zensight_sensor_netlink::query::run(
                query_session,
                query_producer,
                query_source,
                query_events,
                query_routes,
                query_collect,
                query_ebpf,
            )
            .await;
        });
    }

    // Dynamic configuration (P4): toggle any collector at runtime, no restart.
    {
        let cmd_session = runner.session().clone();
        let cmd_producer = "netlink".to_string();
        runner.spawn(async move {
            zensight_sensor_netlink::command::run_collection(
                cmd_session,
                cmd_producer,
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
            source.clone(),
            exp_cfg,
            reporter.clone(),
            metric_cache,
        )
        .with_wake(sentinel_wake);
        let handle = evaluator.handle();
        let cmd_session = runner.session().clone();
        let cmd_producer = "netlink".to_string();
        runner.spawn(async move {
            evaluator.run().await;
        });
        runner.spawn(async move {
            zensight_sensor_netlink::command::run(cmd_session, cmd_producer, handle).await;
        });
        tracing::info!("Sentinel + expectation command channel enabled");
    }

    let metadata = serde_json::json!({
        "sentinel": true,
        "host": source,
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
