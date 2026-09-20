//! OpenTelemetry exporter for ZenSight telemetry.
//!
//! A host-origin **producer** since #1202: `SensorRunner` opens the one
//! session, publishes the five framework documents (a health document with
//! `self_stats`, the declared budget and the shed ladder) under
//! `state/exporter-otel/…`, and serves `introspect`/`describe`. The exporter
//! still publishes no telemetry (RFC 04 §1.1); what changed is that a fleet
//! can now see this process the way it sees every sensor.

use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser};
use tokio::sync::watch;
use tracing::{error, info};
use zensight_sensor_core::{SensorArgs, SensorRunner};

use zensight_exporter_otel::{ExporterConfig, OtelExporter, PRODUCER, TelemetrySubscriber};

/// OpenTelemetry exporter for ZenSight telemetry.
#[derive(Parser, Debug)]
#[command(name = "zensight-exporter-otel")]
#[command(about = "Export ZenSight telemetry to OpenTelemetry collectors")]
#[command(version)]
struct Args {
    /// The flags every producer takes: `--config` (default
    /// `otel-exporter.json5`, as every sensor defaults to its own file),
    /// `--log-level` (overrides the file's `logging.level`, #757) and
    /// `--check-config` (#1150).
    #[command(flatten)]
    common: SensorArgs,

    /// OTLP endpoint (overrides config).
    #[arg(long)]
    endpoint: Option<String>,
}

impl Args {
    fn parse_with_default_config() -> Self {
        let matches = Self::command()
            .mut_arg("config", |arg| arg.default_value("otel-exporter.json5"))
            .get_matches();
        Self::from_arg_matches(&matches).expect("the arguments parse")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse_with_default_config();

    let mut config = ExporterConfig::load_from_file(&args.common.config)?;

    // `--check-config` stops here, before the runner, the session and the
    // OTLP exporter exist (#1150). A deploy script gates on the exit status.
    if args.common.check_config {
        zensight_sensor_core::report_config_ok(&args.common.config);
        return Ok(());
    }

    // Override endpoint from CLI
    if let Some(endpoint) = args.endpoint {
        config.opentelemetry.endpoint = endpoint;
    }

    // Everything the exporter needs is cloned out before the config moves
    // into the runner.
    let otel = config.opentelemetry.clone();
    let filters = config.filters.clone();

    // The runner (#1202) initialises tracing (`logging.level`/`format`, the
    // CLI flag winning — #757's rule, now the framework's), opens the one
    // session and owns the health, identity and budget publishers.
    let source = zensight_sensor_core::resolved_source(None);
    let mut runner = SensorRunner::new_with_args(PRODUCER, source, config, Some(&args.common))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    runner = runner.with_identity();
    let session = runner.session().clone();

    info!("Starting ZenSight OpenTelemetry Exporter");
    info!(
        endpoint = %otel.endpoint,
        protocol = ?otel.protocol,
        export_metrics = otel.export_metrics,
        export_logs = otel.export_logs,
        export_alerts = otel.export_alerts,
        traces = otel.traces.enabled,
        "Configuration loaded"
    );

    // The workers' shutdown, flipped after the runner returns.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Create the OTEL exporter
    let exporter = Arc::new(OtelExporter::new(&otel, &filters).await?);

    // The health document reports what this process holds (#1202): the
    // series it is observing for its asynchronous instruments.
    {
        let e = exporter.clone();
        runner.health().register_table_stats(Box::new(move || {
            vec![zensight_common::health::TableStats {
                name: "series".to_string(),
                entries: e.series_count() as u64,
                bytes: None,
                capacity_entries: None,
                capacity_bytes: None,
            }]
        }));
    }

    // Create Zenoh subscriber. A configured `filters.key_expr` narrows the
    // telemetry subscription (R6/#357) — default stays the full telemetry class selector.
    let subscriber = {
        let s = TelemetrySubscriber::new(exporter.clone());
        match &filters.key_expr {
            Some(ke) => s.with_key_expr(ke.clone()),
            None => s,
        }
    };

    // Evict series nobody is reporting any more.
    //
    // This task is the missing caller for `cleanup_stale_observations` (#754):
    // the method existed with ZERO callers and its store was write-only, so a
    // host that went quiet kept flat-lining its last value forever. The
    // asynchronous instrument callbacks read that same store, so an evicted
    // series stops being observed and the metric gaps — which is the honest
    // rendering of "this host stopped reporting".
    //
    // Cadence mirrors the Prometheus exporter's stale sweep so the two
    // exporters age a dead sensor out at the same rate.
    const STALE_AFTER: Duration = Duration::from_secs(300);
    const SWEEP_EVERY: Duration = Duration::from_secs(60);
    /// How long an unresolved alert lifecycle is kept (#1146). An hour: long
    /// enough that no real incident is forgotten mid-flight, short enough that
    /// a reinstalled fleet does not ratchet the tracker to `MAX_PENDING`.
    const SPAN_TTL: Duration = Duration::from_secs(3600);
    {
        let cleanup_exporter = exporter.clone();
        let mut cleanup_shutdown = shutdown_rx.clone();
        runner.spawn_named("cleanup", async move {
            let mut interval = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        cleanup_exporter.cleanup_stale_observations(STALE_AFTER);
                        // The same sweep, for the same reason (#1146): a producer
                        // that stopped reporting will not send the `Resolved` an
                        // open lifecycle is waiting for. Without this the tracker
                        // only ever grew, and at `MAX_PENDING` it stopped emitting
                        // spans at all for the rest of the process's life.
                        //
                        // The TTL is generous next to the sweep, because sensors
                        // re-publish a firing alert: a real incident refreshes
                        // itself long before this, so what expires here is a
                        // lifecycle whose producer is gone.
                        cleanup_exporter.expire_alert_spans(SPAN_TTL);
                    }
                    _ = cleanup_shutdown.changed() => {
                        if *cleanup_shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Start subscriber.
    //
    // A dead pipeline winds the process down rather than lingering in a state
    // whose only symptom is silence (#757). This exporter has no /health to
    // report through — it pushes rather than being scraped — so exiting
    // non-zero, and letting the systemd unit's `Restart=on-failure` do its job,
    // IS the signal. The failure races the runner below.
    let (failed_tx, mut failed_rx) = watch::channel(false);
    {
        let subscriber_shutdown = shutdown_rx.clone();
        let session = session.clone();
        runner.spawn_named("telemetry-subscriber", async move {
            if let Err(e) = subscriber.run(session, subscriber_shutdown).await {
                error!("Subscriber error: {}", e);
                let _ = failed_tx.send(true);
            }
        });
    }

    // The runner waits for SIGTERM/Ctrl+C, then aborts the workers, retracts
    // this process's alerts and closes the session.
    let metadata = serde_json::json!({
        "endpoint": otel.endpoint,
        "export_metrics": otel.export_metrics,
        "export_logs": otel.export_logs,
        "export_alerts": otel.export_alerts,
        "traces": otel.traces.enabled,
        "action_surface": false,
    });
    let outcome = tokio::select! {
        r = runner.run_with_metadata(Some(metadata)) => r.map_err(|e| anyhow::anyhow!("{e}")),
        _ = failed_rx.changed() => Err(anyhow::anyhow!("telemetry pipeline failed")),
    };

    // Signal shutdown to whatever is still draining, then flush the OTEL
    // pipeline: a stop must not discard what it has already accepted.
    let _ = shutdown_tx.send(true);
    exporter.shutdown()?;

    // Print final stats
    let stats = exporter.stats();
    info!(
        points_received = stats.points_received,
        points_filtered = stats.points_filtered,
        metrics_exported = stats.metrics_exported,
        logs_exported = stats.logs_exported,
        "Final statistics"
    );

    // Exit non-zero when the pipeline died, so a supervisor restarts us instead
    // of leaving a process that is running and exporting nothing (#757).
    if let Err(e) = &outcome {
        error!("Exporter stopped because its telemetry pipeline failed: {e}");
    } else {
        info!("Exporter stopped");
    }
    outcome
}
