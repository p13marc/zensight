//! Zenoh sensor for machine-checked desired-state assertions (#821).
//!
//! See the crate docs (`lib.rs`) for what this is and what it deliberately
//! is not. The startup shape is the framework's (sysinfo's skeleton, minus
//! artifacts): runner, identity, shared alert reporter with a late-joiner
//! seed, then the sentinel evaluator and its `@rpc` control surface.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zensight_sensor_core::{AlertReporter, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_hostspec::command;
use zensight_sensor_hostspec::config::HostspecSensorConfig;
use zensight_sensor_hostspec::sentinel::Evaluator;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("hostspec.json5");

    // `SensorConfig::load` runs the same validate() the hot-swap path runs:
    // a set that would be refused over `expectations/set` refuses to start,
    // naming every offending expectation.
    let config = HostspecSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let source = config.source();
    let expectations = config.hostspec.expectations.clone();

    let mut runner = SensorRunner::new_with_args("hostspec", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();

    // The shared reporter: hostspec's whole output is alerts, so it always
    // exists, with the set-wide debounce as its base (per-expectation
    // for_secs still overrides per observation).
    let mut reporter = AlertReporter::new(
        runner.publisher(),
        zensight_common::Protocol::Hostspec,
        format,
    )
    .with_debounce(Duration::from_secs(expectations.default_for_secs))
    // The set-wide recovery hold (#932). The sentinel resolves per-assertion
    // overrides itself and passes them per reconcile, so this is the base for
    // anything that does not — the threshold rules (#931) share this reporter.
    .with_recovery(Duration::from_secs(expectations.default_recover_after_secs));
    if let Some(id) = runner.identity() {
        reporter = reporter.with_identity(id);
    }
    let reporter = Arc::new(reporter);
    runner = runner.with_alert_reporter(reporter.clone());

    tracing::info!(
        assertions = %if expectations.is_empty() { "empty set".to_string() } else {
            format!("{} rule slug(s)", expectations.rule_slugs().len())
        },
        interval_secs = expectations.eval_interval_secs,
        source = %source,
        "hostspec sensor running (read-only; executes nothing)"
    );

    let reporter_for_thresholds = reporter.clone();
    let evaluator = Evaluator::new(
        source.clone(),
        expectations.clone(),
        reporter,
        runner.publisher(),
    );
    // The handle must be taken BEFORE run(self) consumes the evaluator.
    let handle = evaluator.handle();
    runner.spawn(evaluator.run());

    // The @desired reconciler (#816): this sensor's expectation set is the
    // v1 fleet-authorable topic. The reconciler and the RPC surface are the
    // two writers to one handle; the shared marker says who won last.
    let desired_cfg = runner.config().desired.clone();
    let desired_key = {
        use zensight_common::registry::desired;
        desired::key(&desired::Subject::hostspec_expectations(
            zensight_common::PROFILE.host_id(),
        ))
    };
    let apply_handle = handle.clone();
    let (marker, _reconcile_task) = zensight_sensor_core::desired::reconcile_topic(
        runner.session().clone(),
        runner.publisher(),
        zensight_sensor_core::desired::DesiredTopic {
            topic: "expectations",
            desired_key,
        },
        desired_cfg,
        expectations.clone(),
        move |cfg: zensight_common::hostspec::ExpectationsConfig| {
            let h = apply_handle.clone();
            async move {
                // The SAME gate the RPC path runs: an invalid desired doc is
                // refused (kept off the handle) and rides the marker.
                zensight_sensor_hostspec::sentinel::validate(&cfg)?;
                h.replace(cfg).await;
                Ok(())
            }
        },
    );

    let session = runner.session().clone();
    runner.spawn(command::run(
        session,
        "hostspec".to_string(),
        handle,
        marker,
    ));

    // Threshold rules over this sensor's own telemetry (#931). Separate from
    // the assertion set above and deliberately so: an expectation is a
    // *statement about the host* that this sensor goes and checks; a
    // threshold is a number an operator picked about a metric it publishes.
    // Same three writers, same marker discipline, its own topic.
    zensight_sensor_core::threshold::adopt(
        &mut runner,
        zensight_common::Protocol::Hostspec,
        reporter_for_thresholds,
        {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::hostspec_thresholds(
                zensight_common::PROFILE.host_id(),
            ))
        },
        &[],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    runner
        .run_with_metadata(Some(serde_json::json!({
            "source": source,
            "eval_interval_secs": expectations.eval_interval_secs,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}
