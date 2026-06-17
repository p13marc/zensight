use iced::Subscription;
use iced::keyboard::{self, Key, key};

use zenoh::sample::SampleKind;
use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};

use zensight_common::{
    Alert, CorrelationEntry, DeviceLiveness, ErrorReport, HealthSnapshot, SensorInfo,
    TelemetryPoint, ZenohConfig, all_telemetry_wildcard, decode_auto,
};

use crate::message::Message;

/// Key expression for sensor liveliness tokens.
const SENSOR_LIVELINESS_EXPR: &str = "zensight/*/@/alive";

/// Key expression for device liveliness tokens.
const DEVICE_LIVELINESS_EXPR: &str = "zensight/*/@/devices/*/alive";

/// Create a subscription that connects to Zenoh and receives telemetry.
pub fn zenoh_subscription(config: ZenohConfig) -> Subscription<Message> {
    Subscription::run_with(config, move |config| {
        let config = config.clone();
        async_stream::stream! {
            // Signal that we're attempting to connect
            yield Message::Connecting;

            // Connect to Zenoh
            let session = match connect_zenoh(&config).await {
                Ok(session) => {
                    yield Message::Connected(Some(std::sync::Arc::new(session.clone())));
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

            // Subscribe to all zensight telemetry using AdvancedSubscriber
            // This enables:
            // - history(): Get cached samples from publishers on subscription
            // - detect_late_publishers(): Get history from publishers that appear later
            // - recovery(): Automatically recover missed samples
            let key_expr = all_telemetry_wildcard();
            let subscriber = match session
                .declare_subscriber(&key_expr)
                .history(HistoryConfig::default().detect_late_publishers())
                .recovery(RecoveryConfig::default())
                .subscriber_detection()
                .await
            {
                Ok(sub) => sub,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create advanced subscriber");
                    yield Message::Disconnected(e.to_string());
                    return;
                }
            };

            tracing::info!("Advanced subscriber created with history and recovery");

            // Subscribe to sensor liveliness tokens
            let sensor_liveliness = match session
                .liveliness()
                .declare_subscriber(SENSOR_LIVELINESS_EXPR)
                .await
            {
                Ok(sub) => Some(sub),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create sensor liveliness subscriber");
                    None
                }
            };

            // Subscribe to device liveliness tokens
            let device_liveliness = match session
                .liveliness()
                .declare_subscriber(DEVICE_LIVELINESS_EXPR)
                .await
            {
                Ok(sub) => Some(sub),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create device liveliness subscriber");
                    None
                }
            };

            // Query existing liveliness tokens to get current state
            if let Ok(replies) = session.liveliness().get(SENSOR_LIVELINESS_EXPR).await {
                while let Ok(reply) = replies.recv_async().await {
                    if let Ok(sample) = reply.result()
                        && let Some(msg) = parse_sensor_liveliness(sample.key_expr().as_str(), true)
                    {
                        yield msg;
                    }
                }
            }

            if let Ok(replies) = session.liveliness().get(DEVICE_LIVELINESS_EXPR).await {
                while let Ok(reply) = replies.recv_async().await {
                    if let Ok(sample) = reply.result()
                        && let Some(msg) = parse_device_liveliness(sample.key_expr().as_str(), true)
                    {
                        yield msg;
                    }
                }
            }

            // Process incoming samples from all subscriptions
            loop {
                tokio::select! {
                    // Telemetry subscription
                    result = subscriber.recv_async() => {
                        match result {
                            Ok(sample) => {
                                let key = sample.key_expr().as_str();
                                // A Delete on an alert key is a resolve tombstone.
                                if sample.kind() == SampleKind::Delete {
                                    if let Some(msg) = parse_alert_cleared(key) {
                                        yield msg;
                                    }
                                } else {
                                    let payload = sample.payload().to_bytes();
                                    if let Some(msg) = decode_sample(key, &payload) {
                                        yield msg;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Subscriber error");
                                yield Message::Disconnected(e.to_string());
                                return;
                            }
                        }
                    }

                    // Sensor liveliness subscription
                    result = async {
                        match &sensor_liveliness {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result {
                            let is_alive = sample.kind() == SampleKind::Put;
                            if let Some(msg) = parse_sensor_liveliness(sample.key_expr().as_str(), is_alive) {
                                yield msg;
                            }
                        }
                    }

                    // Device liveliness subscription
                    result = async {
                        match &device_liveliness {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result {
                            let is_alive = sample.kind() == SampleKind::Put;
                            if let Some(msg) = parse_device_liveliness(sample.key_expr().as_str(), is_alive) {
                                yield msg;
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Parse a sensor liveliness key expression.
///
/// Key format: `zensight/<protocol>/@/alive`
/// Returns the protocol name.
fn parse_sensor_liveliness(key: &str, is_alive: bool) -> Option<Message> {
    // Parse without allocating a Vec: "zensight/<protocol>/@/alive"
    let rest = key.strip_prefix("zensight/")?;
    let (protocol, rest) = rest.split_once('/')?;
    if rest != "@/alive" {
        return None;
    }
    let protocol = protocol.to_string();
    if is_alive {
        tracing::info!(protocol = %protocol, "Sensor came online");
        Some(Message::SensorOnline(protocol))
    } else {
        tracing::warn!(protocol = %protocol, "Sensor went offline");
        Some(Message::SensorOffline(protocol))
    }
}

/// Parse a device liveliness key expression.
///
/// Key format: `zensight/<protocol>/@/devices/<device_id>/alive`
/// Returns (protocol, device_id).
fn parse_device_liveliness(key: &str, is_alive: bool) -> Option<Message> {
    // Parse without allocating a Vec: "zensight/<protocol>/@/devices/<device_id>/alive"
    let rest = key.strip_prefix("zensight/")?;
    let (protocol, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("@/devices/")?;
    let (device_id, rest) = rest.split_once('/')?;
    if rest != "alive" {
        return None;
    }
    let protocol = protocol.to_string();
    let device_id = device_id.to_string();
    if is_alive {
        tracing::debug!(protocol = %protocol, device = %device_id, "Device came online");
        Some(Message::DeviceOnline(protocol, device_id))
    } else {
        tracing::debug!(protocol = %protocol, device = %device_id, "Device went offline");
        Some(Message::DeviceOffline(protocol, device_id))
    }
}

/// Parse an alert-key Delete tombstone into an [`Message::AlertCleared`].
///
/// Key format: `zensight/<protocol>/@/alerts/<alert_key>`.
fn parse_alert_cleared(key: &str) -> Option<Message> {
    let rest = key.strip_prefix("zensight/")?;
    let (protocol, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("@/alerts/")?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(Message::AlertCleared {
        protocol: protocol.to_string(),
        alert_key: rest.to_string(),
    })
}

/// Decode a sample based on its key expression pattern.
///
/// Routes messages to the appropriate type based on the key structure:
/// - `zensight/<protocol>/@/health` -> HealthSnapshot
/// - `zensight/<protocol>/@/devices/<device>/liveness` -> DeviceLiveness
/// - `zensight/<protocol>/@/errors` -> ErrorReport
/// - `zensight/_meta/sensors/<name>` -> SensorInfo
/// - `zensight/_meta/correlation/<ip>` -> CorrelationEntry
/// - `zensight/<protocol>/<source>/<metric>` -> TelemetryPoint
fn decode_sample(key: &str, payload: &[u8]) -> Option<Message> {
    // Parse without allocating a Vec — use positional split_once
    let rest = key.strip_prefix("zensight/")?;

    // Get the second segment (protocol or _meta)
    let (segment1, rest) = rest.split_once('/')?;

    // Check for metadata paths first
    if segment1 == "_meta" {
        let (segment2, _remainder) = rest.split_once('/').unwrap_or((rest, ""));
        if segment2 == "sensors" {
            return match decode_auto::<SensorInfo>(payload) {
                Ok(info) => Some(Message::SensorInfoReceived(info)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode SensorInfo");
                    None
                }
            };
        } else if segment2 == "correlation" {
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

    // Check for @ paths (sensor health/liveness/errors)
    // rest is now: "@/<subpath>" or "<source>/<metric...>"
    let (segment2, rest_after_seg2) = rest.split_once('/').unwrap_or((rest, ""));
    if segment2 == "@" {
        let (segment3, rest_after_seg3) = rest_after_seg2
            .split_once('/')
            .unwrap_or((rest_after_seg2, ""));
        if segment3 == "health" {
            return match decode_auto::<HealthSnapshot>(payload) {
                Ok(snapshot) => Some(Message::HealthSnapshotReceived(snapshot)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode HealthSnapshot");
                    None
                }
            };
        } else if segment3 == "errors" {
            return match decode_auto::<ErrorReport>(payload) {
                Ok(report) => Some(Message::ErrorReportReceived(report)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode ErrorReport");
                    None
                }
            };
        } else if segment3 == "devices"
            && let Some((device, suffix)) = rest_after_seg3.split_once('/')
            && suffix == "liveness"
        {
            let protocol = segment1.to_string();
            return match decode_auto::<DeviceLiveness>(payload) {
                Ok(liveness) => Some(Message::DeviceLivenessReceived(protocol, liveness)),
                Err(e) => {
                    tracing::warn!(
                        error = %e, key = %key, device = %device,
                        "Failed to decode DeviceLiveness"
                    );
                    None
                }
            };
        } else if segment3 == "alerts" {
            // zensight/<protocol>/@/alerts/<alert_key> (Put = firing/resolved).
            return match decode_auto::<Alert>(payload) {
                Ok(alert) => Some(Message::AlertReceived(alert)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode Alert");
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
/// It also generates sensor health snapshots and device liveness updates to showcase
/// the health monitoring features.
pub fn demo_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        async_stream::stream! {
            use crate::demo::DemoSimulator;

            // Signal connected state (demo mode has no real session)
            yield Message::Connected(None);

            // Create the demo simulator
            let mut simulator = DemoSimulator::new();
            let mut tick_count = 0u64;

            loop {
                // Update interval (500-800ms for responsive UI)
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);

                // Generate a tick of telemetry
                let points = simulator.tick(now);

                // Track metrics per sensor
                let mut sysinfo_count = 0u64;
                let mut snmp_count = 0u64;
                let mut modbus_count = 0u64;
                let mut syslog_count = 0u64;

                // Yield all telemetry points
                for point in points {
                    match point.protocol {
                        zensight_common::Protocol::Sysinfo => sysinfo_count += 1,
                        zensight_common::Protocol::Snmp => snmp_count += 1,
                        zensight_common::Protocol::Modbus => modbus_count += 1,
                        zensight_common::Protocol::Syslog => syslog_count += 1,
                        _ => {}
                    }
                    yield Message::TelemetryReceived(point);
                }

                // Update metrics counts
                simulator.record_metrics("sysinfo", sysinfo_count);
                simulator.record_metrics("snmp", snmp_count);
                simulator.record_metrics("modbus", modbus_count);
                simulator.record_metrics("syslog", syslog_count);

                // Every 5 ticks (~3 seconds), generate health snapshots
                if tick_count.is_multiple_of(5) {
                    for snapshot in simulator.generate_health_snapshots() {
                        yield Message::HealthSnapshotReceived(snapshot);
                    }
                }

                // Every 3 ticks (~1.8 seconds), generate liveness updates
                if tick_count.is_multiple_of(3) {
                    for (protocol, mut liveness) in simulator.generate_liveness_updates() {
                        liveness.last_seen = now;
                        yield Message::DeviceLivenessReceived(protocol, liveness);
                    }
                }

                tick_count += 1;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sensor_liveliness_online() {
        let key = "zensight/snmp/@/alive";
        let msg = parse_sensor_liveliness(key, true);
        assert!(matches!(msg, Some(Message::SensorOnline(ref p)) if p == "snmp"));
    }

    #[test]
    fn test_parse_sensor_liveliness_offline() {
        let key = "zensight/sysinfo/@/alive";
        let msg = parse_sensor_liveliness(key, false);
        assert!(matches!(msg, Some(Message::SensorOffline(ref p)) if p == "sysinfo"));
    }

    #[test]
    fn test_parse_sensor_liveliness_invalid() {
        // Wrong format
        assert!(parse_sensor_liveliness("zensight/snmp/device/metric", true).is_none());
        // Missing alive
        assert!(parse_sensor_liveliness("zensight/snmp/@/health", true).is_none());
        // Wrong prefix
        assert!(parse_sensor_liveliness("other/snmp/@/alive", true).is_none());
    }

    #[test]
    fn test_parse_device_liveliness_online() {
        let key = "zensight/snmp/@/devices/router01/alive";
        let msg = parse_device_liveliness(key, true);
        assert!(
            matches!(msg, Some(Message::DeviceOnline(ref p, ref d)) if p == "snmp" && d == "router01")
        );
    }

    #[test]
    fn test_parse_device_liveliness_offline() {
        let key = "zensight/sysinfo/@/devices/server01/alive";
        let msg = parse_device_liveliness(key, false);
        assert!(
            matches!(msg, Some(Message::DeviceOffline(ref p, ref d)) if p == "sysinfo" && d == "server01")
        );
    }

    #[test]
    fn test_parse_device_liveliness_invalid() {
        // Wrong format
        assert!(parse_device_liveliness("zensight/snmp/@/alive", true).is_none());
        // Missing alive
        assert!(parse_device_liveliness("zensight/snmp/@/devices/router01/status", true).is_none());
        // Too short
        assert!(parse_device_liveliness("zensight/snmp/@/devices", true).is_none());
    }

    #[test]
    fn test_liveliness_key_expressions() {
        // Verify our constants match expected patterns
        assert_eq!(SENSOR_LIVELINESS_EXPR, "zensight/*/@/alive");
        assert_eq!(DEVICE_LIVELINESS_EXPR, "zensight/*/@/devices/*/alive");
    }

    #[test]
    fn test_parse_alert_cleared() {
        let msg = parse_alert_cleared("zensight/netlink/@/alerts/ssh-listening-00ff").unwrap();
        match msg {
            Message::AlertCleared {
                protocol,
                alert_key,
            } => {
                assert_eq!(protocol, "netlink");
                assert_eq!(alert_key, "ssh-listening-00ff");
            }
            _ => panic!("expected AlertCleared"),
        }
        // Not an alert key.
        assert!(parse_alert_cleared("zensight/netlink/@/health").is_none());
        // Nested key (has extra slash) is rejected.
        assert!(parse_alert_cleared("zensight/netlink/@/alerts/a/b").is_none());
    }

    #[test]
    fn test_decode_sample_alert() {
        let alert = zensight_common::Alert::new(
            "host1",
            zensight_common::Protocol::Netring,
            zensight_common::AlertKind::Anomaly,
            "port_scan",
            zensight_common::AlertSeverity::Warning,
            "scan",
        );
        let key = format!("zensight/netring/@/alerts/{}", alert.alert_key());
        let payload = zensight_common::encode(&alert, zensight_common::Format::Json).unwrap();
        match decode_sample(&key, &payload) {
            Some(Message::AlertReceived(got)) => assert_eq!(got.rule, "port_scan"),
            other => panic!("expected AlertReceived, got {other:?}"),
        }
    }
}
