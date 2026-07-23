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

    // Clone config data we need before spawning tasks
    let snmp_config = runner.config().snmp.clone();
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

    // Load additional MIB files
    for mib_file in &snmp_config.mib.files {
        if let Err(e) = mib_resolver.load_file(mib_file) {
            tracing::warn!(file = %mib_file, error = %e, "Failed to load MIB file");
        } else {
            tracing::info!(file = %mib_file, "Loaded MIB file");
        }
    }

    // Add custom OID mappings from config
    if !snmp_config.oid_names.is_empty() {
        mib_resolver.add_custom_mappings(&snmp_config.oid_names);
        tracing::info!(
            count = snmp_config.oid_names.len(),
            "Added custom OID mappings"
        );
    }

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

        // Initialize poller (required for SNMPv3 to discover engine ID)
        if let Err(e) = poller.init().await {
            tracing::error!(
                device = %device.name,
                error = %e,
                "Failed to initialize SNMP poller, skipping device"
            );
            continue;
        }

        runner.spawn(async move {
            poller.run().await;
        });
    }

    // Spawn trap receiver if enabled
    if snmp_config.trap_listener.enabled {
        let trap_receiver = TrapReceiver::new(
            &snmp_config.trap_listener.bind,
            session.clone(),
            mib_resolver.clone(),
            serialization,
        );

        runner.spawn(async move {
            if let Err(e) = trap_receiver.run().await {
                tracing::error!(error = %e, "Trap receiver failed");
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
