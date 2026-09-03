//! Out-of-band hardware sensor binary (#953).
//!
//! The startup shape is the framework's, in the `pve`/`probe` polling dialect:
//! runner, identity, a shared alert reporter with a late-joiner seed, advanced
//! publishers for the state documents and the identity claims, then one
//! poller over every configured chassis.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zensight_sensor_core::{AlertReporter, SensorArgs, SensorConfig, SensorRunner, resolve_secret};

use zensight_sensor_bmc::config::BmcSensorConfig;
use zensight_sensor_bmc::poller::Poller;
use zensight_sensor_bmc::redfish::RedfishClient;

#[tokio::main]
async fn main() -> Result<()> {
    let args = SensorArgs::parse_with_default("bmc.json5");
    let config = BmcSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let source = config.bmc.resolved_source();
    let bmc = config.bmc.clone();

    // Passwords are resolved BEFORE the runner: a missing secret should fail
    // at start, loudly, rather than as a 401 every minute.
    let mut clients = HashMap::new();
    for endpoint in bmc.endpoints.iter().filter(|e| e.enabled) {
        let password = resolve_secret(&endpoint.password).map_err(|e| anyhow::anyhow!("{e}"))?;
        let ca_pem = match &endpoint.ca_file {
            Some(path) => Some(std::fs::read(path).map_err(|e| {
                anyhow::anyhow!("endpoint {:?}: reading ca_file {path}: {e}", endpoint.name)
            })?),
            None => None,
        };
        if endpoint.insecure {
            // Said on every boot, not once in a comment. An operator who
            // inherited this config has to be able to learn it from the log.
            tracing::warn!(
                chassis = %endpoint.name,
                address = %endpoint.address,
                "bmc: insecure is ON for this endpoint — its TLS certificate is NOT verified, \
                 so the BMC is not authenticated. Point ca_file at the CA that signed it"
            );
        }
        clients.insert(
            endpoint.name.clone(),
            Arc::new(RedfishClient::new(
                endpoint.base_url(),
                endpoint.username.clone(),
                password,
                Duration::from_secs(endpoint.timeout(bmc.timeout_secs)),
                endpoint.insecure,
                ca_pem,
                bmc.max_concurrent,
            )?),
        );
    }

    if clients.is_empty() {
        // A no-op, not an error: this is what a config shipped to a fleet
        // where only some hosts manage a BMC looks like.
        tracing::warn!("bmc: no enabled endpoints — this sensor will publish nothing but health");
    }

    let mut runner = SensorRunner::new_with_args("bmc", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();

    let reporter = if bmc.alerts.enabled {
        let mut r = AlertReporter::new(runner.publisher(), zensight_common::Protocol::Bmc, format)
            .with_debounce(Duration::from_secs(bmc.alerts.for_secs))
            // The rule table this build can still raise, so a restart retires
            // an inherited alert for a rule that no longer exists (#882).
            .with_known_rules(zensight_sensor_bmc::alerts::ALL_RULES.iter().copied());
        if let Some(id) = runner.identity() {
            r = r.with_identity(id);
        }
        Some(Arc::new(r))
    } else {
        tracing::warn!("bmc: alerts are disabled — telemetry only, nothing is asserted");
        None
    };

    // State documents ride an advanced publisher with cache 1, so a late
    // joiner seeds the current document instead of waiting a whole interval to
    // learn that a supply failed.
    let session = runner.session().clone();
    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::for_producer("bmc").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_bmc::poller::STATE_QOS),
    );
    let evidence = bmc.evidence.then(|| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::for_producer("bmc").telemetry_prefix(),
                format,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        )
    });

    tracing::info!(
        endpoints = clients.len(),
        poll_interval_secs = bmc.interval_secs,
        "bmc sensor running (read-only; there is no action surface)"
    );

    let poller = Poller::new(
        bmc.clone(),
        source.clone(),
        clients,
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
            "endpoints": bmc.endpoints.iter().filter(|e| e.enabled).map(|e| &e.name).collect::<Vec<_>>(),
            "poll_interval_secs": bmc.interval_secs,
            "ipmi": cfg!(feature = "ipmi"),
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}
