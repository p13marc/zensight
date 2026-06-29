//! Zenoh sensor for SNMP telemetry.
//!
//! This sensor polls SNMP devices and publishes telemetry to Zenoh.

mod config;
mod mib;
mod oid;
mod poller;
mod trap;

use std::sync::Arc;

use anyhow::Result;
use zensight_sensor_core::{SensorArgs, SensorRunner};

use crate::config::SnmpSensorConfig;
use crate::mib::MibResolver;
use crate::poller::SnmpPoller;
use crate::trap::TrapReceiver;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("snmp.json5");

    // Load configuration using the framework's SensorConfig trait
    let config = SnmpSensorConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("snmp", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config. SNMP secrets
    // (community, auth/priv passwords) are caught by the framework's redaction.
    let report_host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "snmp",
        report_host,
        runner.config().clone(),
        runner.health(),
    ));
    let mut runner = runner.with_report(report_source);

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

    // Spawn device pollers
    for device in snmp_config.devices.clone() {
        let mut poller = SnmpPoller::new(
            device.clone(),
            session.clone(),
            &snmp_config.key_prefix,
            mib_resolver.clone(),
            &snmp_config.oid_groups,
            serialization,
        );

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
            &snmp_config.key_prefix,
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
