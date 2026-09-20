//! Prometheus exporter for ZenSight telemetry.
//!
//! A host-origin **producer** since #1202: `SensorRunner` opens the one
//! session, publishes the five framework documents (a health document with
//! `self_stats`, the declared budget and the shed ladder) under
//! `state/exporter-prometheus/…`, and serves `introspect`/`describe`. The
//! exporter still publishes no telemetry (RFC 04 §1.1); what changed is that a
//! fleet can now see this process the way it sees every sensor.

use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser};
use tokio::sync::watch;
use tracing::{error, info};
use zensight_sensor_core::{SensorArgs, SensorRunner};

use zensight_exporter_prometheus::{
    ExporterConfig, HttpServer, MetricCollector, PRODUCER, RemoteWriteClient, TelemetrySubscriber,
};

/// Prometheus exporter for ZenSight telemetry.
#[derive(Parser, Debug)]
#[command(name = "zensight-exporter-prometheus")]
#[command(about = "Export ZenSight telemetry as Prometheus metrics")]
#[command(version)]
struct Args {
    /// The flags every producer takes: `--config` (default
    /// `prometheus-exporter.json5`, as every sensor defaults to its own
    /// file), `--log-level` (overrides the file's `logging.level`, #757) and
    /// `--check-config` (#1150).
    #[command(flatten)]
    common: SensorArgs,

    /// HTTP listen address (overrides config).
    #[arg(long)]
    listen: Option<String>,
}

impl Args {
    fn parse_with_default_config() -> Self {
        let matches = Self::command()
            .mut_arg("config", |arg| {
                arg.default_value("prometheus-exporter.json5")
            })
            .get_matches();
        Self::from_arg_matches(&matches).expect("the arguments parse")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse_with_default_config();

    let mut config = ExporterConfig::load_from_file(&args.common.config)?;

    // `--check-config` stops here, before the runner, the session and the
    // HTTP listener exist (#1150). A deploy script gates on the exit status.
    if args.common.check_config {
        zensight_sensor_core::report_config_ok(&args.common.config);
        return Ok(());
    }

    // Override listen address from CLI
    if let Some(listen) = args.listen {
        config.prometheus.listen = listen;
    }

    // Everything the collector and the server need is cloned out before the
    // config moves into the runner.
    let prometheus = config.prometheus.clone();
    let aggregation = config.aggregation.clone();
    let filters = config.filters.clone();
    let remote_write = config.remote_write.clone();

    // The runner (#1202) initialises tracing (`logging.level`/`format`, the
    // CLI flag winning — #757's rule, now the framework's), opens the one
    // session and owns the health, identity and budget publishers.
    let source = zensight_sensor_core::resolved_source(None);
    let mut runner = SensorRunner::new_with_args(PRODUCER, source, config, Some(&args.common))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    runner = runner.with_identity();
    let session = runner.session().clone();

    info!("Starting ZenSight Prometheus Exporter");

    // The workers' shutdown, flipped after the runner returns.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Whether the ingest pipeline is actually alive. `/health` reports it, so
    // a dead subscriber stops the process claiming to be healthy (#757).
    let health = zensight_common::pipeline_health::PipelineHealth::new();

    // Create the collector
    let collector = Arc::new(MetricCollector::new(
        prometheus.clone(),
        aggregation.clone(),
        filters.clone(),
    ));

    // The health document reports what this process holds (#1202): the
    // series it is forwarding, and the alerts and incidents it mirrors.
    {
        let c = collector.clone();
        runner.health().register_table_stats(Box::new(move || {
            vec![
                zensight_common::health::TableStats {
                    name: "series".to_string(),
                    entries: c.series_count() as u64,
                    bytes: None,
                    capacity_entries: Some(aggregation.max_series as u64),
                    capacity_bytes: None,
                },
                zensight_common::health::TableStats {
                    name: "alerts".to_string(),
                    entries: c.alert_count() as u64,
                    bytes: None,
                    capacity_entries: None,
                    capacity_bytes: None,
                },
                zensight_common::health::TableStats {
                    name: "incidents".to_string(),
                    entries: c.incident_count() as u64,
                    bytes: None,
                    capacity_entries: None,
                    capacity_bytes: None,
                },
            ]
        }));
    }

    // Parse listen address
    let listen_addr = prometheus
        .listen
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid listen address: {}", e))?;

    // Create components. A configured `filters.key_expr` narrows the telemetry
    // subscription (R6/#357) — default stays the full telemetry class selector.
    let subscriber = {
        let s = TelemetrySubscriber::new(collector.clone());
        match &filters.key_expr {
            Some(ke) => s.with_key_expr(ke.clone()),
            None => s,
        }
    };
    let http_server = HttpServer::new(
        collector.clone(),
        listen_addr,
        prometheus.path.clone(),
        health.clone(),
    );

    // Start cleanup task
    let cleanup_collector = collector.clone();
    let cleanup_interval = Duration::from_secs(aggregation.cleanup_interval_secs);
    let mut cleanup_shutdown = shutdown_rx.clone();
    runner.spawn_named("cleanup", async move {
        let mut interval = tokio::time::interval(cleanup_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    cleanup_collector.cleanup_stale();
                }
                _ = cleanup_shutdown.changed() => {
                    if *cleanup_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });

    // Start subscriber.
    //
    // A dead pipeline must stop the process claiming to be healthy (#757). This
    // used to log the error and carry on, serving an empty /metrics and a 200
    // /health forever — indistinguishable, to anything watching, from
    // "connected, no data yet". A monitoring component that reports healthy
    // while it monitors nothing is worse than one that is plainly down. The
    // failure flips `pipeline_failed`, which races the runner below.
    let (failed_tx, mut failed_rx) = watch::channel(false);
    {
        let subscriber_shutdown = shutdown_rx.clone();
        let subscriber_health = health.clone();
        let session = session.clone();
        runner.spawn_named("telemetry-subscriber", async move {
            if let Err(e) = subscriber.run(session, subscriber_shutdown).await {
                error!("Subscriber error: {}", e);
                subscriber_health.set_failed();
                let _ = failed_tx.send(true);
            }
        });
    }

    // Start HTTP server
    {
        let http_shutdown = shutdown_rx.clone();
        runner.spawn_named("http", async move {
            if let Err(e) = http_server.run(http_shutdown).await {
                error!("HTTP server error: {}", e);
            }
        });
    }

    // Start remote-write push loop (optional; default off)
    if remote_write.enabled {
        let client = RemoteWriteClient::new(collector.clone(), &remote_write)?;
        let rw_shutdown = shutdown_rx.clone();
        runner.spawn_named("remote-write", async move {
            if let Err(e) = client.run(rw_shutdown).await {
                error!("Remote-write error: {}", e);
            }
        });
    }

    // The runner waits for SIGTERM/Ctrl+C, then aborts the workers, retracts
    // this process's alerts and closes the session. A pipeline failure ends
    // the wait early: exiting non-zero, and letting the unit's
    // `Restart=on-failure` do its job, IS the signal (#757).
    let metadata = serde_json::json!({
        "listen": prometheus.listen,
        "path": prometheus.path,
        "remote_write": remote_write.enabled,
        "max_series": aggregation.max_series,
        "action_surface": false,
    });
    let outcome = tokio::select! {
        r = runner.run_with_metadata(Some(metadata)) => r.map_err(|e| anyhow::anyhow!("{e}")),
        _ = failed_rx.changed() => Err(anyhow::anyhow!("telemetry pipeline failed")),
    };

    // Signal shutdown to whatever is still draining.
    let _ = shutdown_tx.send(true);

    // Print final stats
    let stats = collector.stats();
    info!(
        points_received = stats.points_received,
        points_accepted = stats.points_accepted,
        points_filtered = stats.points_filtered,
        series_count = collector.series_count(),
        "Final statistics"
    );

    if let Err(e) = &outcome {
        error!("Exporter stopped: {e}");
    } else {
        info!("Exporter stopped");
    }
    outcome
}
