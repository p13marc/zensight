//! ZenSight netring sensor binary. Linux only.

#[cfg(not(target_os = "linux"))]
compile_error!("zensight-sensor-netring requires Linux (AF_PACKET/AF_XDP).");

use std::sync::Arc;

use anyhow::Result;
use zensight_sensor_core::{AlertReporter, Protocol, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_netring::config::NetringSensorConfig;
use zensight_sensor_netring::{monitor, publish};

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("netring.json5");
    let config = NetringSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    let source = config.netring.resolved_source();

    let runner = SensorRunner::new_with_args("netring", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let format = runner.config().serialization;
    let runner = runner.with_status_publishing().with_format(format);

    // On-demand debug-report (`@/artifact`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "netring",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    let artifacts = runner.config().artifact_limits();

    let mut cfg = runner.config().netring.clone();
    cfg.source = source.clone();
    let key_prefix = cfg.key_prefix.clone();

    tracing::info!(
        "Netring sensor running (prefix: {}, source: {}, capture: {})",
        key_prefix,
        source,
        cfg.pcap.clone().unwrap_or_else(|| cfg.interfaces.join(","))
    );

    // Build the netring monitor FIRST (#333): the on-demand capture producer
    // needs the monitor's `ReloadHandle` + the tap subscription's registration
    // index, both known only after build, so `with_artifacts` runs after this.
    // Returns the drain channels, the telemetry-channel keepalive, the runtime
    // detection-tuning handle (#121), and the capture-tap index (if armed).
    let capture_tap = zensight_sensor_netring::capture::CaptureTap::default();
    let (mon, mut channels, keepalive, detector_handle, capture_tap_index) =
        monitor::build(&cfg, capture_tap.clone()).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Producers for the unified `@/artifact` channel: a Tier-1 report bundle and
    // a Tier-2 dir snapshot (a natural use is a dir pointed at netring's pcap
    // output) are always registered (no-op unless enabled). The on-demand capture
    // producer (#333) is registered only when `capture.on_demand.enabled` armed
    // the tap.
    let mut producers: Vec<std::sync::Arc<dyn zensight_sensor_core::ArtifactProducer>> = vec![
        std::sync::Arc::new(zensight_sensor_core::ReportProducer::new(
            report_source,
            &artifacts.report,
        )),
        std::sync::Arc::new(zensight_sensor_core::SnapshotProducer::new(
            &artifacts.snapshot,
        )),
    ];
    if let Some(tap_index) = capture_tap_index {
        producers.push(std::sync::Arc::new(
            zensight_sensor_netring::capture::CaptureProducer::new(
                &cfg.capture.on_demand,
                capture_tap.clone(),
                mon.reload_handle(),
                tap_index,
                source.clone(),
            ),
        ));
    }

    let runner = runner
        .with_identity(source.clone())
        .with_artifacts(source.clone(), producers);

    let session = runner.session().clone();

    let reporter = AlertReporter::new(runner.publisher(), Protocol::Netring, format);
    let reporter = Arc::new(match runner.identity() {
        Some(id) => reporter.with_identity(id),
        None => reporter,
    });

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

    // Runtime capture-focus channel (#225): hot-swap the packet-tier BPF filter
    // during an incident with no capture restart. Only spawned when armed; the
    // ReloadHandle is harmless either way (packet_filter_count() == 0 if not).
    if cfg.capture_focus.enabled {
        runner.spawn(zensight_sensor_netring::command::run_capture_filter(
            runner.session().clone(),
            key_prefix.clone(),
            mon.reload_handle(),
            cfg.capture_focus.base_expr.clone(),
        ));
    }

    // Runtime threat-intel channel (#328): hot-swap the live IOC set / YARA rules
    // without a capture restart. Spawned when the matchers are armed — either
    // startup indicators/rules, or `threat.reload` (which arms them empty).
    if cfg.threat.reload || mon.reload_handle().has_ioc() || cfg.threat.yara.file.is_some() {
        runner.spawn(zensight_sensor_netring::command::run_threat_intel(
            runner.session().clone(),
            key_prefix.clone(),
            mon.reload_handle(),
            cfg.threat.ioc.clone(),
        ));
    }

    // Capture-to-disk engine (#327): rotating spool or anomaly-triggered
    // pre-trigger-ring capture. Armed iff the monitor registered the continuous
    // packet feed (`capture.to_disk.mode != off`). Triggered files are served as
    // TTL'd Tier-1 blobs on the engine's own BlobServer (same `@/artifact/blob`
    // prefix as the artifact channel — servers ignore ids they don't own).
    let mut capture_disk_trigger = None;
    if let Some((disk_rx, disk_stats)) = channels.disk.take() {
        use zensight_sensor_netring::{command, disk, query};
        let to_disk = cfg.capture.to_disk.clone();
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = disk::CaptureDiskHandle::new(ctl_tx, disk_stats.clone());
        let index: disk::CaptureIndex =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let blob = zenoh_blob::BlobServer::new(
            runner.session().clone(),
            zensight_common::artifact_blob_prefix(&key_prefix),
            zenoh_blob::Format::Json,
        );
        {
            let blob = blob.clone();
            runner.spawn(async move {
                if let Err(e) = blob.run().await {
                    tracing::error!(error = %e, "netring: capture blob server exited");
                }
            });
        }
        runner.spawn(disk::run_engine(
            to_disk.clone(),
            source.clone(),
            disk_rx,
            ctl_rx,
            disk_stats.clone(),
            index.clone(),
            Some(blob),
            keepalive.clone(),
        ));
        runner.spawn(command::run_capture_disk(
            runner.session().clone(),
            key_prefix.clone(),
            handle.clone(),
            to_disk.max_files as u64,
            to_disk.max_total_bytes,
        ));
        runner.spawn(query::run_captures(
            runner.session().clone(),
            key_prefix.clone(),
            index,
        ));
        // Periodic capture/disk/* telemetry (ring occupancy, retention usage).
        {
            let tx = keepalive.clone();
            let stats = disk_stats;
            let sensor_id = source.clone();
            let period = cfg.bandwidth_period_secs.max(1);
            runner.spawn(async move {
                use std::sync::atomic::Ordering;
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(period));
                loop {
                    tick.tick().await;
                    let mode = match stats.mode() {
                        zensight_sensor_netring::config::CaptureDiskMode::Off => "off",
                        zensight_sensor_netring::config::CaptureDiskMode::Rotating => "rotating",
                        zensight_sensor_netring::config::CaptureDiskMode::Triggered => "triggered",
                    };
                    let pts = zensight_sensor_netring::map::capture_disk_points(
                        &sensor_id,
                        mode,
                        stats.ring_packets.load(Ordering::Relaxed),
                        stats.ring_bytes.load(Ordering::Relaxed),
                        stats.retained_files.load(Ordering::Relaxed),
                        stats.retained_bytes.load(Ordering::Relaxed),
                        stats.dropped.load(Ordering::Relaxed),
                        stats.evictions.load(Ordering::Relaxed),
                        stats.triggers.load(Ordering::Relaxed),
                    );
                    if pts.into_iter().any(|p| tx.send(p).is_err()) {
                        break; // telemetry pump gone
                    }
                }
            });
        }
        capture_disk_trigger = Some((handle, to_disk));
    }

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
                channels.aggregate.clone(),
                channels.name_map.clone(),
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
                channels.aggregate.clone(),
                channels.name_map.clone(),
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
        if cfg.collect.encrypted_dns {
            runner.spawn(query::run_encrypted_dns(
                s.clone(),
                key_prefix.clone(),
                channels.enc_dns.clone(),
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
        #[cfg(feature = "ipfix")]
        if cfg.collect.ipfix {
            runner.spawn(query::run_ipfix(
                s.clone(),
                key_prefix.clone(),
                channels.ipfix_records.clone(),
            ));
        }
    }

    // Host-evidence feed (#307): republish observed assets as third-party
    // identity evidence on `zensight/_meta/evidence/**` for the correlator.
    // Change-driven with a periodic liveness refresh, capped per tick. Only
    // runs when the asset inventory is collected and the feed is enabled.
    if cfg.evidence.enabled && cfg.collect.assets {
        use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};
        let ev_registry = Arc::new(
            AdvancedPublisherRegistry::new(
                runner.session().clone(),
                key_prefix.clone(),
                format,
                AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        );
        runner.spawn(zensight_sensor_netring::evidence::run_asset_evidence(
            channels.assets.clone(),
            channels.asset_dirty.clone(),
            ev_registry,
            cfg.evidence.clone(),
        ));
    }

    // Passive-DNS name-observation feed (#307): republish newly-observed IP→name
    // bindings (forwarded off the name-map drain task) as `NameObservation`
    // evidence on `zensight/_meta/evidence/names/**` for the correlator.
    if cfg.evidence.enabled
        && let Some(name_obs_rx) = channels.name_obs_rx.take()
    {
        use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};
        let name_registry = Arc::new(
            AdvancedPublisherRegistry::new(
                runner.session().clone(),
                key_prefix.clone(),
                format,
                AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        );
        runner.spawn(zensight_sensor_netring::evidence::run_name_evidence(
            name_obs_rx,
            name_registry,
        ));
    }

    // Drain task (telemetry + anomalies + periodic flow aggregates).
    let health = runner.health();
    runner.spawn(publish::run_drains(
        channels,
        session,
        key_prefix,
        source,
        format,
        reporter,
        flow_period,
        health,
        capture_disk_trigger,
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
        "source": cfg.source,
        "capture": if is_pcap { "pcap" } else { "capture" },
        "collect": { "bandwidth": cfg.collect.bandwidth, "flows": cfg.collect.flows },
        "anomalies": { "port_scan": cfg.anomalies.port_scan },
    });

    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
