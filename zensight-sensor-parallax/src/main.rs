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
use zensight_sensor_parallax::query;

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
    let runner = runner.with_status_publishing().with_format(Format::Json);
    let runner = runner
        .with_liveliness()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // On-demand artifact channel (`@/artifact`): a report producer (redacted
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
    let host_prefix = format!("{}/{}", parallax_config.key_prefix, source);

    // Build the stream catalogue: enumerated V4L2 cameras + configured RTSP +
    // test-pattern sources.
    let catalog = Arc::new(Catalog::build(&parallax_config));
    tracing::info!(
        "Parallax sensor running (prefix: {}, source: {}, streams: [{}])",
        parallax_config.key_prefix,
        source,
        catalog.stream_names().collect::<Vec<_>>().join(", ")
    );

    // One liveliness token per advertised stream (`@/devices/<stream>/alive`)
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

    // Alert channel: reporter + the late-joiner `@/query/alerts` seed. Rules
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

    // Serve the stream catalogue on `@/query/streams`.
    {
        let q_session = session.clone();
        let q_prefix = host_prefix.clone();
        let q_catalog = catalog.clone();
        runner.spawn(async move {
            query::run(q_session, q_prefix, q_catalog).await;
        });
    }

    // Build status metadata
    let metadata = serde_json::json!({
        "source": source,
        "streams": catalog.stream_names().collect::<Vec<_>>(),
        "preview_fps": parallax_config.preview.fps,
        "video_bitrate_kbps": parallax_config.video.bitrate_kbps,
        "idle_timeout_secs": parallax_config.idle_timeout_secs,
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
