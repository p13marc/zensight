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

/// Create a demo subscription that generates mock telemetry data.
///
/// This subscription simulates live data by periodically generating
/// new telemetry points with updated timestamps and varying values.
pub fn demo_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        async_stream::stream! {
            use crate::mock;
            use rand::{Rng, SeedableRng};

            // Signal connected state
            yield Message::Connected;

            // Use a Send-compatible RNG (seeded from system entropy)
            let mut rng = rand::rngs::SmallRng::from_os_rng();
            let mut counter: u64 = 0;

            loop {
                // Wait between updates (simulates real-time data)
                let delay = 500 + rng.random_range(0u64..1000u64);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);

                counter += 1;

                // Generate varying mock data based on counter
                let points = match counter % 5 {
                    0 => mock::snmp::router("router01"),
                    1 => mock::sysinfo::host("server01"),
                    2 => mock::syslog::server("webserver"),
                    3 => mock::modbus::plc("plc01"),
                    _ => mock::snmp::switch("switch01", 4),
                };

                // Update timestamps and add some variation to values
                for mut point in points {
                    point.timestamp = now;

                    // Add some random variation to numeric values
                    match &mut point.value {
                        zensight_common::TelemetryValue::Gauge(v) => {
                            *v += rng.random_range(-5.0..5.0);
                        }
                        zensight_common::TelemetryValue::Counter(v) => {
                            *v = v.saturating_add(rng.random_range(0u64..100u64));
                        }
                        _ => {}
                    }

                    yield Message::TelemetryReceived(point);
                }
            }
        }
    })
}
