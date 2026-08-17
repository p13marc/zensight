//! Zenoh sensor for Syslog telemetry.
//!
//! This sensor receives syslog messages via UDP, TCP, and Unix socket,
//! parses them (RFC 3164 and RFC 5424 formats), and publishes
//! them to Zenoh as TelemetryPoints.

mod commands;
mod config;
mod dedup;
mod derived;
mod evidence;
mod file_source;
mod filter;
mod ingest;
#[cfg(feature = "journald")]
mod journald;
mod logbundle;
mod multiline;
mod parser;
mod query;
mod receiver;
mod search;
mod sentinel;
mod store;
mod telemetry_guard;
mod template;
mod tls;

use anyhow::Result;
use commands::{FilterCommand, FilterStatus};
use config::SyslogSensorConfig;
use filter::FilterManager;
use sentinel::LogSentinel;
use std::sync::Arc;
use zensight_common::serialization::encode;
use zensight_common::telemetry::Protocol;
use zensight_sensor_core::{
    AlertReporter, SensorArgs, SensorConfig, SensorRunner, serve_alerts_query,
};

/// Process-wide monotonic sequence that disambiguates per-line log event uids
/// (#104) when multiple lines share a millisecond timestamp.
static LOG_EVENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// While loss persists, re-report every this many monitor windows (#546) so a
/// long partial-loss state keeps surfacing instead of being reported once.
const LOSS_REREPORT_WINDOWS: u32 = 6;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("syslog.json5");

    // Load configuration
    let config = SyslogSensorConfig::load_from_file(&args.config)?;
    let source = config.syslog.resolved_source();
    tracing::info!(summary = %config.startup_summary(), "syslog sensor configuration");

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("logs", source.clone(), config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing. Artifact producers are registered later
    // (`with_artifacts`), once the log store + ring the `logbundle` producer
    // (#555) reads from exist — see below.
    let runner = runner.with_identity();

    // Get session and config for the receiver
    let session = runner.session().clone();
    // Telemetry (log events) goes through declared publishers (declare-on-first-use
    // + cache per key, drop QoS), never a one-shot `session.put`. Shared across the
    // per-task clones so each key is declared once.
    let registry = std::sync::Arc::new(zensight_common::PublisherRegistry::new(session.clone()));
    let syslog_config = runner.config().syslog.clone();

    // Determine serialization format (from config; default CBOR)
    let format = runner.config().serialization;

    // Create filter manager
    let filter_manager = Arc::new(
        FilterManager::new(&syslog_config.filter)
            .map_err(|e| anyhow::anyhow!("Failed to compile filter: {}", e))?,
    );

    // Start syslog listeners (+ journald reader). `journald_stats` carries the
    // reader's throughput/loss accounting when the journald source is enabled;
    // `ingest_stats` carries the network paths' received/parsed/dropped
    // accounting (#106).
    let (mut rx, journald_stats, ingest_stats) = receiver::start_listeners(&syslog_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start syslog listeners: {}", e))?;

    tracing::info!("Syslog listeners started");

    // Process incoming messages
    let include_raw = syslog_config.include_raw_message;
    let enable_dynamic_filters = syslog_config.enable_dynamic_filters;

    // Build status metadata
    let metadata = serde_json::json!({
        "listeners": syslog_config.listeners.iter().map(|l| {
            format!("{}://{}", l.protocol, l.bind)
        }).collect::<Vec<_>>(),
        "include_raw_message": include_raw,
        "filter_enabled": !syslog_config.filter.is_empty(),
        "dynamic_filters_enabled": enable_dynamic_filters,
    });

    // Set up dynamic filter command handling if enabled
    let filter_manager_for_commands = filter_manager.clone();
    let session_for_commands = session.clone();

    let mut runner = runner;

    if enable_dynamic_filters {
        let command_key = commands::command_key("logs");
        let status_key = commands::status_key("logs");

        tracing::info!("Dynamic filters enabled, listening on {}", command_key);

        // Serve filter writes as an @rpc procedure (RFC 05; epic #453).
        let subscriber = session
            .declare_queryable(&command_key)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to declare filter/set queryable: {}", e))?;

        // Declare queryable for filter status
        let filter_manager_for_status = filter_manager_for_commands.clone();
        let queryable = session_for_commands
            .declare_queryable(&status_key)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to declare status queryable: {}", e))?;

        // Spawn command handler task
        let filter_manager_cmd = filter_manager_for_commands.clone();
        runner.spawn(async move {
            loop {
                tokio::select! {
                    Ok(query) = subscriber.recv_async() => {
                        let payload = query
                            .payload()
                            .map(|p| p.to_bytes().to_vec())
                            .unwrap_or_default();
                        match serde_json::from_slice::<FilterCommand>(&payload) {
                            Ok(cmd) => {
                                handle_filter_command(&filter_manager_cmd, cmd).await;
                                if let Err(e) = query.reply(command_key.as_str(), Vec::<u8>::new()).await {
                                    tracing::warn!("Failed to ack filter command: {}", e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse filter command: {}", e);
                                let err = zensight_sensor_core::rpc::RpcError::invalid_args(e.to_string());
                                let _ = query
                                    .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                                    .await;
                            }
                        }
                    }
                    Ok(query) = queryable.recv_async() => {
                        let status = build_filter_status(&filter_manager_for_status).await;
                        match serde_json::to_vec(&status) {
                            Ok(payload) => {
                                if let Err(e) = query.reply(status_key.as_str(), payload).await {
                                    tracing::warn!("Failed to reply to status query: {}", e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to serialize status: {}", e);
                            }
                        }
                    }
                }
            }
        });
    }

    // Shared alert reporter for all sensor-emitted alerts: journald known-events
    // (#61) and per-unit error budgets (#105). One reporter per protocol — the
    // two alert families are namespaced by `rule` and reconcile independently —
    // so `serve_alerts_query` is declared exactly once.
    let journald_events_on =
        matches!(&syslog_config.journald, Some(j) if j.enabled && j.detect_events);
    let budget_alerts_on = syslog_config.derived && syslog_config.error_budget.enabled;
    // Log sentinel (#543): on when the operator declared rules, or the built-in
    // known-events are active (they ride the journald `detect_events` gate).
    let sentinel_on = journald_events_on || !syslog_config.sentinel.rules.is_empty();
    let alert_reporter: Option<Arc<AlertReporter>> =
        if journald_events_on || budget_alerts_on || sentinel_on {
            let reporter = AlertReporter::new(runner.publisher(), Protocol::Logs, format);
            // Stamp alerts with the host identity envelope when available.
            let reporter = match runner.identity() {
                Some(id) => reporter.with_identity(id),
                None => reporter,
            };
            let reporter = Arc::new(reporter);
            // Seed late-joining consumers (e.g. the GUI) with the firing set.
            runner.spawn(serve_alerts_query(reporter.clone()));
            Some(reporter)
        } else {
            None
        };
    if syslog_config.error_budget.enabled && !syslog_config.derived {
        tracing::warn!(
            "error_budget enabled but derived telemetry is off; SLO alerting needs \
             the derived aggregator — skipping budget alerts"
        );
    }
    // Log sentinel (#543): declarative pattern→alert rules evaluated per intake
    // line, folding the journald known-events (#61) in as built-in rules. Runs
    // whenever a reporter exists and there's something to evaluate.
    let log_sentinel: Option<Arc<LogSentinel>> = alert_reporter
        .as_ref()
        .filter(|_| sentinel_on)
        .map(|reporter| {
            let mut rules_cfg = syslog_config.sentinel.clone();
            // Built-in known-events ride the journald `detect_events` gate: keep
            // them off when journald detection is off, preserving the old opt-out.
            rules_cfg.include_builtins = rules_cfg.include_builtins && journald_events_on;
            let sentinel = Arc::new(LogSentinel::new(source.clone(), rules_cfg));
            runner.spawn(sentinel.clone().run_reconcile_loop(reporter.clone()));
            runner.spawn(sentinel::serve_rules(
                session.clone(),
                "logs".to_string(),
                sentinel.handle(),
            ));
            // #543 deprecation: the journald known-event severity override is
            // gone; point operators at the sentinel-rule replacement.
            if let Some(j) = &syslog_config.journald
                && !j.event_severity.is_empty()
            {
                tracing::warn!(
                    "journald.event_severity is deprecated and ignored (#543); \
                     override a known-event by adding a sentinel rule with the \
                     same id (coredump/unit-failed/oomd-kill/kernel-oom)"
                );
            }
            let rule_count = syslog_config.sentinel.rules.len();
            tracing::info!(
                rules = rule_count,
                builtins = journald_events_on,
                "log sentinel enabled"
            );
            sentinel
        });

    // journald robustness monitor (#62): periodically snapshot the reader's
    // read/published/dropped/sampled counters; on sustained loss raise an
    // ErrorReport so the Sensors view reflects "we are dropping your logs" —
    // healthy ≠ "process up". Only runs when the journald source is enabled.
    if let Some(stats) = journald_stats.clone() {
        let health = runner.health();
        let drop_alert_ratio = syslog_config
            .journald
            .as_ref()
            .map(|j| j.drop_alert_ratio)
            .unwrap_or(0.01);
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            let mut prev = stats.snapshot();
            // Level-triggered (#546): re-report periodically while loss persists,
            // and emit an explicit recovery report on clearing.
            let mut alerting = false;
            let mut windows_lossy: u32 = 0;
            loop {
                tick.tick().await;
                let cur = stats.snapshot();
                let loss = cur.loss_ratio_since(&prev);
                let dropped = cur.dropped.saturating_sub(prev.dropped);
                let sampled = cur.sampled_out.saturating_sub(prev.sampled_out);
                if loss > drop_alert_ratio && (dropped + sampled) > 0 {
                    windows_lossy += 1;
                    // Report on entering, then every LOSS_REREPORT_WINDOWS while
                    // still lossy — so a long partial-loss state keeps surfacing.
                    if !alerting || windows_lossy.is_multiple_of(LOSS_REREPORT_WINDOWS) {
                        alerting = true;
                        let report = zensight_sensor_core::ErrorReport::new(
                            zensight_sensor_core::ErrorType::Other,
                            format!(
                                "journald dropping logs: {:.1}% loss over last window \
                                 ({dropped} dropped, {sampled} sampled-out). Raise the \
                                 channel/rate budget or narrow server-side matches.",
                                loss * 100.0
                            ),
                        );
                        if let Err(e) = health.publish_error(&report).await {
                            tracing::warn!(error = %e, "failed to publish journald drop ErrorReport");
                        }
                        tracing::warn!(
                            loss_pct = loss * 100.0,
                            dropped,
                            sampled,
                            "journald: sustained log loss"
                        );
                    }
                } else if alerting {
                    alerting = false;
                    windows_lossy = 0;
                    let report = zensight_sensor_core::ErrorReport::new(
                        zensight_sensor_core::ErrorType::Other,
                        "journald log loss recovered — drop rate back under threshold",
                    );
                    if let Err(e) = health.publish_error(&report).await {
                        tracing::warn!(error = %e, "failed to publish journald recovery report");
                    }
                    tracing::info!("journald: log loss recovered");
                }
                prev = cur;
            }
        });
    }

    // Network-ingest robustness monitor + telemetry (#106): bring the UDP/TCP/
    // Unix paths to journald parity. On a tick, publish the
    // `logs/ingest/{received,parsed,parse_failed,dropped}_total` counters and,
    // on sustained loss, raise an edge-triggered `ErrorReport` so the Sensors
    // view reflects "we are dropping your logs" — UDP drops + parse failures are
    // no longer silent. Only runs when at least one network listener exists
    // (journald has its own monitor above).
    if !syslog_config.listeners.is_empty() {
        let stats = ingest_stats.clone();
        let health = runner.health();
        let registry_tick = registry.clone();
        let v1_prefix_tick =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "logs")
                .telemetry_prefix();
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        let drop_alert_ratio = syslog_config.ingest.drop_alert_ratio;
        let source = source.clone();
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            let mut prev = stats.snapshot();
            let mut alerting = false;
            let mut windows_lossy: u32 = 0;
            loop {
                tick.tick().await;
                let cur = stats.snapshot();
                let loss = cur.loss_ratio_since(&prev);
                let dropped = cur.dropped.saturating_sub(prev.dropped);

                // Publish the ingest counters + a windowed `dropped_ratio` gauge
                // (#546) so dashboards can alert on sustained loss directly.
                let mut points = cur.to_points(&source);
                points.push(telemetry_guard::checked_point(
                    &source,
                    "ingest/dropped_ratio",
                    zensight_common::telemetry::TelemetryValue::Gauge(loss),
                ));
                for point in points {
                    let key = format!("{}/{}", v1_prefix_tick, point.metric);
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = registry_tick
                                .put(&key, payload, zensight_common::QosClass::Telemetry)
                                .await
                            {
                                tracing::warn!(error = %e, key, "failed to publish ingest metric");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to encode ingest metric"),
                    }
                }

                // Sustained-loss health alert — level-triggered (#546): report on
                // entering, re-report periodically while lossy, recovery on clear.
                if loss > drop_alert_ratio && dropped > 0 {
                    windows_lossy += 1;
                    if !alerting || windows_lossy.is_multiple_of(LOSS_REREPORT_WINDOWS) {
                        alerting = true;
                        let report = zensight_sensor_core::ErrorReport::new(
                            zensight_sensor_core::ErrorType::Other,
                            format!(
                                "network ingest dropping logs: {:.1}% loss over last window \
                                 ({dropped} dropped). Raise the channel/rate budget or reduce \
                                 the inbound rate.",
                                loss * 100.0
                            ),
                        );
                        if let Err(e) = health.publish_error(&report).await {
                            tracing::warn!(error = %e, "failed to publish ingest drop ErrorReport");
                        }
                        tracing::warn!(
                            loss_pct = loss * 100.0,
                            dropped,
                            "network ingest: sustained log loss"
                        );
                    }
                } else if alerting {
                    alerting = false;
                    windows_lossy = 0;
                    let report = zensight_sensor_core::ErrorReport::new(
                        zensight_sensor_core::ErrorType::Other,
                        "network ingest log loss recovered — drop rate back under threshold",
                    );
                    if let Err(e) = health.publish_error(&report).await {
                        tracing::warn!(error = %e, "failed to publish ingest recovery report");
                    }
                    tracing::info!("network ingest: log loss recovered");
                }
                prev = cur;
            }
        });
    }

    // Derived rollup telemetry (#63): aggregate the log stream into per-severity
    // / per-unit / error rollups, emitted on a tick alongside the per-message
    // points. The aggregator observes each published message; the tick task
    // snapshots it (+ journald throughput) to telemetry.
    let aggregator = syslog_config.derived.then(|| {
        // Resolve the per-unit error-budget / SLO thresholds (#105). Alerting is
        // gated on a reporter being present (events + budget share one).
        let eb = &syslog_config.error_budget;
        let budget = derived::BudgetParams {
            enabled: budget_alerts_on,
            target_ratio: eb.target_ratio,
            burn_rate: eb.burn_rate,
            burn_windows: eb.burn_windows,
            min_messages: eb.min_messages,
        };
        Arc::new(derived::LogAggregator::new(syslog_config.top_units).with_budget(budget))
    });
    if let Some(agg) = aggregator.clone() {
        let registry_tick = registry.clone();
        let v1_prefix_tick =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "logs")
                .telemetry_prefix();
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        let stats_tick = journald_stats.clone();
        let budget_reporter = budget_alerts_on.then(|| alert_reporter.clone()).flatten();
        // Local host identifies this sensor's rollups (network syslog spans many
        // hosts; journald is local — a single sensor-wide source keeps the
        // derived series cardinality bounded).
        let source = source.clone();
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                let snapshot = stats_tick.as_ref().map(|s| s.snapshot());
                let mut points = agg.emit(&source, snapshot);

                // SLO / error-budget layer (#105): error_ratio + burn_rate gauges
                // for the same bounded unit set, plus burn alerts when enabled.
                let budget = agg.tick_budgets(&source);
                points.extend(budget.points);
                if let Some(reporter) = &budget_reporter {
                    for alert in budget.firing {
                        let key = alert.alert_key();
                        if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                            tracing::warn!(error = %e, alert = %key, "failed to publish budget alert");
                        }
                    }
                    if let Err(e) = reporter
                        .reconcile(derived::BUDGET_RULE, &budget.firing_keys)
                        .await
                    {
                        tracing::warn!(error = %e, "budget alert reconcile failed");
                    }
                }

                for point in points {
                    let key = format!("{}/{}", v1_prefix_tick, point.metric);
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = registry_tick.put(&key, payload, zensight_common::QosClass::Telemetry).await {
                                tracing::warn!(error = %e, key, "failed to publish derived metric");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to encode derived metric"),
                    }
                }
            }
        });
        tracing::info!(interval_secs, "derived rollup telemetry enabled");
    }

    // Streaming log-template mining (#102): mask + cluster each line into a
    // stable template, attach `template_id`/`template` labels to the per-line
    // points, and emit bounded `by_template/*` series on a tick. Additive
    // and independent of the `derived` toggle.
    let template_agg = syslog_config.templating.enabled.then(|| {
        let t = &syslog_config.templating;
        let params = template::DrainParams {
            depth: t.depth,
            sim_threshold: t.sim_threshold,
            max_children: t.max_children,
            max_clusters: t.max_clusters,
        };
        Arc::new(template::TemplateAggregator::new(params, t.top_templates))
    });
    if let Some(tagg) = template_agg.clone() {
        let registry_tick = registry.clone();
        let v1_prefix_tick =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "logs")
                .telemetry_prefix();
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        let source = source.clone();
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                for point in tagg.emit(&source) {
                    let key = format!("{}/{}", v1_prefix_tick, point.metric);
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = registry_tick.put(&key, payload, zensight_common::QosClass::Telemetry).await {
                                tracing::warn!(error = %e, key, "failed to publish template metric");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to encode template metric"),
                    }
                }
            }
        });
        tracing::info!("log-template mining enabled");
    }

    // Durable log store (#544): a disk-backed history behind the hot ring, so
    // `@rpc/logs/events` can serve days back across restarts. Opt-in. The intake
    // loop only pushes to `store_tx`; a dedicated writer task batches to disk so
    // a slow disk never adds latency to ingestion.
    let store_cfg = &syslog_config.store;
    let (log_store, store_tx, store_counters) = if store_cfg.enabled {
        match store::resolve_store_path(store_cfg.path.as_deref()) {
            Some(path) => match store::LogStore::open(&path, store_cfg.cache_bytes) {
                Ok(s) => {
                    let store = Arc::new(s);
                    let counters = Arc::new(store::StoreCounters::default());
                    let (tx, rx) = tokio::sync::mpsc::channel::<zensight_common::LogRecord>(
                        store_cfg.queue_capacity.max(1),
                    );
                    // Writer task: batch-append off the hot path.
                    runner.spawn(store_writer_loop(
                        store.clone(),
                        rx,
                        counters.clone(),
                        store_cfg.batch_size.max(1),
                        std::time::Duration::from_secs(store_cfg.flush_interval_secs.max(1)),
                    ));
                    // Prune + health task.
                    runner.spawn(store_maintenance_loop(
                        store.clone(),
                        counters.clone(),
                        registry.clone(),
                        source.clone(),
                        format,
                        store_cfg.clone(),
                    ));
                    tracing::info!(path = %path.display(), "durable log store enabled");
                    (Some(store), Some(tx), Some(counters))
                }
                Err(e) => {
                    tracing::error!(error = %e, path = %path.display(), "failed to open log store; disabling");
                    (None, None, None)
                }
            },
            None => {
                tracing::warn!("log store enabled but no state dir resolved; disabling");
                (None, None, None)
            }
        }
    } else {
        (None, None, None)
    };

    // Per-line event ring + on-demand query channel (#358): log lines are
    // served from `@rpc/logs/events`, never streamed on the telemetry bus. When
    // the durable store is on, historical (`from`/`to`/`after_uid`) queries are
    // answered from it; recent ones from the ring.
    let (event_ring, event_ring_capacity) = query::new_ring(syslog_config.events_ring_capacity);
    runner.spawn(query::run_events(
        session.clone(),
        "logs".to_string(),
        event_ring.clone(),
        log_store.clone(),
    ));

    // Register artifact producers now that the store + ring exist (#555). The
    // `logbundle` producer reads from both; report/snapshot need only the runner.
    {
        let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
            "logs",
            source.clone(),
            runner.config().clone(),
            runner.health(),
        ));
        let artifacts = runner.config().artifact_limits();
        runner = runner.with_artifacts(vec![
            std::sync::Arc::new(zensight_sensor_core::ReportProducer::new(
                report_source,
                &artifacts.report,
            )) as std::sync::Arc<dyn zensight_sensor_core::ArtifactProducer>,
            std::sync::Arc::new(zensight_sensor_core::SnapshotProducer::new(
                &artifacts.snapshot,
            )),
            std::sync::Arc::new(logbundle::LogBundleProducer::new(
                &syslog_config.logbundle,
                source.clone(),
                log_store.clone(),
                event_ring.clone(),
            )),
        ]);
    }

    // Observer evidence (#552): track remote senders and publish a HostEvidence
    // claim per device on `evidence/device/*` (Evidence QoS, cache-1 for late
    // correlator joins) so they reach the entity catalog. No-op without network
    // sources (only Network peers are "observed devices").
    let has_network = syslog_config
        .listeners
        .iter()
        .any(|l| !matches!(l.protocol, config::ListenerProtocol::Unix));
    let evidence_tracker = (syslog_config.evidence.enabled && has_network).then(|| {
        let tracker = Arc::new(evidence::EvidenceTracker::new(
            syslog_config.evidence.clone(),
        ));
        let ev_registry = Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::V1Context::for_producer(
                    &zensight_common::PROFILE,
                    "logs",
                )
                .telemetry_prefix(),
                format,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        );
        let refresh = std::time::Duration::from_secs(syslog_config.evidence.refresh_secs.max(1));
        let t = tracker.clone();
        runner.spawn(async move {
            let mut tick = tokio::time::interval(refresh);
            loop {
                tick.tick().await;
                let now_ms = chrono::Utc::now().timestamp_millis();
                let t2 = t.clone();
                // Reverse-DNS (if enabled) blocks → build claims off the runtime.
                let claims =
                    match tokio::task::spawn_blocking(move || t2.build_claims(now_ms)).await {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                for (device, claim) in claims {
                    let key: String = zensight_common::registry::logs::key(
                        &zensight_common::PROFILE.local_origin(),
                        &zensight_common::registry::logs::Subject::evidence_device(&device),
                    )
                    .into();
                    if let Err(e) = ev_registry.publish_serializable(&key, &claim).await {
                        tracing::warn!(device = %device, error = %e, "evidence publish failed");
                    }
                }
            }
        });
        tracing::info!("observer evidence enabled");
        tracker
    });

    // Spawn the message processing task
    let publish_health = runner.health();
    let evidence_loop = evidence_tracker.clone();
    let aggregator_loop = aggregator.clone();
    let template_loop = template_agg.clone();
    let sentinel_loop = log_sentinel.clone();
    let sentinel_reporter = log_sentinel
        .is_some()
        .then(|| alert_reporter.clone())
        .flatten();
    let store_tx_loop = store_tx.clone();
    let store_counters_loop = store_counters.clone();
    // Repeat collapse (#546): fold consecutive identical lines into one record +
    // `repeat_count`. Opt-in; `None` = pass every line straight through.
    let mut collapser = syslog_config.ingest.collapse_repeats.then(|| {
        dedup::RepeatCollapser::new(std::time::Duration::from_millis(
            syslog_config.ingest.collapse_window_ms.max(1),
        ))
    });
    runner.spawn(async move {
        // Flush timer for the collapser's trailing run (a run that ends without a
        // following different line). Cheap fixed tick; a no-op when collapse is off.
        let mut flush_tick = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                Some(received) = rx.recv() => {
                    // Observer evidence (#552): record the sender before filtering
                    // (a device whose logs are filtered still exists). Pure map
                    // update — no I/O on the hot path.
                    if let Some(ev) = &evidence_loop {
                        ev.observe(&received, chrono::Utc::now().timestamp_millis());
                    }

                    // Sentinel rules (#543) run before filtering so a coredump,
                    // unit failure, or operator-declared pattern still alerts even
                    // when the line is filtered from the telemetry stream.
                    if let (Some(s), Some(r)) = (&sentinel_loop, &sentinel_reporter) {
                        s.observe(r, &received.message, std::time::Instant::now()).await;
                    }

                    // Apply filter
                    if !filter_manager.matches(&received.message, &received.resolved_hostname).await {
                        tracing::trace!(
                            "Filtered message from {} [{}]",
                            received.resolved_hostname,
                            received.message.severity.as_str()
                        );
                        continue;
                    }

                    // Feed derived rollups (#63) — counts every post-filter line
                    // (before collapse, so totals stay honest even when identical
                    // lines fold into one ring record).
                    if let Some(agg) = &aggregator_loop {
                        agg.observe(&received.message);
                    }

                    // Repeat collapse: a folded line is suppressed here and emitted
                    // once its run closes (a different line, or the flush tick).
                    let to_emit = match &mut collapser {
                        Some(c) => c.observe(received, std::time::Instant::now()),
                        None => Some((received, 1)),
                    };
                    if let Some((record, count)) = to_emit {
                        emit_line(
                            &record, count, include_raw, &template_loop,
                            &event_ring, event_ring_capacity, &publish_health,
                            &store_tx_loop, &store_counters_loop,
                        ).await;
                    }
                }
                _ = flush_tick.tick() => {
                    if let Some(c) = &mut collapser
                        && let Some((record, count)) = c.flush_due(std::time::Instant::now())
                    {
                        emit_line(
                            &record, count, include_raw, &template_loop,
                            &event_ring, event_ring_capacity, &publish_health,
                            &store_tx_loop, &store_counters_loop,
                        ).await;
                    }
                }
                else => break,
            }
        }
    });

    // Run until Ctrl+C (handles shutdown gracefully)
    runner
        .run_with_metadata(Some(metadata))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
}

/// Emit one processed log line to the `@rpc/logs/events` ring (#358): build the
/// per-line uid + telemetry point, run template mining on it, and push
/// it. Shared by the live-recv and repeat-collapse-flush paths (#546) so both
/// treat a line identically; `repeat_count > 1` folds a collapsed run and is
/// surfaced as a `repeat_count` label.
#[allow(clippy::too_many_arguments)]
async fn emit_line(
    received: &receiver::ReceivedMessage,
    repeat_count: u64,
    include_raw: bool,
    template: &Option<Arc<template::TemplateAggregator>>,
    event_ring: &query::EventRing,
    event_ring_capacity: usize,
    health: &Arc<zensight_sensor_core::SensorHealth>,
    store_tx: &Option<tokio::sync::mpsc::Sender<zensight_common::LogRecord>>,
    store_counters: &Option<Arc<store::StoreCounters>>,
) {
    use std::sync::atomic::Ordering;

    let ts_ms = received
        .message
        .timestamp
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let seq = LOG_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let uid = receiver::make_log_uid(ts_ms, seq);

    let mut point = receiver::to_telemetry_point(received, include_raw, &uid);
    if repeat_count > 1 {
        point
            .labels
            .insert("repeat_count".to_string(), repeat_count.to_string());
    }

    // Log-template mining (#102) on the representative line.
    if let Some(tagg) = template {
        let is_error = (received.message.severity as u8) <= (parser::Severity::Error as u8);
        if let Some(mined) = tagg.observe(&received.message.message, is_error) {
            point.labels.insert("template_id".to_string(), mined.id);
            point.labels.insert("template".to_string(), mined.template);
        }
    }

    if let Some(record) = zensight_common::LogRecord::from_point(&point) {
        // Durable store (#544): hand a copy to the writer task off the hot path.
        // A full writer queue drops + counts rather than back-pressuring intake.
        if let Some(tx) = store_tx
            && tx.try_send(record.clone()).is_err()
            && let Some(c) = store_counters
        {
            store::StoreCounters::inc(&c.dropped);
        }
        query::push(event_ring, event_ring_capacity, record);
        health.record_metrics_published(1);
    }
}

/// Durable-store writer task (#544): drain the intake channel, batch, and write
/// on a blocking thread so disk I/O never touches the hot loop.
async fn store_writer_loop(
    store: Arc<store::LogStore>,
    mut rx: tokio::sync::mpsc::Receiver<zensight_common::LogRecord>,
    counters: Arc<store::StoreCounters>,
    batch_size: usize,
    flush_interval: std::time::Duration,
) {
    let mut batch: Vec<zensight_common::LogRecord> = Vec::with_capacity(batch_size);
    let mut tick = tokio::time::interval(flush_interval);
    loop {
        tokio::select! {
            got = rx.recv() => match got {
                Some(rec) => {
                    batch.push(rec);
                    if batch.len() >= batch_size {
                        flush_store_batch(&store, &mut batch, &counters).await;
                    }
                }
                None => {
                    // Channel closed (shutdown): final flush and stop.
                    flush_store_batch(&store, &mut batch, &counters).await;
                    break;
                }
            },
            _ = tick.tick() => flush_store_batch(&store, &mut batch, &counters).await,
        }
    }
}

/// Write and clear `batch` (no-op if empty), updating counters.
async fn flush_store_batch(
    store: &Arc<store::LogStore>,
    batch: &mut Vec<zensight_common::LogRecord>,
    counters: &Arc<store::StoreCounters>,
) {
    if batch.is_empty() {
        return;
    }
    let recs = std::mem::take(batch);
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.write_batch(&recs)).await {
        Ok(Ok(n)) => store::StoreCounters::add(&counters.written, n as u64),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "log store: batch write failed");
            store::StoreCounters::inc(&counters.errors);
        }
        Err(e) => {
            tracing::warn!(error = %e, "log store: writer task panicked");
            store::StoreCounters::inc(&counters.errors);
        }
    }
}

/// Durable-store maintenance task (#544): periodic prune + store health gauges
/// (`store/records`, `store/oldest_age_secs`, `store/write_drops_total`).
async fn store_maintenance_loop(
    store: Arc<store::LogStore>,
    counters: Arc<store::StoreCounters>,
    registry: Arc<zensight_common::PublisherRegistry>,
    source: String,
    format: zensight_common::serialization::Format,
    cfg: config::LogStoreConfig,
) {
    use std::sync::atomic::Ordering;
    let prefix =
        zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "logs")
            .telemetry_prefix();
    let max_age_ms = (cfg.max_age_days as i64).saturating_mul(86_400_000);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(
        cfg.prune_interval_secs.max(1),
    ));
    loop {
        tick.tick().await;
        let now_ms = chrono::Utc::now().timestamp_millis();

        let s = store.clone();
        let keep = cfg.max_records;
        match tokio::task::spawn_blocking(move || s.prune(now_ms, max_age_ms, keep)).await {
            Ok(Ok(n)) if n > 0 => tracing::info!(pruned = n, "log store: pruned old records"),
            Ok(Err(e)) => tracing::warn!(error = %e, "log store: prune failed"),
            _ => {}
        }

        let s = store.clone();
        let stats = match tokio::task::spawn_blocking(move || s.stats()).await {
            Ok(Ok(st)) => st,
            _ => continue,
        };
        let oldest_age = stats
            .oldest_ts
            .map(|t| (now_ms - t).max(0) / 1000)
            .unwrap_or(0);
        let points = [
            telemetry_guard::checked_point(
                &source,
                "store/records",
                zensight_common::telemetry::TelemetryValue::Gauge(stats.records as f64),
            ),
            telemetry_guard::checked_point(
                &source,
                "store/oldest_age_secs",
                zensight_common::telemetry::TelemetryValue::Gauge(oldest_age as f64),
            ),
            telemetry_guard::checked_point(
                &source,
                "store/write_drops_total",
                zensight_common::telemetry::TelemetryValue::Counter(
                    counters.dropped.load(Ordering::Relaxed),
                ),
            ),
        ];
        for point in points {
            let key = format!("{}/{}", prefix, point.metric);
            match encode(&point, format) {
                Ok(payload) => {
                    if let Err(e) = registry
                        .put(&key, payload, zensight_common::QosClass::Telemetry)
                        .await
                    {
                        tracing::warn!(error = %e, key, "failed to publish store metric");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to encode store metric"),
            }
        }
    }
}

/// Handle a filter command.
async fn handle_filter_command(filter_manager: &FilterManager, cmd: FilterCommand) {
    match cmd {
        FilterCommand::AddFilter { id, filter } => {
            let filter_id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            match filter_manager.add_filter(filter_id.clone(), &filter).await {
                Ok(()) => {
                    tracing::info!("Added dynamic filter: {}", filter_id);
                }
                Err(e) => {
                    tracing::warn!("Failed to add filter {}: {}", filter_id, e);
                }
            }
        }
        FilterCommand::RemoveFilter { id } => {
            if filter_manager.remove_filter(&id).await {
                tracing::info!("Removed dynamic filter: {}", id);
            } else {
                tracing::warn!("Filter not found: {}", id);
            }
        }
        FilterCommand::ClearFilters => {
            filter_manager.clear_filters().await;
            tracing::info!("Cleared all dynamic filters");
        }
        FilterCommand::GetStatus => {
            // Status is handled via queryable, this command is a no-op via pub/sub
            tracing::debug!("GetStatus command received (use query for response)");
        }
    }
}

/// Build filter status response.
async fn build_filter_status(filter_manager: &FilterManager) -> FilterStatus {
    FilterStatus {
        base_filter: filter_manager.base_config().clone(),
        dynamic_filters: filter_manager.dynamic_filter_info().await,
        stats: filter_manager.stats(),
    }
}
