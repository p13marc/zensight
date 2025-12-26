//! Zenoh bridge for Syslog telemetry.
//!
//! This bridge receives syslog messages via UDP, TCP, and Unix socket,
//! parses them (RFC 3164 and RFC 5424 formats), and publishes
//! them to Zenoh as TelemetryPoints.

mod commands;
mod config;
mod filter;
mod parser;
mod receiver;

use anyhow::Result;
use commands::{FilterCommand, FilterStatus};
use config::SyslogBridgeConfig;
use filter::FilterManager;
use std::sync::Arc;
use zensight_bridge_framework::{BridgeArgs, BridgeRunner};
use zensight_common::serialization::{Format, encode};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = BridgeArgs::parse_with_default("syslog.json5");

    // Load configuration
    let config = SyslogBridgeConfig::load_from_file(&args.config)?;

    // Create the bridge runner
    let runner = BridgeRunner::new_with_args("syslog", config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Enable status publishing
    let runner = runner.with_status_publishing();

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

    // Start syslog listeners
    let mut rx = receiver::start_listeners(&syslog_config)
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

    // Spawn the message processing task
    let session_clone = session.clone();
    runner.spawn(async move {
        loop {
            tokio::select! {
                Some(received) = rx.recv() => {
                    // Apply filter
                    if !filter_manager.matches(&received.message, &received.resolved_hostname).await {
                        tracing::trace!(
                            "Filtered message from {} [{}]",
                            received.resolved_hostname,
                            received.message.severity.as_str()
                        );
                        continue;
                    }

                    // Convert to telemetry point
                    let point = receiver::to_telemetry_point(&received, include_raw);

                    // Build key expression
                    let key = receiver::build_key_expr(&key_prefix, &received);

                    // Serialize and publish
                    match encode(&point, format) {
                        Ok(payload) => {
                            if let Err(e) = session_clone.put(&key, payload).await {
                                tracing::error!("Failed to publish to {}: {}", key, e);
                            } else {
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
