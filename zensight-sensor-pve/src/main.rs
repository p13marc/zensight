//! Proxmox VE sensor binary (#818).
//!
//! The startup shape is the framework's, in the SNMP/gNMI polling-sensor
//! dialect: runner, identity, a shared alert reporter with a late-joiner
//! seed, advanced publishers for the state documents and the evidence claims,
//! then one poller.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zensight_sensor_core::{AlertReporter, SensorArgs, SensorConfig, SensorRunner, resolve_secret};

use zensight_sensor_pve::api::PveClient;
use zensight_sensor_pve::config::PveSensorConfig;
use zensight_sensor_pve::poller::Poller;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("pve.json5");

    let config = PveSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let source = config.pve.resolved_source();

    // The token never sits in the config file in a deployment: `file:/path`
    // (root-0600) or `${ENV}`. Resolved before the runner so a missing secret
    // fails at start, loudly, rather than as a 401 every minute.
    let token = resolve_secret(&config.pve.token).map_err(|e| anyhow::anyhow!("{e}"))?;
    let token = if token.starts_with("PVEAPIToken=") {
        token
    } else {
        format!("PVEAPIToken={token}")
    };

    let pve = config.pve.clone();
    let mut runner = SensorRunner::new_with_args("pve", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();

    if pve.accept_invalid_certs {
        tracing::warn!(
            host = %pve.host,
            "pve: accept_invalid_certs is ON — the API endpoint is NOT authenticated. \
             A stock Proxmox certificate is self-signed, so this is the documented \
             opt-in; replacing the certificate is the better answer."
        );
    }

    let client = Arc::new(PveClient::new(
        pve.base_url(),
        token,
        Duration::from_secs(pve.timeout_secs),
        pve.accept_invalid_certs,
        pve.max_concurrent,
    )?);

    let reporter = if pve.alerts.enabled {
        let mut r = AlertReporter::new(runner.publisher(), zensight_common::Protocol::Pve, format)
            .with_debounce(Duration::from_secs(pve.alerts.for_secs))
            // The rule table this build can still raise, so a restart retires
            // an inherited alert for a rule that no longer exists (#882).
            .with_known_rules(zensight_sensor_pve::alerts::ALL_RULES.iter().copied());
        if let Some(id) = runner.identity() {
            r = r.with_identity(id);
        }
        let r = Arc::new(r);
        Some(r)
    } else {
        tracing::warn!("pve: alerts are disabled — telemetry only, nothing is asserted");
        None
    };

    // State documents ride an advanced publisher with cache 1, so a late
    // joiner (the GUI, a storage) seeds the current document rather than
    // waiting a poll interval to learn the hypervisor exists.
    let session = runner.session().clone();
    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::for_producer("pve").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_pve::poller::STATE_QOS),
    );
    let evidence = pve.evidence.then(|| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::for_producer("pve").telemetry_prefix(),
                format,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        )
    });

    tracing::info!(
        host = %pve.host,
        port = pve.port,
        poll_interval_secs = pve.poll_interval_secs,
        config_interval_secs = pve.config_interval_secs,
        backup_interval_secs = pve.backup_interval_secs,
        alerts = pve.alerts.enabled,
        "pve sensor running (read-only; there is no action surface)"
    );

    let poller = Poller::new(
        client,
        pve.clone(),
        source.clone(),
        runner.publisher(),
        states,
        evidence,
        reporter.clone(),
        runner.health(),
    );
    runner.spawn(poller.run());
    if let Some(r) = reporter {
        runner = runner.with_alert_reporter(r);
    }

    runner
        .run_with_metadata(Some(serde_json::json!({
            "endpoint": format!("{}:{}", pve.host, pve.port),
            "nodes": pve.nodes,
            "poll_interval_secs": pve.poll_interval_secs,
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}
