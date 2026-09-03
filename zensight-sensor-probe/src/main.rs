//! Outside-in probe sensor binary (#820).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zensight_sensor_core::{AlertReporter, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_probe::config::ProbeSensorConfig;
use zensight_sensor_probe::poller::Poller;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("probe.json5");
    let config = ProbeSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let source = config.probe.resolved_source();
    let vantage = config.probe.resolved_vantage();
    let pc = config.probe.clone();

    // rustls needs a process-wide provider before any handshake. Installing it
    // here rather than lazily means a misconfiguration fails at start, once,
    // instead of on the first TLS target.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut runner = SensorRunner::new_with_args("probe", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();

    // The shared reporter. Unconditional since #931: `alerts.enabled` governs
    // this sensor's OWN judgements about a check's outcome, but an operator can
    // push a threshold rule to a running sensor over `@desired` or `@rpc`, so a
    // build that could not report an alert would have had to refuse a rule it
    // had just declared it accepts.
    let reporter = {
        let mut r =
            AlertReporter::new(runner.publisher(), zensight_common::Protocol::Probe, format)
                .with_debounce(Duration::from_secs(pc.alerts.for_secs))
                // The rule table this build can still raise, so a restart
                // retires an inherited alert for a rule that is gone (#882).
                .with_known_rules(zensight_sensor_probe::alerts::ALL_RULES.iter().copied());
        if let Some(id) = runner.identity() {
            r = r.with_identity(id);
        }
        Arc::new(r)
    };
    runner = runner.with_alert_reporter(reporter.clone());

    // The operator's threshold rules over this sensor's own telemetry (#931):
    // `duration_ms`, `tls/days_remaining`, `clock/offset_ms` — the numbers this
    // crate's docs have been promising an operator would get to choose. Every
    // one of them rides `Publisher::publish`, so the runner's publisher is the
    // whole surface here.
    zensight_sensor_core::threshold::adopt(
        &mut runner,
        zensight_common::Protocol::Probe,
        reporter.clone(),
        {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::probe_thresholds(
                zensight_common::PROFILE.host_id(),
            ))
        },
        &[],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            runner.session().clone(),
            zensight_sensor_core::v1::for_producer("probe").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_probe::poller::STATE_QOS),
    );

    if pc.targets.is_empty() {
        // Valid, and worth saying out loud: an empty target list is a sensor
        // that will publish a health document forever and check nothing, which
        // is indistinguishable from a broken one unless it says so (#867's
        // lesson, one crate over).
        tracing::warn!(
            "probe: no targets configured — this sensor will check nothing. Add \
             probe.targets to configs/probe.json5."
        );
    }
    tracing::info!(
        vantage = %vantage,
        targets = pc.targets.iter().filter(|t| t.enabled).count(),
        interval_secs = pc.interval_secs,
        max_concurrent = pc.max_concurrent,
        icmp = cfg!(feature = "icmp"),
        "probe sensor running (a client only — no listeners, no action surface). \
         NOTE: a probe running on the server cannot tell you the server is \
         unreachable; this does not replace external outage monitoring."
    );

    let poller = Poller::new(
        pc.clone(),
        runner.publisher(),
        states,
        pc.alerts.enabled.then(|| reporter.clone()),
        runner.health(),
        zensight_sensor_core::relation::RelationSet::new("probe", runner.session().clone(), format),
    )?;
    runner.spawn(poller.run());

    runner
        .run_with_metadata(Some(serde_json::json!({
            "vantage": vantage,
            "targets": pc.targets.iter().filter(|t| t.enabled).map(|t| &t.name).collect::<Vec<_>>(),
            "icmp": cfg!(feature = "icmp"),
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}
