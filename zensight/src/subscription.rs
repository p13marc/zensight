use iced::Subscription;
use iced::keyboard::{self, Key, key};

use zensight_common::{
    BridgeInfo, CorrelationEntry, DeviceLiveness, ErrorReport, HealthSnapshot, TelemetryPoint,
    ZenohConfig, all_telemetry_wildcard, decode_auto,
};

use crate::message::Message;

/// Create a subscription that connects to Zenoh and receives telemetry.
pub fn zenoh_subscription(config: ZenohConfig) -> Subscription<Message> {
    Subscription::run_with(config, move |config| {
        let config = config.clone();
        async_stream::stream! {
            // Connect to Zenoh
            let session = match connect_zenoh(&config).await {
                Ok(session) => {
                    yield Message::Connected;
                    session
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to connect to Zenoh");
                    yield Message::Disconnected(e.to_string());
                    // Wait before the stream ends (subscription will restart)
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    return;
                }
            };

            // Subscribe to all zensight telemetry (uses ** wildcard to catch everything)
            let key_expr = all_telemetry_wildcard();
            let subscriber = match session.declare_subscriber(&key_expr).await {
                Ok(sub) => sub,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create subscriber");
                    yield Message::Disconnected(e.to_string());
                    return;
                }
            };

            // Process incoming samples
            loop {
                match subscriber.recv_async().await {
                    Ok(sample) => {
                        let key = sample.key_expr().as_str();
                        let payload = sample.payload().to_bytes();

                        // Route to appropriate handler based on key expression pattern
                        if let Some(msg) = decode_sample(key, &payload) {
                            yield msg;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Subscriber error");
                        yield Message::Disconnected(e.to_string());
                        return;
                    }
                }
            }
        }
    })
}

/// Decode a sample based on its key expression pattern.
///
/// Routes messages to the appropriate type based on the key structure:
/// - `zensight/<protocol>/@/health` -> HealthSnapshot
/// - `zensight/<protocol>/@/devices/<device>/liveness` -> DeviceLiveness
/// - `zensight/<protocol>/@/errors` -> ErrorReport
/// - `zensight/_meta/bridges/<name>` -> BridgeInfo
/// - `zensight/_meta/correlation/<ip>` -> CorrelationEntry
/// - `zensight/<protocol>/<source>/<metric>` -> TelemetryPoint
fn decode_sample(key: &str, payload: &[u8]) -> Option<Message> {
    let parts: Vec<&str> = key.split('/').collect();

    // Minimum valid key: zensight/<protocol>/<something>
    if parts.len() < 3 || parts[0] != "zensight" {
        return None;
    }

    // Check for metadata paths first
    if parts[1] == "_meta" {
        if parts.len() >= 4 && parts[2] == "bridges" {
            // zensight/_meta/bridges/<name>
            return match decode_auto::<BridgeInfo>(payload) {
                Ok(info) => Some(Message::BridgeInfoReceived(info)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode BridgeInfo");
                    None
                }
            };
        } else if parts.len() >= 4 && parts[2] == "correlation" {
            // zensight/_meta/correlation/<ip>
            return match decode_auto::<CorrelationEntry>(payload) {
                Ok(entry) => Some(Message::CorrelationReceived(entry)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode CorrelationEntry");
                    None
                }
            };
        }
        return None;
    }

    // Check for @ paths (bridge health/liveness/errors)
    if parts.len() >= 4 && parts[2] == "@" {
        if parts[3] == "health" {
            // zensight/<protocol>/@/health
            return match decode_auto::<HealthSnapshot>(payload) {
                Ok(snapshot) => Some(Message::HealthSnapshotReceived(snapshot)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode HealthSnapshot");
                    None
                }
            };
        } else if parts[3] == "errors" {
            // zensight/<protocol>/@/errors
            return match decode_auto::<ErrorReport>(payload) {
                Ok(report) => Some(Message::ErrorReportReceived(report)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode ErrorReport");
                    None
                }
            };
        } else if parts[3] == "devices" && parts.len() >= 6 && parts[5] == "liveness" {
            // zensight/<protocol>/@/devices/<device>/liveness
            let protocol = parts[1].to_string();
            return match decode_auto::<DeviceLiveness>(payload) {
                Ok(liveness) => Some(Message::DeviceLivenessReceived(protocol, liveness)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode DeviceLiveness");
                    None
                }
            };
        }
        return None;
    }

    // Regular telemetry: zensight/<protocol>/<source>/<metric...>
    match decode_auto::<TelemetryPoint>(payload) {
        Ok(point) => Some(Message::TelemetryReceived(point)),
        Err(e) => {
            tracing::warn!(error = %e, key = %key, "Failed to decode TelemetryPoint");
            None
        }
    }
}

/// Connect to Zenoh using the provided configuration.
async fn connect_zenoh(config: &ZenohConfig) -> anyhow::Result<zenoh::Session> {
    let mut zenoh_config = zenoh::Config::default();

    // Set mode
    let mode_str = format!("\"{}\"", config.mode);
    zenoh_config
        .insert_json5("mode", &mode_str)
        .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;

    // Set connect endpoints
    if !config.connect.is_empty() {
        let endpoints_json = serde_json::to_string(&config.connect)?;
        zenoh_config
            .insert_json5("connect/endpoints", &endpoints_json)
            .map_err(|e| anyhow::anyhow!("Failed to set connect endpoints: {}", e))?;
    }

    // Set listen endpoints
    if !config.listen.is_empty() {
        let endpoints_json = serde_json::to_string(&config.listen)?;
        zenoh_config
            .insert_json5("listen/endpoints", &endpoints_json)
            .map_err(|e| anyhow::anyhow!("Failed to set listen endpoints: {}", e))?;
    }

    tracing::info!(
        mode = %config.mode,
        connect = ?config.connect,
        listen = ?config.listen,
        "Connecting to Zenoh"
    );

    let session = zenoh::open(zenoh_config)
        .await
        .map_err(|e| anyhow::anyhow!("Zenoh open failed: {}", e))?;

    tracing::info!(zid = %session.zid(), "Connected to Zenoh");

    Ok(session)
}

/// Create a tick subscription for periodic UI updates.
pub fn tick_subscription() -> Subscription<Message> {
    iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::Tick)
}

/// Create a keyboard subscription for global shortcuts.
///
/// Handles:
/// - Ctrl+F: Focus search input
/// - Escape: Close dialogs, clear selections
pub fn keyboard_subscription() -> Subscription<Message> {
    keyboard::listen()
        .map(|event| {
            if let keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                match key.as_ref() {
                    // Ctrl+F: Focus search
                    Key::Character("f") if modifiers.control() => Some(Message::FocusSearch),

                    // Escape: Close/back
                    Key::Named(key::Named::Escape) => Some(Message::EscapePressed),

                    _ => None,
                }
            } else {
                None
            }
        })
        .filter_map(|msg| msg)
}

/// Create a demo subscription that generates mock telemetry data.
///
/// This subscription uses the [`DemoSimulator`](crate::demo::DemoSimulator) to generate
/// realistic, time-varying telemetry with random anomalies and events that trigger alerts.
pub fn demo_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        async_stream::stream! {
            use crate::demo::DemoSimulator;

            // Signal connected state
            yield Message::Connected;

            // Create the demo simulator
            let mut simulator = DemoSimulator::new();

            loop {
                // Update interval (500-800ms for responsive UI)
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);

                // Generate a tick of telemetry
                let points = simulator.tick(now);

                // Yield all points
                for point in points {
                    yield Message::TelemetryReceived(point);
                }
            }
        }
    })
}
