//! Zenoh sensor for systemd unit/service state and boot performance.
//!
//! Reads the `org.freedesktop.systemd1.Manager` D-Bus interface (system bus) and
//! publishes unit-state aggregates + boot-performance timings to Zenoh under
//! `zensight/systemd/<host>/…`. Fails gracefully (reports unhealthy, never
//! crashes) on non-systemd hosts.

use anyhow::Result;
use zensight_sensor_core::{Format, SensorArgs, SensorConfig, SensorRunner};

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

    // D-Bus event stream (#275): watched UnitNew/Removed + JobNew/Removed → ring.
    let watch = zensight_sensor_systemd::config::compile_watch(&systemd_config.watch_units);
    let events_state = event_state.clone();
    runner.spawn(async move {
        zensight_sensor_systemd::events::run(watch, events_state, None).await;
    });

    // On-demand unit inventory query channel (#274/#275):
    // @/query/{units,failed,unit,events}.
    let query_session = runner.session().clone();
    let query_prefix = systemd_config.key_prefix.clone();
    let query_events = event_state.clone();
    runner.spawn(async move {
        zensight_sensor_systemd::query::run(query_session, query_prefix, query_events).await;
    });

    // Spawn the collector task.
    let collector = SystemdCollector::new(
        source.clone(),
        systemd_config.clone(),
        runner.publisher(),
        runner.health(),
    )
    .with_events(event_state);
    runner.spawn(async move {
        collector.run().await;
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
