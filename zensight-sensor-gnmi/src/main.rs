//! gNMI sensor for ZenSight
//!
//! Connects to gNMI-enabled network devices and publishes streaming telemetry to Zenoh.

use tracing::{error, info};

use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};
use zensight_sensor_gnmi::{GnmiConfig, GnmiSubscriber};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("gnmi.json5");

    // Load configuration
    let config = GnmiConfig::load_from_file(&args.config)?;
    let source = config.gnmi.resolved_source();

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("gnmi", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing

    // On-demand debug-report (the artifact channel): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config. Target
    // `password` is redacted by default; add `redact_extra: ["username"]` in the
    // config if target usernames are sensitive.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "gnmi",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    // Tier-2 directory snapshots (the artifact channel). No-op unless `snapshot.enabled`.
    let artifacts = runner.config().artifact_limits();
    let mut runner = runner.with_identity().with_artifacts(vec![
        std::sync::Arc::new(zensight_sensor_core::ReportProducer::new(
            report_source,
            &artifacts.report,
        )) as std::sync::Arc<dyn zensight_sensor_core::ArtifactProducer>,
        std::sync::Arc::new(zensight_sensor_core::SnapshotProducer::new(
            &artifacts.snapshot,
        )),
    ]);

    // Get session and config
    let session = runner.session().clone();
    let gnmi_config = runner.config().gnmi.clone();

    info!(
        "Starting gNMI sensor with {} targets",
        gnmi_config.targets.len()
    );

    // This sensor's FIRST alerting surface (#931). It had none — no
    // `AlertReporter`, no `alerts.rs`, no `alert/{alert_key}` subject — so an
    // operator watching gnmi metrics had nowhere for a threshold to land.
    // The rules are the operator's; this sensor still asserts nothing of its
    // own. Handing the reporter to the runner is what serves the late-joiner
    // seed and makes the firing set survive a restart (#882).
    let reporter = {
        let mut r = zensight_sensor_core::AlertReporter::new(
            runner.publisher(),
            zensight_common::Protocol::Gnmi,
            gnmi_config.serialization.into(),
        );
        if let Some(id) = runner.identity() {
            r = r.with_identity(id);
        }
        std::sync::Arc::new(r)
    };
    runner = runner.with_alert_reporter(reporter.clone());

    let thresholds = zensight_sensor_core::threshold::adopt(
        &mut runner,
        zensight_common::Protocol::Gnmi,
        reporter,
        {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::gnmi_thresholds(
                zensight_common::PROFILE.host_id(),
            ))
        },
        &[],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Create subscriber tasks for each target
    for target in gnmi_config.targets {
        let subscriber = GnmiSubscriber::new(
            target.clone(),
            zensight_sensor_core::v1::for_producer("gnmi")
                .telemetry_prefix()
                .into(),
            gnmi_config.serialization,
        )
        .with_thresholds(thresholds.clone());
        let session = session.clone();

        runner.spawn(async move {
            if let Err(e) = subscriber.run(session).await {
                error!("Subscriber for {} failed: {}", target.name, e);
            }
        });
    }

    // Build status metadata
    let metadata = serde_json::json!({
        "targets": runner.config().gnmi.targets.iter().map(|t| &t.name).collect::<Vec<_>>(),
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
