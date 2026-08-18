//! Zenoh sensor for systemd unit/service state and boot performance.
//!
//! Reads the `org.freedesktop.systemd1.Manager` D-Bus interface (system bus) and
//! publishes unit-state aggregates + boot-performance timings to Zenoh under
//! `zensight/systemd/<host>/…`. Fails gracefully (reports unhealthy, never
//! crashes) on non-systemd hosts.

use anyhow::Result;
use zensight_sensor_core::{Protocol, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_systemd::collector::SystemdCollector;
use zensight_sensor_systemd::config::SystemdSensorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("systemd.json5");

    // Load configuration via the framework's SensorConfig trait.
    let config = SystemdSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;
    let source = config.source();

    // Create the sensor runner.
    let runner = SensorRunner::new_with_args("systemd", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing and pin JSON serialization.
    let format = runner.config().serialization;
    let runner = runner.with_format(format);

    // On-demand debug-report (the artifact channel): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "systemd",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    let artifacts = runner.config().artifact_limits();
    let runner = runner.with_identity();
    let mut runner = runner.with_artifacts(vec![
        std::sync::Arc::new(zensight_sensor_core::ReportProducer::new(
            report_source,
            &artifacts.report,
        )) as std::sync::Arc<dyn zensight_sensor_core::ArtifactProducer>,
        std::sync::Arc::new(zensight_sensor_core::SnapshotProducer::new(
            &artifacts.snapshot,
        )),
    ]);

    let systemd_config = runner.config().systemd.clone();

    tracing::info!(
        "systemd sensor running (interval: {}s, source: {})",
        systemd_config.poll_interval_secs,
        source
    );

    // Shared control-plane event ring (#275), fed by the D-Bus event stream and
    // served on @rpc/systemd/events.
    let event_state =
        zensight_sensor_systemd::events::EventState::new(systemd_config.events_capacity);

    use std::sync::Arc;
    use std::time::Duration;
    use zensight_sensor_core::{AlertReporter, serve_alerts_query};

    // Sentinel wake (#277): the event stream nudges the sentinel for instant
    // re-eval on watched control-plane changes.
    let sentinel_wake = Arc::new(tokio::sync::Notify::new());

    // D-Bus event stream (#275): watched UnitNew/Removed + JobNew/Removed → ring,
    // and nudge the sentinel on watched changes.
    let watch = zensight_sensor_systemd::config::compile_watch(&systemd_config.watch_units);
    let events_state = event_state.clone();
    let events_wake = sentinel_wake.clone();
    runner.spawn(async move {
        zensight_sensor_systemd::events::run(watch, events_state, Some(events_wake)).await;
    });

    // On-demand unit inventory query channel (#274/#275):
    // @rpc/systemd/{units,failed,unit,events}.
    let query_session = runner.session().clone();
    let query_producer = "systemd".to_string();
    let query_events = event_state.clone();
    let query_cgroup = systemd_config.cgroup.clone();
    let query_expose_unit_files = systemd_config.actions.expose_unit_files;
    runner.spawn(async move {
        zensight_sensor_systemd::query::run(
            query_session,
            query_producer,
            query_events,
            query_cgroup,
            query_expose_unit_files,
        )
        .await;
    });

    // Shared AlertReporter → state/systemd/alert/* for both the built-in
    // threshold alerts (#276) and the sentinel (#277), with one late-joiner
    // alert-state seed on state/systemd/alert/*. Created when either feature is active.
    let expectations = systemd_config.expectations.clone();
    let alerts_active = systemd_config.alerts.enabled || expectations.is_some();
    let reporter = alerts_active.then(|| {
        let mut reporter = AlertReporter::new(runner.publisher(), Protocol::Systemd, format)
            .with_debounce(Duration::from_secs(systemd_config.alerts.for_secs));
        if let Some(id) = runner.identity() {
            reporter = reporter.with_identity(id);
        }
        let r = Arc::new(reporter);
        runner.spawn(serve_alerts_query(r.clone()));
        r
    });

    let mut collector = SystemdCollector::new(
        source.clone(),
        systemd_config.clone(),
        runner.publisher(),
        runner.health(),
    )
    .with_events(event_state);
    // Threshold alerts (#276).
    if systemd_config.alerts.enabled
        && let Some(reporter) = &reporter
    {
        let evaluator = zensight_sensor_systemd::alerts::AlertEvaluator::new(
            source.clone(),
            systemd_config.alerts.clone(),
            reporter.clone(),
        );
        collector = collector.with_alerts(evaluator);
        tracing::info!("systemd threshold alerting enabled");
    }
    runner.spawn(async move {
        collector.run().await;
    });

    // Embedded sentinel (#277): declarative expectations → alerts, hot-swappable
    // via @rpc/systemd/expectations/set (+ read on …/expectations). Needs its own D-Bus
    // connection for per-expectation state reads.
    if let (Some(exp_cfg), Some(reporter)) = (expectations, reporter) {
        match zbus::Connection::system().await {
            Ok(conn) => {
                let evaluator = zensight_sensor_systemd::sentinel::Evaluator::new(
                    source.clone(),
                    exp_cfg,
                    reporter,
                    conn,
                )
                .with_wake(sentinel_wake);
                let handle = evaluator.handle();
                runner.spawn(async move { evaluator.run().await });
                let cmd_session = runner.session().clone();
                let cmd_producer = "systemd".to_string();
                runner.spawn(async move {
                    zensight_sensor_systemd::command::run(cmd_session, cmd_producer, handle).await;
                });
                tracing::info!("systemd sentinel enabled");
            }
            Err(e) => {
                tracing::error!(error = %e, "systemd sentinel: system bus connect failed");
                declare_sentinel_unavailable(
                    &mut runner,
                    "the systemd sentinel could not reach the system bus on this host",
                );
            }
        }
    } else {
        declare_sentinel_unavailable(
            &mut runner,
            "no `expectations` are configured for this systemd sensor, or alerting is off",
        );
    }

    // Gated service control (#283) — default OFF; only declared when explicitly
    // enabled. Read-only sensor otherwise.
    let action_session = runner.session().clone();
    let action_producer = "systemd".to_string();
    let action_cfg = systemd_config.actions.clone();
    runner.spawn(async move {
        zensight_sensor_systemd::action::run(action_session, action_producer, action_cfg).await;
    });

    let metadata = serde_json::json!({
        "source": source,
        "poll_interval_secs": systemd_config.poll_interval_secs,
    });

    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}

/// Declare `expectations` + `expectations/set` for a run with no sentinel (#648).
///
/// Both are registered unconditionally in `registry/systemd.toml`, so a build
/// that skipped them made `introspect` advertise two procedures it never served
/// (RFC 08 §6.1) and tripped `check_registry_coverage` at startup. Answering
/// `error/gated` also tells an operator *which* precondition is missing, which
/// declaring nothing could not.
fn declare_sentinel_unavailable<C: zensight_sensor_core::SensorConfig>(
    runner: &mut zensight_sensor_core::SensorRunner<C>,
    why: &'static str,
) {
    let session = runner.session().clone();
    runner.spawn(async move {
        zensight_common::served::serve_unavailable(
            session,
            vec![
                zensight_common::command::command_key("systemd", "expectations"),
                zensight_common::command::status_key("systemd", "expectations"),
            ],
            zensight_common::rpc::RpcError::gated(why),
        )
        .await;
    });
}
