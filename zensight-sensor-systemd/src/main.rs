//! Zenoh sensor for systemd unit/service state and boot performance.
//!
//! Reads the `org.freedesktop.systemd1.Manager` D-Bus interface (system bus) and
//! publishes unit-state aggregates + boot-performance timings to Zenoh under
//! `zensight/systemd/<host>/…`. Fails gracefully (reports unhealthy, never
//! crashes) on non-systemd hosts.

use anyhow::Result;
use zensight_sensor_core::{Format, Protocol, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_systemd::collector::SystemdCollector;
use zensight_sensor_systemd::config::SystemdSensorConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("systemd.json5");

    // Load configuration via the framework's SensorConfig trait.
    let config = SystemdSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;
    let source = config.source();

    // Create the sensor runner.
    let runner = SensorRunner::new_with_args("systemd", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing and pin JSON serialization.
    let runner = runner.with_status_publishing().with_format(Format::Json);

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "systemd",
        source.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    let runner = runner.with_report(report_source);

    // Tier-2 directory snapshots (`@/snapshot`). No-op unless `snapshot.enabled`.
    let mut runner = runner.with_snapshot(source.clone());

    let systemd_config = runner.config().systemd.clone();

    tracing::info!(
        "systemd sensor running (prefix: {}, interval: {}s, source: {})",
        systemd_config.key_prefix,
        systemd_config.poll_interval_secs,
        source
    );

    // Shared control-plane event ring (#275), fed by the D-Bus event stream and
    // served on @/query/events.
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
    // @/query/{units,failed,unit,events}.
    let query_session = runner.session().clone();
    let query_prefix = systemd_config.key_prefix.clone();
    let query_events = event_state.clone();
    runner.spawn(async move {
        zensight_sensor_systemd::query::run(query_session, query_prefix, query_events).await;
    });

    // Shared AlertReporter → zensight/systemd/@/alerts/* for both the built-in
    // threshold alerts (#276) and the sentinel (#277), with one late-joiner
    // firing-set seed on @/query/alerts. Created when either feature is active.
    let expectations = systemd_config.expectations.clone();
    let alerts_active = systemd_config.alerts.enabled || expectations.is_some();
    let reporter = alerts_active.then(|| {
        let r = Arc::new(
            AlertReporter::new(runner.publisher(), Protocol::Systemd, Format::Json)
                .with_debounce(Duration::from_secs(systemd_config.alerts.for_secs)),
        );
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
    // via @/commands/expectations (+ @/status/expectations). Needs its own D-Bus
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
                let cmd_prefix = systemd_config.key_prefix.clone();
                runner.spawn(async move {
                    zensight_sensor_systemd::command::run(cmd_session, cmd_prefix, handle).await;
                });
                tracing::info!("systemd sentinel enabled");
            }
            Err(e) => tracing::error!(error = %e, "systemd sentinel: system bus connect failed"),
        }
    }

    let metadata = serde_json::json!({
        "source": source,
        "poll_interval_secs": systemd_config.poll_interval_secs,
    });

    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
