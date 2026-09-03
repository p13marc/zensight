//! Zenoh sensor for live video (parallax pipeline).
//!
//! Advertises a stream catalogue (V4L2 cameras / RTSP / test patterns) and
//! publishes encoded video + JPEG previews on the opaque `@media` plane on
//! demand (`OpenStream`/`CloseStream` commands).

use std::sync::Arc;

use anyhow::Result;
use zensight_sensor_core::{Format, Protocol, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_parallax::catalog::Catalog;
use zensight_sensor_parallax::config::ParallaxSensorConfig;
use zensight_sensor_parallax::session::SessionManager;
use zensight_sensor_parallax::{command, query};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("parallax.json5");

    // Load configuration using the framework's SensorConfig trait
    let config = ParallaxSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Resolve the source id (hostname)
    let source = config.resolved_source();

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("parallax", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing, set format, and declare liveliness early so
    // per-stream device tokens can be declared before run().
    let runner = runner.with_format(Format::Json);
    let runner = runner
        .with_liveliness()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // On-demand artifact channel (`@rpc/parallax/artifact/*`): a report producer (redacted
    // config + health + counters) and a snapshot producer. No-op unless the
    // matching `artifacts.*` kind is enabled in config.
    let report_source = Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "parallax",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    let artifacts = runner.config().artifact_limits();
    let runner = runner.with_identity();
    let mut runner = runner.with_artifacts(vec![
        Arc::new(zensight_sensor_core::ReportProducer::new(
            report_source,
            &artifacts.report,
        )) as Arc<dyn zensight_sensor_core::ArtifactProducer>,
        Arc::new(zensight_sensor_core::SnapshotProducer::new(
            &artifacts.snapshot,
        )),
    ]);

    let parallax_config = runner.config().parallax.clone();
    let session = runner.session().clone();

    // Build the stream catalogue: enumerated V4L2 cameras + configured RTSP +
    // test-pattern sources.
    let catalog = Arc::new(Catalog::build(&parallax_config));
    tracing::info!(
        "Parallax sensor running (source: {}, streams: [{}])",
        source,
        catalog.stream_names().collect::<Vec<_>>().join(", ")
    );

    // One liveliness token per advertised stream (`state/parallax/device/<stream>/alive`)
    // so the GUI can flip a camera card Offline when the sensor dies.
    if let Some(liveliness) = runner.liveliness() {
        for stream in catalog.stream_names() {
            if let Err(e) = liveliness.declare_device_alive(stream).await {
                tracing::warn!(stream = %stream, error = %e, "failed to declare stream liveliness");
            }
        }
        runner
            .health()
            .set_devices_total(catalog.entries().len() as u64);
    }

    // Alert channel: reporter + the late-joiner alert-state seed
    // (state/parallax/alert/*). Rules
    // (camera disappeared / RTSP connect failed / encoder overrun) hang off
    // this reporter.
    let mut reporter = zensight_sensor_core::AlertReporter::new(
        runner.publisher(),
        Protocol::Parallax,
        Format::Json,
    );
    if let Some(id) = runner.identity() {
        reporter = reporter.with_identity(id);
    }
    let reporter = Arc::new(reporter);
    runner = runner.with_alert_reporter(reporter.clone());

    // The operator's threshold rules over this sensor's own telemetry (#931):
    // frames published, encoder queue depth, viewers. Everything parallax
    // publishes as a point rides `Publisher::publish`, so the runner's
    // publisher is the whole surface here.
    zensight_sensor_core::threshold::adopt(
        &mut runner,
        Protocol::Parallax,
        reporter.clone(),
        {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::parallax_thresholds(
                zensight_common::PROFILE.host_id(),
            ))
        },
        &[],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let alerts = Arc::new(zensight_sensor_parallax::alerts::ParallaxAlerts::new(
        reporter.clone(),
        source.clone(),
    ));

    // Per-stream stats counters + the telemetry ticker
    // (`<stream>/stats/{fps,kbps,drops,viewers,encode_ms}` + the always-on
    // `streams/advertised` presence gauge).
    let stats = zensight_sensor_parallax::stats::StatsRegistry::default();
    // Receiver feedback (#715). Deliberately built here and handed to two
    // places that cannot reach an encoder: the queryable that accepts reports,
    // and the stats ticker that publishes their aggregate. Nothing that holds
    // a `SessionHandle` ever sees it — RFC 07 §1.2 forbids re-tuning a shared
    // tier from a report, and this is where that becomes structural.
    let reports = std::sync::Arc::new(
        zensight_sensor_parallax::reports::ReceiverReports::from_config(&parallax_config),
    );

    {
        let t_publisher = runner.publisher();
        let t_source = source.clone();
        let t_stats = stats.clone();
        let t_alerts = alerts.clone();
        let t_reports = reports.clone();
        let advertised = catalog.entries().len();
        let interval = std::time::Duration::from_secs(parallax_config.stats_interval_secs);
        runner.spawn(async move {
            zensight_sensor_parallax::stats::run_ticker(
                t_publisher,
                t_source,
                t_stats,
                advertised,
                interval,
                Some(t_alerts),
                t_reports,
            )
            .await;
        });
    }

    // Camera-presence watcher: re-enumerate V4L2 devices and drive the
    // camera_disappeared rule. No-op without local cameras.
    {
        let w_catalog = catalog.clone();
        let w_alerts = alerts.clone();
        runner.spawn(async move {
            zensight_sensor_parallax::alerts::watch_cameras(
                w_catalog,
                w_alerts,
                std::time::Duration::from_secs(30),
            )
            .await;
        });
    }

    // The stream-session actor: owns every open pipeline, driven by commands.
    let session_handle = SessionManager::spawn(
        catalog.clone(),
        parallax_config.clone(),
        source.clone(),
        runner.publisher(),
        stats.clone(),
        Some(runner.health()),
        Some(alerts.clone()),
    );

    // Stream control channel (`@rpc/parallax/stream/set` + `@rpc/parallax/streams`).
    {
        let c_session = session.clone();
        let c_producer = "parallax".to_string();
        let c_handle = session_handle.clone();
        runner.spawn(async move {
            command::run(c_session, c_producer, c_handle).await;
        });
    }

    // Receiver feedback (`@rpc/parallax/stream/report`). Beside the control
    // channel and pointedly NOT part of it: `command::run` takes a
    // `SessionHandle`, this takes only the report store.
    {
        let r_session = session.clone();
        let r_producer = "parallax".to_string();
        let r_reports = reports.clone();
        runner.spawn(async move {
            zensight_sensor_parallax::reports::run(r_session, r_producer, r_reports).await;
        });
    }

    // Serve the stream catalogue on `@rpc/parallax/streams`.
    {
        let q_session = session.clone();
        let q_producer = "parallax".to_string();
        let q_catalog = catalog.clone();
        // The catalogue advertises the wire ladder; the per-tier encoder
        // shaping (#509) stays on this host.
        let q_tiers = parallax_config.video.ladder();
        let q_handle = session_handle.clone();
        runner.spawn(async move {
            query::run(q_session, q_producer, q_catalog, q_tiers, q_handle).await;
        });
    }

    // Build status metadata
    let metadata = serde_json::json!({
        "source": source,
        "streams": catalog.stream_names().collect::<Vec<_>>(),
        "preview_fps": parallax_config.preview.fps,
        "tiers": parallax_config.video.tiers.iter().map(|t| &t.spec.name).collect::<Vec<_>>(),
        "default_tier": parallax_config.video.default_tier,
        "idle_timeout_secs": parallax_config.idle_timeout_secs,
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
