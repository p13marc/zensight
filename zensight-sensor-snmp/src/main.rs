//! Zenoh sensor for SNMP telemetry.
//!
//! This sensor polls SNMP devices and publishes telemetry to Zenoh.

use std::sync::Arc;

use anyhow::Result;
use zensight_common::v1::V1ContextExt;
use zensight_sensor_core::{SensorConfig, SensorRunner};

use zensight_sensor_snmp::cli::SnmpArgs;
use zensight_sensor_snmp::config::SnmpSensorConfig;
use zensight_sensor_snmp::mib::MibResolver;
use zensight_sensor_snmp::poller::SnmpPoller;
use zensight_sensor_snmp::trap::TrapReceiver;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SnmpArgs::parse_with_default("snmp.json5");

    // The one-shot discovery mode (#825 item 4) short-circuits HERE, before
    // the runner, the session or any publisher exists. An operator sweeping a
    // subnet from a laptop should not thereby join a fleet, and the way to
    // guarantee that is to never build the thing that would.
    if let Some(cidr) = args.discover.clone() {
        // Diagnostics go to stderr and the proposal to stdout, so
        // `--discover … > devices.json5` yields a file that is only the
        // proposal. No tracing subscriber is installed at all on this path:
        // the sweep says what it is doing in plain sentences.
        let found = zensight_sensor_snmp::cli::discover(&args, &cidr).await?;
        // A sweep that found nothing is a finding, not a failure of the sweep,
        // so the exit code stays 0 and the proposal says so in words.
        let _ = found;
        return Ok(());
    }
    let args = args.common;

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

    // Legacy JSON pseudo-MIBs: deprecated #532, removed #580. One release of
    // hard error beats silently ignoring the setting.
    if !snmp_config.mib.files.is_empty() {
        anyhow::bail!(
            "snmp.mib.files (JSON pseudo-MIBs) was removed (#580) — convert the \
             definitions to standard SMI .mib files and list their directory in \
             snmp.mib.dirs"
        );
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
        // #559: names ride the telemetry key chunk-for-chunk. A violating
        // name still publishes (slugged losslessly at the boundary), but the
        // published key then won't look like the configured name — say so.
        for name in snmp_config.oid_names.values() {
            let ok = name
                .split('/')
                .map(|c| c.replace("{index}", "1"))
                .all(|c| zenkey::grammar::is_valid_plain_chunk(&c));
            if !ok {
                tracing::warn!(
                    name = %name,
                    "oid_names entry violates the key chunk grammar (lowercase alnum + `._-`); \
                     it will be escaped on the wire — prefer a lowercase name (#559)"
                );
            }
        }
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
    //
    // The reporter is unconditional since #931: `alerts.enabled` governs this
    // sensor's own per-device evaluators, but an operator can push a threshold
    // rule to a running sensor over `@desired` or `@rpc`, so a build that
    // could not report an alert would have had to refuse a rule it had just
    // declared it accepts.
    let alert_reporter = {
        use zensight_common::Protocol;
        use zensight_sensor_core::AlertReporter;
        let mut reporter = AlertReporter::new(runner.publisher(), Protocol::Snmp, serialization)
            .with_debounce(std::time::Duration::from_secs(snmp_config.alerts.for_secs));
        if let Some(id) = runner.identity() {
            reporter = reporter.with_identity(id);
        }
        let reporter = Arc::new(reporter);
        runner = runner.with_alert_reporter(reporter.clone());
        if snmp_config.alerts.enabled {
            tracing::info!("SNMP threshold alerting enabled");
        }
        reporter
    };

    // The operator's threshold rules over every metric this proxy polls
    // (#931). `source` is a label in a `ThresholdRule`, so one rule can name
    // one device or match them all — which is what the shared vocabulary was
    // shaped for, without it needing to know that proxies exist. The evaluator
    // is installed on each poller's own registry below, and on the trap
    // receiver's, because that is where a polled point actually goes.
    let thresholds = zensight_sensor_core::threshold::adopt(
        &mut runner,
        zensight_common::Protocol::Snmp,
        alert_reporter.clone(),
        {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::snmp_thresholds(
                zensight_common::PROFILE.host_id(),
            ))
        },
        &[],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Shared advanced-publisher registry for the per-device InterfaceTable
    // state docs (#529): cache 1 → late joiners seed the current doc.
    let interfaces_registry = snmp_config.publish_interfaces.then(|| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::for_producer("snmp").telemetry_prefix(),
                serialization,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_counters(runner.publisher().counters())
            .with_qos(zensight_common::QosClass::HealthLiveness),
        )
    });

    // Observed-device evidence (#537): shared Evidence-QoS advanced registry
    // (cache 1 → the correlator seeds current claims on late join).
    let evidence_registry = snmp_config.evidence.enabled.then(|| {
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::for_producer("snmp").telemetry_prefix(),
                serialization,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_counters(runner.publisher().counters())
            .with_qos(zensight_common::QosClass::Evidence),
        )
    });

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

    // Device pollers, as a set that can change without a restart (#936).
    //
    // Everything a poller needs is captured once here; the supervisor
    // (`fleet::DeviceFleet`) knows none of it and only decides who runs.
    let spawn_device: zensight_sensor_snmp::fleet::SpawnDevice = {
        let session = session.clone();
        let mib_resolver = mib_resolver.clone();
        let oid_groups = snmp_config.oid_groups.clone();
        let default_alerts = snmp_config.alerts.clone();
        let thresholds = thresholds.clone();
        let alert_reporter = alert_reporter.clone();
        let interfaces_registry = interfaces_registry.clone();
        let profiles = profiles.clone();
        let smi = smi.clone();
        let evidence_registry = evidence_registry.clone();
        let evidence_cycles = snmp_config.evidence.refresh_cycles;
        let resilience = snmp_config.resilience;
        let health = runner.health();
        let known_ips = known_ips.clone();
        std::sync::Arc::new(move |device: zensight_sensor_snmp::config::DeviceConfig| {
            let session = session.clone();
            let mib_resolver = mib_resolver.clone();
            let oid_groups = oid_groups.clone();
            let default_alerts = default_alerts.clone();
            let thresholds = thresholds.clone();
            let alert_reporter = alert_reporter.clone();
            let interfaces_registry = interfaces_registry.clone();
            let profiles = profiles.clone();
            let smi = smi.clone();
            let evidence_registry = evidence_registry.clone();
            let health = health.clone();
            let known_ips = known_ips.clone();
            tokio::spawn(async move {
                let mut poller = SnmpPoller::new(
                    device.clone(),
                    session,
                    mib_resolver,
                    &oid_groups,
                    serialization,
                );
                poller.with_thresholds(thresholds);
                {
                    let cfg = device.alerts.clone().unwrap_or(default_alerts);
                    if cfg.enabled {
                        let evaluator = zensight_sensor_snmp::alerts::AlertEvaluator::new(
                            device.name.clone(),
                            cfg,
                            alert_reporter,
                        );
                        poller.with_alerts(evaluator);
                    }
                }
                if let Some(registry) = interfaces_registry {
                    poller.with_interfaces_doc(registry);
                }
                if let Some(profiles) = profiles {
                    poller.with_profiles(profiles);
                }
                if let Some(smi) = smi {
                    poller.with_smi(smi);
                }
                if let Some(registry) = evidence_registry {
                    poller.with_evidence(registry, evidence_cycles);
                }
                poller.with_resilience(resilience);
                poller.with_health(health);
                poller.with_known_ips(known_ips);

                // Initialize the client. A failure no longer drops the device
                // (#539): the poll loop keeps retrying with backoff, so a
                // device that is offline at startup starts working when it
                // comes online.
                if let Err(e) = poller.init().await {
                    tracing::warn!(
                        device = %device.name,
                        error = %e,
                        "SNMP client init failed; will keep retrying with backoff"
                    );
                }
                poller.run().await;
            })
        })
    };

    let fleet = std::sync::Arc::new(tokio::sync::Mutex::new(
        zensight_sensor_snmp::fleet::DeviceFleet::new(spawn_device),
    ));
    {
        let mut f = fleet.lock().await;
        let change = f.apply(&snmp_config.devices);
        tracing::info!(devices = change.added.len(), "SNMP device pollers started");
        runner.health().set_devices_total(f.len() as u64);
    }

    // The two writers of the device set (#936). `@desired` carries a fleet's
    // whole set for this host; `@rpc/snmp/targets/set` carries an operator's
    // ad-hoc change. Both go through the same supervisor, and
    // `state/snmp/applied/targets` says which went last.
    //
    // Credentials are resolved HERE, from this host's file config. The wire
    // carries a name; a name this host does not have is refused onto the
    // marker rather than falling back to a default community.
    {
        use zensight_sensor_snmp::config::{devices_from_wire, devices_to_wire};
        let baseline_devices = snmp_config.devices.clone();
        let credentials = snmp_config.credentials.clone();

        let desired_key = {
            use zensight_common::registry::desired;
            desired::key(&desired::Subject::snmp_targets(
                zensight_common::PROFILE.host_id(),
            ))
        };
        let apply_fleet = fleet.clone();
        let apply_baseline = baseline_devices.clone();
        let apply_creds = credentials.clone();
        let apply_health = runner.health();
        let (marker, _reconcile) = zensight_sensor_core::desired::reconcile_topic(
            runner.session().clone(),
            runner.publisher(),
            zensight_sensor_core::desired::DesiredTopic {
                topic: "targets",
                desired_key,
            },
            runner.config().desired.clone(),
            devices_to_wire(&baseline_devices),
            move |wire: zensight_common::targets::SnmpTargets| {
                let fleet = apply_fleet.clone();
                let baseline = apply_baseline.clone();
                let creds = apply_creds.clone();
                let health = apply_health.clone();
                async move {
                    let devices = devices_from_wire(&wire, &baseline, &creds)?;
                    let mut f = fleet.lock().await;
                    let change = f.apply(&devices);
                    if !change.is_noop() {
                        tracing::info!(
                            added = ?change.added, restarted = ?change.restarted,
                            removed = ?change.removed, unchanged = change.unchanged,
                            "device set replaced from @desired"
                        );
                    }
                    health.set_devices_total(f.len() as u64);
                    Ok(())
                }
            },
        );

        let rpc_fleet = fleet.clone();
        let rpc_baseline = baseline_devices.clone();
        let rpc_creds = credentials.clone();
        let rpc_health = runner.health();
        let status_fleet = fleet.clone();
        let ctx = zensight_sensor_core::v1::for_producer("snmp");
        let tasks = zensight_sensor_core::rpc::serve_topic::<
            zensight_common::targets::SnmpTargets,
            _,
            _,
            _,
            _,
        >(
            runner.session().clone(),
            &ctx,
            "targets",
            move |wire: zensight_common::targets::SnmpTargets| {
                let fleet = rpc_fleet.clone();
                let baseline = rpc_baseline.clone();
                let creds = rpc_creds.clone();
                let health = rpc_health.clone();
                let marker = marker.clone();
                async move {
                    let devices = devices_from_wire(&wire, &baseline, &creds)
                        .map_err(zensight_sensor_core::rpc::RpcError::invalid_args)?;
                    let mut f = fleet.lock().await;
                    let change = f.apply(&devices);
                    tracing::info!(
                        added = ?change.added, restarted = ?change.restarted,
                        removed = ?change.removed,
                        "device set replaced over @rpc"
                    );
                    health.set_devices_total(f.len() as u64);
                    marker
                        .publish(
                            zensight_common::desired::AppliedSource::Rpc,
                            &wire,
                            None,
                            None,
                        )
                        .await;
                    Ok(())
                }
            },
            move || {
                let fleet = status_fleet.clone();
                async move {
                    // What is ACTUALLY being polled, from the supervisor —
                    // not what the config file said at startup. Those differ
                    // the moment a set is replaced, and answering with the
                    // second would be answering a question nobody asked.
                    //
                    // `devices_to_wire` drops every secret on the way out, so
                    // this GET returns credential *names* and nothing more.
                    let devices = fleet.lock().await.devices();
                    serde_json::to_vec(&devices_to_wire(&devices)).map_err(|e| {
                        zensight_sensor_core::rpc::RpcError::producer(
                            "snmp",
                            "serialize",
                            e.to_string(),
                        )
                    })
                }
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        for t in tasks {
            runner.spawn(async move {
                let _ = t.await;
            });
        }
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
            zensight_sensor_core::v1::for_producer("snmp").telemetry_prefix(),
            serialization,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_counters(runner.publisher().counters())
        .with_qos(zensight_common::QosClass::HealthLiveness);
        let report_key: String = zensight_sensor_core::v1::for_producer("snmp")
            .const_state_key(&["discovery"])
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

    // Gated PDU outlet control (#956). The procedures are declared
    // UNCONDITIONALLY — `action/capability` so "off" is an answer rather than
    // a silence (#648), `action/set` so the registry does not advertise
    // something nothing serves — and the gate inside them is what refuses.
    //
    // The write credential is substituted into the device set ONCE, here. The
    // read credential is not deprioritised on the SET path; it is not present
    // in the value that path can reach.
    {
        let action_cfg = snmp_config.actions.clone();
        let history = zensight_sensor_snmp::action::History::new(action_cfg.history_capacity);
        let server = zensight_sensor_snmp::action::ActionServer {
            cfg: action_cfg,
            write_devices: zensight_sensor_snmp::action::write_devices(&snmp_config),
            devices: snmp_config.devices.clone(),
        };
        let session = runner.session().clone();
        runner.spawn(zensight_sensor_snmp::action::run(
            session,
            "snmp".to_string(),
            server,
            history,
        ));
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
        trap_receiver.with_thresholds(thresholds.clone());
        if snmp_config.alerts.enabled {
            trap_receiver.with_alerts(alert_reporter.clone());
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
