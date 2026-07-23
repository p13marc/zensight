//! Zenoh sensor for SNMP telemetry.
//!
//! This sensor polls SNMP devices and publishes telemetry to Zenoh.

use std::sync::Arc;

use anyhow::Result;
use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};

use zensight_sensor_snmp::config::SnmpSensorConfig;
use zensight_sensor_snmp::mib::MibResolver;
use zensight_sensor_snmp::poller::SnmpPoller;
use zensight_sensor_snmp::trap::TrapReceiver;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("snmp.json5");

    // Load configuration using the framework's SensorConfig trait
    let config = SnmpSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;
    let source = config.snmp.resolved_source();

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("snmp", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing

    // On-demand debug-report (the artifact channel): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config. SNMP secrets
    // (community, auth/priv passwords) are caught by the framework's redaction.
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "snmp",
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

    // Get session for setting up pollers
    let session = runner.session().clone();

    // Clone config data we need before spawning tasks. Credential sets and
    // ${ENV}/file: secret indirection resolve here (#538) — hard errors —
    // and only this resolved copy carries live secrets; the runner's stored
    // config (which feeds the redacted debug bundle) keeps the references.
    let mut snmp_config = runner.config().snmp.clone();
    snmp_config
        .resolve_credentials()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let serialization = runner.config().serialization;

    // Initialize MIB resolver
    let mut mib_resolver = MibResolver::new();

    if snmp_config.mib.load_builtin {
        mib_resolver
            .load_builtin_mibs()
            .map_err(|e| anyhow::anyhow!("Failed to load built-in MIBs: {}", e))?;
        tracing::info!(
            modules = ?mib_resolver.loaded_modules(),
            count = mib_resolver.mapping_count(),
            "Loaded built-in MIB definitions"
        );
    }

    // Load additional MIB files (legacy JSON pseudo-MIBs; deprecated #532)
    if !snmp_config.mib.files.is_empty() {
        tracing::warn!(
            "snmp.mib.files (JSON pseudo-MIBs) is deprecated — move standard SMI \
             .mib files into snmp.mib.dirs; JSON support goes away next release"
        );
    }
    for mib_file in &snmp_config.mib.files {
        if let Err(e) = mib_resolver.load_file(mib_file) {
            tracing::warn!(file = %mib_file, error = %e, "Failed to load MIB file");
        } else {
            tracing::info!(file = %mib_file, "Loaded MIB file");
        }
    }

    // Real SMI MIBs (#532): vendor files drop into mib.dirs unmodified.
    let smi = if snmp_config.mib.dirs.is_empty() {
        None
    } else {
        let resolver = zensight_sensor_snmp::smi::SmiResolver::load_dirs(&snmp_config.mib.dirs)
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        tracing::info!(dirs = ?snmp_config.mib.dirs, "Loaded SMI MIB modules");
        Some(Arc::new(resolver))
    };

    // Add custom OID mappings from config
    if !snmp_config.oid_names.is_empty() {
        mib_resolver.add_custom_mappings(&snmp_config.oid_names);
        tracing::info!(
            count = snmp_config.oid_names.len(),
            "Added custom OID mappings"
        );
    }

    // Device profiles (#531): shipped base set + user dirs. A malformed or
    // dangling profile is a startup error — never a silently-thinner fleet.
    let profiles = if snmp_config.profiles.enabled {
        let mut set = zensight_sensor_snmp::profile::ProfileSet::builtin();
        for dir in &snmp_config.profiles.dirs {
            let loaded = set
                .load_dir(std::path::Path::new(dir))
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;
            tracing::info!(dir = %dir, loaded, "Loaded user device profiles");
        }
        set.validate().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        // Profile naming/SYNTAX tables are fleet-wide; config `oid_names`
        // and builtins added above take precedence on collisions.
        mib_resolver.add_profile_mappings(&set.all_oid_names(), &set.all_oid_syntax());
        Some(Arc::new(set))
    } else {
        None
    };

    let mib_resolver = Arc::new(mib_resolver);

    // Threshold alerting (#528): one shared reporter, one evaluator per
    // device (rules/thresholds per device via `devices[].alerts` override).
    let alert_reporter = if snmp_config.alerts.enabled {
        use zensight_common::Protocol;
        use zensight_sensor_core::{AlertReporter, serve_alerts_query};
        let mut reporter = AlertReporter::new(runner.publisher(), Protocol::Snmp, serialization)
            .with_debounce(std::time::Duration::from_secs(snmp_config.alerts.for_secs));
        if let Some(id) = runner.identity() {
            reporter = reporter.with_identity(id);
        }
        let reporter = Arc::new(reporter);
        runner.spawn(serve_alerts_query(reporter.clone()));
        tracing::info!("SNMP threshold alerting enabled");
        Some(reporter)
    } else {
        None
    };

    // Shared advanced-publisher registry for the per-device InterfaceTable
    // state docs (#529): cache 1 → late joiners seed the current doc.
    let interfaces_registry = snmp_config.publish_interfaces.then(|| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::V1Context::for_producer(
                    &zensight_common::PROFILE,
                    "snmp",
                )
                .telemetry_prefix(),
                serialization,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::HealthLiveness),
        )
    });

    // Observed-device evidence (#537): shared Evidence-QoS advanced registry
    // (cache 1 → the correlator seeds current claims on late join).
    let evidence_registry = snmp_config.evidence.enabled.then(|| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::V1Context::for_producer(
                    &zensight_common::PROFILE,
                    "snmp",
                )
                .telemetry_prefix(),
                serialization,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        )
    });

    runner
        .health()
        .set_devices_total(snmp_config.devices.len() as u64);

    // Fleet-known IPs (#541): configured addresses now, evidence-observed
    // IPs as pollers learn them. Discovery never re-proposes any of these.
    let known_ips = Arc::new(std::sync::RwLock::new(
        snmp_config
            .devices
            .iter()
            .filter_map(|d| {
                d.address
                    .rsplit_once(':')
                    .map(|(host, _)| host.trim_start_matches('[').trim_end_matches(']'))
                    .filter(|h| h.parse::<std::net::IpAddr>().is_ok())
                    .map(str::to_string)
            })
            .collect::<std::collections::HashSet<_>>(),
    ));

    // Spawn device pollers
    for device in snmp_config.devices.clone() {
        let mut poller = SnmpPoller::new(
            device.clone(),
            session.clone(),
            mib_resolver.clone(),
            &snmp_config.oid_groups,
            serialization,
        );

        if let Some(reporter) = &alert_reporter {
            let cfg = device
                .alerts
                .clone()
                .unwrap_or_else(|| snmp_config.alerts.clone());
            if cfg.enabled {
                let evaluator = zensight_sensor_snmp::alerts::AlertEvaluator::new(
                    device.name.clone(),
                    cfg,
                    reporter.clone(),
                );
                poller.with_alerts(evaluator);
            }
        }

        if let Some(registry) = &interfaces_registry {
            poller.with_interfaces_doc(registry.clone());
        }

        if let Some(profiles) = &profiles {
            poller.with_profiles(profiles.clone());
        }

        if let Some(smi) = &smi {
            poller.with_smi(smi.clone());
        }

        if let Some(registry) = &evidence_registry {
            poller.with_evidence(registry.clone(), snmp_config.evidence.refresh_cycles);
        }

        poller.with_resilience(snmp_config.resilience);
        poller.with_health(runner.health());
        poller.with_known_ips(known_ips.clone());

        // Initialize the client. A failure no longer drops the device
        // (#539): the poll loop keeps retrying with backoff, so a device
        // that is offline at startup starts working when it comes online.
        if let Err(e) = poller.init().await {
            tracing::warn!(
                device = %device.name,
                error = %e,
                "SNMP client init failed; will keep retrying with backoff"
            );
        }

        runner.spawn(async move {
            poller.run().await;
        });
    }

    // Subnet auto-discovery (#541): opt-in, propose-only.
    if let Some(discovery_config) = snmp_config.discovery.clone() {
        let credentials: Vec<(String, zensight_sensor_snmp::config::CredentialSet)> =
            discovery_config
                .credentials
                .iter()
                .map(|name| {
                    snmp_config
                        .credentials
                        .get(name)
                        .cloned()
                        .map(|set| (name.clone(), set))
                        .ok_or_else(|| {
                            anyhow::anyhow!("discovery references unknown credential set {name:?}")
                        })
                })
                .collect::<Result<_>>()?;

        let interval = std::time::Duration::from_secs(discovery_config.interval_secs.max(60));
        let mut discovery = zensight_sensor_snmp::discovery::Discovery::new(
            discovery_config,
            credentials,
            known_ips.clone(),
        );
        if let Some(profiles) = &profiles {
            discovery.with_profiles(profiles.clone());
        }
        // Startup validation: an over-broad CIDR fails loudly, never scans.
        let addresses = discovery
            .addresses()
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        tracing::info!(
            addresses = addresses.len(),
            interval_secs = interval.as_secs(),
            "SNMP subnet discovery enabled (propose-only)"
        );

        let report_registry = zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "snmp")
                .telemetry_prefix(),
            serialization,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_common::QosClass::HealthLiveness);
        let report_key: String =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "snmp")
                .state_key(&["discovery"])
                .into();

        runner.spawn(async move {
            loop {
                let report = discovery.sweep(&addresses).await;
                tracing::info!(
                    scanned = report.scanned,
                    discovered = report.discovered.len(),
                    "discovery sweep complete"
                );
                if let Err(e) = report_registry
                    .publish_serializable(&report_key, &report)
                    .await
                {
                    tracing::warn!(error = %e, "discovery report publish failed");
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // Spawn trap receiver if enabled (#535): durable events + alert mapping.
    if snmp_config.trap_listener.enabled {
        let mut trap_receiver = TrapReceiver::new(
            snmp_config.trap_listener.clone(),
            session.clone(),
            mib_resolver.clone(),
            serialization,
        );
        if let Some(smi) = &smi {
            trap_receiver.with_smi(smi.clone());
        }
        if let Some(reporter) = &alert_reporter {
            trap_receiver.with_alerts(reporter.clone());
        }

        runner.spawn(async move {
            match trap_receiver.bind().await {
                Ok(bound) => {
                    if let Err(e) = bound.run().await {
                        tracing::error!(error = %e, "Trap receiver failed");
                    }
                }
                Err(e) => tracing::error!(error = %e, "Trap listener bind failed"),
            }
        });
    }

    // Build status metadata
    let metadata = serde_json::json!({
        "devices": snmp_config.devices.iter().map(|d| &d.name).collect::<Vec<_>>(),
        "trap_listener": snmp_config.trap_listener.enabled,
        "mib_modules": mib_resolver.loaded_modules(),
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}
