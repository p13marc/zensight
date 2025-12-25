use iced::Subscription;

use zensight_common::{TelemetryPoint, ZenohConfig, all_telemetry_wildcard, decode_auto};

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

            // Subscribe to all zensight telemetry
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
                        let payload = sample.payload().to_bytes();
                        match decode_auto::<TelemetryPoint>(&payload) {
                            Ok(point) => {
                                yield Message::TelemetryReceived(point);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    key = %sample.key_expr(),
                                    "Failed to decode telemetry"
                                );
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
        }
    })
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
