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
    runner.spawn(zensight_sensor_core::serve_alerts_query(reporter.clone()));
    let alerts = Arc::new(zensight_sensor_parallax::alerts::ParallaxAlerts::new(
        reporter.clone(),
        source.clone(),
    ));

    // Per-stream stats counters + the telemetry ticker
    // (`<stream>/stats/{fps,kbps,drops,viewers,encode_ms}` + the always-on
    // `streams/advertised` presence gauge).
    let stats = zensight_sensor_parallax::stats::StatsRegistry::default();
    {
        let t_publisher = runner.publisher();
        let t_source = source.clone();
        let t_stats = stats.clone();
        let t_alerts = alerts.clone();
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

    // Serve the stream catalogue on `@rpc/parallax/streams`.
    {
        let q_session = session.clone();
        let q_producer = "parallax".to_string();
        let q_catalog = catalog.clone();
        let q_tiers = parallax_config.video.tiers.clone();
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
        "tiers": parallax_config.video.tiers.iter().map(|t| &t.name).collect::<Vec<_>>(),
        "default_tier": parallax_config.video.default_tier,
        "idle_timeout_secs": parallax_config.idle_timeout_secs,
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
