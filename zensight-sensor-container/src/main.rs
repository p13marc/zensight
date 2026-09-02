//! OCI container sensor binary (#819).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zensight_sensor_core::{AlertReporter, SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_container::config::ContainerSensorConfig;
use zensight_sensor_container::poller::Poller;
use zensight_sensor_container::runtime::{RuntimeClient, default_sockets};
use zensight_sensor_container::upstream::UpstreamChecker;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("container.json5");
    let config = ContainerSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let source = config.container.resolved_source();
    let cc = config.container.clone();

    let mut runner = SensorRunner::new_with_args("container", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();

    let timeout = Duration::from_secs(cc.timeout_secs);
    // An explicit list wins; otherwise the conventional paths, and only the
    // ones that exist — listing a socket that is not there would make every
    // cycle log a failure for a runtime this host does not run.
    let sockets: Vec<(std::path::PathBuf, bool)> = if cc.sockets.is_empty() {
        default_sockets()
            .into_iter()
            .filter(|(p, _)| p.exists())
            .collect()
    } else {
        cc.sockets
            .iter()
            .map(|s| {
                // A socket under a user runtime dir is a rootless session, and
                // that changes where its containers' cgroups live.
                let rootless = s.contains("/run/user/");
                (std::path::PathBuf::from(s), rootless)
            })
            .collect()
    };
    if sockets.is_empty() {
        // Loud, and not fatal: a host may gain a runtime later, and a sensor
        // that exits here would need a restart to notice.
        tracing::warn!(
            "container: no runtime socket found (looked for rootful/rootless podman and \
             docker). The sensor will keep running and report the failure in its health \
             document; set container.sockets if yours is elsewhere."
        );
    }
    let clients: Vec<Arc<RuntimeClient>> = sockets
        .into_iter()
        .map(|(p, rootless)| Arc::new(RuntimeClient::new(p, timeout, rootless)))
        .collect();

    let reporter = if cc.alerts.enabled {
        let mut r = AlertReporter::new(
            runner.publisher(),
            zensight_common::Protocol::Container,
            format,
        )
        .with_debounce(Duration::from_secs(cc.alerts.for_secs))
        // The rule table this build can still raise, so a restart retires an
        // inherited alert for a rule that no longer exists (#882).
        .with_known_rules(zensight_sensor_container::alerts::ALL_RULES.iter().copied());
        if let Some(id) = runner.identity() {
            r = r.with_identity(id);
        }
        let r = Arc::new(r);
        Some(r)
    } else {
        None
    };

    let session = runner.session().clone();
    let advanced = |qos| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::for_producer("container").telemetry_prefix(),
                format,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(qos),
        )
    };
    let states = advanced(zensight_sensor_container::poller::STATE_QOS);
    let evidence = cc
        .evidence
        .then(|| advanced(zensight_common::QosClass::Evidence));

    let upstream = if cc.upstream.enabled {
        tracing::warn!(
            registries = ?cc.upstream.registries,
            signatures = cc.upstream.signatures,
            interval_secs = cc.upstream.interval_secs,
            "container: the upstream collector is ON — this sensor will make anonymous \
             read-only requests to the registries listed above. It is the only part of \
             this sensor that leaves the host."
        );
        Some(UpstreamChecker::new(&cc.upstream, timeout)?)
    } else {
        None
    };

    tracing::info!(
        sockets = clients.len(),
        poll_interval_secs = cc.poll_interval_secs,
        cgroup_root = %cc.cgroup_root,
        alerts = cc.alerts.enabled,
        egress = cc.upstream.enabled,
        "container sensor running (read-only; there is no action surface)"
    );

    let poller = Poller::new(
        clients,
        cc.clone(),
        source.clone(),
        runner.publisher(),
        states,
        evidence,
        reporter.clone(),
        runner.health(),
        upstream,
        zensight_sensor_core::relation::RelationSet::new(
            "container",
            runner.session().clone(),
            format,
        ),
    );
    runner.spawn(poller.run());
    if let Some(r) = reporter {
        runner = runner.with_alert_reporter(r);
    }

    runner
        .run_with_metadata(Some(serde_json::json!({
            "poll_interval_secs": cc.poll_interval_secs,
            "egress": cc.upstream.enabled,
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}
