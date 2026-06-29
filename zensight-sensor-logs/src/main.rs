//! Zenoh sensor for Syslog telemetry.
//!
//! This sensor receives syslog messages via UDP, TCP, and Unix socket,
//! parses them (RFC 3164 and RFC 5424 formats), and publishes
//! them to Zenoh as TelemetryPoints.

mod commands;
mod config;
mod derived;
mod events;
mod filter;
mod ingest;
#[cfg(feature = "journald")]
mod journald;
mod multiline;
mod novelty;
mod parser;
mod receiver;
mod template;

use anyhow::Result;
use commands::{FilterCommand, FilterStatus};
use config::SyslogSensorConfig;
use events::EventDetector;
use filter::FilterManager;
use std::sync::Arc;
use zensight_common::serialization::{Format, encode};
use zensight_common::telemetry::Protocol;
use zensight_sensor_core::{AlertReporter, SensorArgs, SensorRunner, serve_alerts_query};

/// Process-wide monotonic sequence that disambiguates per-line log event uids
/// (#104) when multiple lines share a millisecond timestamp.
static LOG_EVENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = SensorArgs::parse_with_default("syslog.json5");

    // Load configuration
    let config = SyslogSensorConfig::load_from_file(&args.config)?;

    // Create the sensor runner
    let runner = SensorRunner::new_with_args("logs", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

    // On-demand debug-report (`@/report`): bundle redacted config + health +
    // counters. No-op unless `report.enabled` is set in the config.
    let report_host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    let report_source = std::sync::Arc::new(zensight_sensor_core::SimpleBundleSource::new(
        "logs",
        report_host.clone(),
        runner.config().clone(),
        runner.health(),
    ));
    // Tier-2 directory snapshots (`@/snapshot`). No-op unless `snapshot.enabled`.
    let runner = runner.with_report(report_source).with_snapshot(report_host);

    // Get session and config for the receiver
    let session = runner.session().clone();
    let syslog_config = runner.config().syslog.clone();

    // Determine serialization format (default to JSON)
    let format = Format::Json;

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

    tracing::info!(
        "Syslog listeners started, publishing to prefix: {}",
        syslog_config.key_prefix
    );

    // Process incoming messages
    let key_prefix = syslog_config.key_prefix.clone();
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
    let _key_prefix_for_commands = key_prefix.clone();

    let mut runner = runner;

    if enable_dynamic_filters {
        let command_key = commands::command_key(&key_prefix);
        let status_key = commands::status_key(&key_prefix);

        tracing::info!("Dynamic filters enabled, listening on {}", command_key);

        // Subscribe to filter commands
        let subscriber = session
            .declare_subscriber(&command_key)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to commands: {}", e))?;

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
                    Ok(sample) = subscriber.recv_async() => {
                        let payload = sample.payload().to_bytes();
                        match serde_json::from_slice::<FilterCommand>(&payload) {
                            Ok(cmd) => {
                                handle_filter_command(&filter_manager_cmd, cmd).await;
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse filter command: {}", e);
                            }
                        }
                    }
                    Ok(query) = queryable.recv_async() => {
                        let status = build_filter_status(&filter_manager_for_status).await;
                        match serde_json::to_vec(&status) {
                            Ok(payload) => {
                                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
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
    // Novelty / rate-spike (#103) needs the template miner to feed it `template_id`s.
    let novelty_alerts_on = syslog_config.templating.enabled && syslog_config.novelty.enabled;
    let alert_reporter: Option<Arc<AlertReporter>> =
        if journald_events_on || budget_alerts_on || novelty_alerts_on {
            let reporter = Arc::new(AlertReporter::new(
                runner.publisher(),
                Protocol::Logs,
                format,
            ));
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
    if syslog_config.novelty.enabled && !syslog_config.templating.enabled {
        tracing::warn!(
            "novelty enabled but templating is off; novelty/rate-spike detection needs \
             the template miner — skipping novelty alerts"
        );
    }

    // Known systemd-event detection → alerts (#61). Only when journald is the
    // source and detection is enabled; the alert path is otherwise untouched.
    let event_detector: Option<Arc<EventDetector>> =
        match (&syslog_config.journald, &alert_reporter) {
            (Some(j), Some(reporter)) if j.enabled && j.detect_events => {
                let detector = Arc::new(EventDetector::new(
                    reporter.clone(),
                    j.event_dedup_secs,
                    &j.event_severity,
                ));
                // Auto-resolve fired events after their dedup window.
                runner.spawn(detector.clone().run_reconcile_loop());
                tracing::info!("journald known-event detection enabled");
                Some(detector)
            }
            _ => None,
        };

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
            let mut alerting = false;
            loop {
                tick.tick().await;
                let cur = stats.snapshot();
                let loss = cur.loss_ratio_since(&prev);
                let dropped = cur.dropped.saturating_sub(prev.dropped);
                let sampled = cur.sampled_out.saturating_sub(prev.sampled_out);
                if loss > drop_alert_ratio && (dropped + sampled) > 0 {
                    // Edge-triggered: report once on entering the lossy state.
                    if !alerting {
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
        let session_tick = session.clone();
        let key_prefix_tick = key_prefix.clone();
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        let drop_alert_ratio = syslog_config.ingest.drop_alert_ratio;
        let source = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "localhost".to_string());
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            let mut prev = stats.snapshot();
            let mut alerting = false;
            loop {
                tick.tick().await;
                let cur = stats.snapshot();

                // Publish the ingest counters as telemetry.
                for point in cur.to_points(&source) {
                    let key = format!("{}/{}/{}", key_prefix_tick, point.source, point.metric);
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = session_tick.put(&key, payload).await {
                                tracing::warn!(error = %e, key, "failed to publish ingest metric");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to encode ingest metric"),
                    }
                }

                // Sustained-loss health alert (edge-triggered, mirrors journald).
                let loss = cur.loss_ratio_since(&prev);
                let dropped = cur.dropped.saturating_sub(prev.dropped);
                if loss > drop_alert_ratio && dropped > 0 {
                    if !alerting {
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
        let session_tick = session.clone();
        let key_prefix_tick = key_prefix.clone();
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        let stats_tick = journald_stats.clone();
        let budget_reporter = budget_alerts_on.then(|| alert_reporter.clone()).flatten();
        // Local host identifies this sensor's rollups (network syslog spans many
        // hosts; journald is local — a single sensor-wide source keeps the
        // derived series cardinality bounded).
        let source = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "localhost".to_string());
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
                    let key = format!("{}/{}/{}", key_prefix_tick, point.source, point.metric);
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = session_tick.put(&key, payload).await {
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
    // points, and emit bounded `logs/by_template/*` series on a tick. Additive
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
        let session_tick = session.clone();
        let key_prefix_tick = key_prefix.clone();
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        let source = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "localhost".to_string());
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                for point in tagg.emit(&source) {
                    let key = format!("{}/{}/{}", key_prefix_tick, point.source, point.metric);
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = session_tick.put(&key, payload).await {
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

    // Novelty / "what's new" detection (#103): on top of the template miner,
    // raise a `log-novelty` anomaly the first time a template shape is seen after
    // warm-up, and a `log-rate-spike` anomaly when a known template's rate jumps
    // N× over its EWMA baseline. Reuses the shared `AlertReporter` (namespaced by
    // `rule`). Gated on templating being on (it needs the `template_id`s).
    let novelty: Option<Arc<novelty::NoveltyTracker>> = match (&template_agg, &alert_reporter) {
        (Some(_), Some(_)) if novelty_alerts_on => {
            let n = &syslog_config.novelty;
            use std::time::Duration;
            let source = hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "localhost".to_string());
            let params = novelty::NoveltyParams {
                warm_up: Duration::from_secs(n.warm_up_secs),
                dedup: Duration::from_secs(n.novelty_dedup_secs.max(1)),
                rate_spike_multiplier: n.rate_spike_multiplier,
                min_spike_count: n.min_spike_count,
                ewma_alpha: n.ewma_alpha,
                max_templates: n.max_templates,
            };
            Some(Arc::new(novelty::NoveltyTracker::new(
                params,
                source,
                std::time::Instant::now(),
            )))
        }
        _ => None,
    };
    if let (Some(tracker), Some(reporter)) = (novelty.clone(), alert_reporter.clone()) {
        let interval_secs = syslog_config.derived_interval_secs.max(1);
        runner.spawn(async move {
            use std::time::Duration;
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                let out = tracker.tick(std::time::Instant::now());
                // Rate-spikes fire/refresh here; novelty point-events fire in the
                // publish loop. Both rules reconcile against their live key sets so
                // anything no longer present auto-resolves.
                for alert in out.firing {
                    let key = alert.alert_key();
                    if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                        tracing::warn!(error = %e, alert = %key, "failed to publish rate-spike alert");
                    }
                }
                if let Err(e) = reporter
                    .reconcile(novelty::NOVELTY_RULE, &out.novelty_keys)
                    .await
                {
                    tracing::warn!(error = %e, "novelty alert reconcile failed");
                }
                if let Err(e) = reporter
                    .reconcile(novelty::SPIKE_RULE, &out.spike_keys)
                    .await
                {
                    tracing::warn!(error = %e, "rate-spike alert reconcile failed");
                }
            }
        });
        tracing::info!("log novelty / rate-spike detection enabled");
    }

    // Spawn the message processing task
    let session_clone = session.clone();
    let publish_health = runner.health();
    let aggregator_loop = aggregator.clone();
    let template_loop = template_agg.clone();
    let novelty_loop = novelty.clone();
    let novelty_reporter = novelty.is_some().then(|| alert_reporter.clone()).flatten();
    runner.spawn(async move {
        loop {
            tokio::select! {
                Some(received) = rx.recv() => {
                    // Known-event detection runs before filtering so a coredump
                    // or unit failure still alerts even if it's filtered from the
                    // telemetry stream.
                    if let Some(detector) = &event_detector {
                        detector.on_message(&received.message, &received.resolved_hostname).await;
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

                    // Feed derived rollups (#63) — counts what's actually
                    // published (post-filter), alongside the per-message point.
                    if let Some(agg) = &aggregator_loop {
                        agg.observe(&received.message);
                    }

                    // Per-line event uid (#104): timestamp-prefixed + monotonic
                    // sequence, so each log line gets a unique, time-sortable key
                    // (`events/<uid>`) instead of last-writer-wins facility/severity.
                    let ts_ms = received
                        .message
                        .timestamp
                        .map(|dt| dt.timestamp_millis())
                        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
                    let seq = LOG_EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let uid = receiver::make_log_uid(ts_ms, seq);

                    // Convert to telemetry point
                    let mut point = receiver::to_telemetry_point(&received, include_raw, &uid);

                    // Log-template mining (#102): mine the message text and
                    // attach the stable template id + masked template as labels.
                    if let Some(tagg) = &template_loop {
                        let is_error = (received.message.severity as u8)
                            <= (parser::Severity::Error as u8);
                        if let Some(mined) = tagg.observe(&received.message.message, is_error) {
                            // Novelty detection (#103): a never-before-seen shape
                            // (after warm-up) fires a one-shot `log-novelty`
                            // anomaly; the tick task ages it out / reconciles.
                            if let (Some(tracker), Some(reporter)) =
                                (&novelty_loop, &novelty_reporter)
                                && let Some(alert) = tracker.observe(
                                    &mined.id,
                                    &mined.template,
                                    std::time::Instant::now(),
                                )
                            {
                                let key = alert.alert_key();
                                if let Err(e) = reporter
                                    .observe(alert, Some(std::time::Duration::ZERO))
                                    .await
                                {
                                    tracing::warn!(error = %e, alert = %key, "failed to publish novelty alert");
                                }
                            }
                            point.labels.insert("template_id".to_string(), mined.id);
                            point.labels.insert("template".to_string(), mined.template);
                        }
                    }

                    // Build key expression
                    let key = receiver::build_key_expr(&key_prefix, &received, &uid);

                    // Serialize and publish
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = session_clone.put(&key, payload).await {
                                tracing::error!("Failed to publish to {}: {}", key, e);
                            } else {
                                // Count published telemetry so the Sensors view
                                // reflects this sensor's throughput (#62).
                                publish_health.record_metrics_published(1);
                                tracing::debug!(
                                    "Published: {} from {} [{}]",
                                    key,
                                    received.resolved_hostname,
                                    received.message.severity.as_str()
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to serialize telemetry: {}", e);
                        }
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
