use iced::Subscription;

use zensight_common::{all_telemetry_wildcard, decode_auto, TelemetryPoint, ZenohConfig};

use crate::message::Message;

/// State for the Zenoh subscription.
enum State {
    /// Not connected, will attempt connection.
    Disconnected,
    /// Connected and receiving telemetry.
    Connected(zenoh::Session),
}

/// Create a subscription that connects to Zenoh and receives telemetry.
pub fn zenoh_subscription(config: ZenohConfig) -> Subscription<Message> {
    Subscription::run_with_id(
        "zenoh-telemetry",
        iced::futures::stream::unfold(State::Disconnected, move |state| {
            let config = config.clone();
            async move {
                match state {
                    State::Disconnected => {
                        // Connect to Zenoh
                        match connect_zenoh(&config).await {
                            Ok(session) => Some((Message::Connected, State::Connected(session))),
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to connect to Zenoh");
                                // Wait before retrying
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                Some((Message::Disconnected(e.to_string()), State::Disconnected))
                            }
                        }
                    }
                    State::Connected(session) => {
                        // Subscribe to all zensight telemetry
                        let key_expr = all_telemetry_wildcard();
                        match session.declare_subscriber(&key_expr).await {
                            Ok(subscriber) => {
                                // Process incoming samples
                                loop {
                                    match subscriber.recv_async().await {
                                        Ok(sample) => {
                                            let payload = sample.payload().to_bytes();
                                            match decode_auto::<TelemetryPoint>(&payload) {
                                                Ok(point) => {
                                                    return Some((
                                                        Message::TelemetryReceived(point),
                                                        State::Connected(session),
                                                    ));
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
                                            return Some((
                                                Message::Disconnected(e.to_string()),
                                                State::Disconnected,
                                            ));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to create subscriber");
                                Some((Message::Disconnected(e.to_string()), State::Disconnected))
                            }
                        }
                    }
                }
            }
        }),
    )
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
