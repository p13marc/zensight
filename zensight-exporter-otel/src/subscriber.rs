//! Zenoh subscriber for receiving telemetry points.

use tokio::sync::watch;
use tracing::{debug, info, trace, warn};
use zenoh::sample::SampleKind;
use zensight_common::config::ZenohConfig;
use zensight_common::telemetry::TelemetryPoint;

use crate::exporter::SharedExporter;

/// Default key expression to subscribe to.
pub const DEFAULT_KEY_EXPR: &str = "zensight/**";

/// Zenoh subscriber that feeds telemetry to the OTEL exporter.
pub struct TelemetrySubscriber {
    exporter: SharedExporter,
    zenoh_config: ZenohConfig,
    key_expr: String,
}

impl TelemetrySubscriber {
    /// Create a new subscriber.
    pub fn new(exporter: SharedExporter, zenoh_config: ZenohConfig) -> Self {
        Self {
            exporter,
            zenoh_config,
            key_expr: DEFAULT_KEY_EXPR.to_string(),
        }
    }

    /// Set a custom key expression to subscribe to.
    pub fn with_key_expr(mut self, key_expr: impl Into<String>) -> Self {
        self.key_expr = key_expr.into();
        self
    }

    /// Run the subscriber until the shutdown signal is received.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!("Connecting to Zenoh...");

        // Build Zenoh config
        let mut config = zenoh::Config::default();

        // Set mode
        match self.zenoh_config.mode.as_str() {
            "client" => {
                config
                    .insert_json5("mode", "\"client\"")
                    .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;
            }
            "router" => {
                config
                    .insert_json5("mode", "\"router\"")
                    .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;
            }
            _ => {
                config
                    .insert_json5("mode", "\"peer\"")
                    .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;
            }
        }

        // Set connect endpoints
        if !self.zenoh_config.connect.is_empty() {
            let endpoints_json = serde_json::to_string(&self.zenoh_config.connect)?;
            config
                .insert_json5("connect/endpoints", &endpoints_json)
                .map_err(|e| anyhow::anyhow!("Failed to set connect endpoints: {}", e))?;
        }

        // Set listen endpoints
        if !self.zenoh_config.listen.is_empty() {
            let endpoints_json = serde_json::to_string(&self.zenoh_config.listen)?;
            config
                .insert_json5("listen/endpoints", &endpoints_json)
                .map_err(|e| anyhow::anyhow!("Failed to set listen endpoints: {}", e))?;
        }

        // Open session
        let session = zenoh::open(config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open Zenoh session: {}", e))?;

        info!(
            zid = %session.zid(),
            "Connected to Zenoh"
        );

        // Subscribe to telemetry
        info!(key_expr = %self.key_expr, "Subscribing to telemetry");
        let subscriber = session
            .declare_subscriber(&self.key_expr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create subscriber: {}", e))?;

        info!("Subscriber started, waiting for telemetry...");

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Shutdown signal received, stopping subscriber");
                        break;
                    }
                }

                sample = subscriber.recv_async() => {
                    match sample {
                        Ok(sample) => {
                            if sample.kind() == SampleKind::Delete {
                                trace!(key = %sample.key_expr(), "Ignoring delete sample");
                                continue;
                            }

                            let payload = sample.payload().to_bytes();

                            // Try JSON first, then CBOR
                            let point: Option<TelemetryPoint> =
                                serde_json::from_slice(&payload).ok().or_else(|| {
                                    ciborium::from_reader(&payload[..]).ok()
                                });

                            match point {
                                Some(point) => {
                                    trace!(
                                        source = %point.source,
                                        protocol = %point.protocol,
                                        metric = %point.metric,
                                        "Received telemetry point"
                                    );
                                    self.exporter.record(&point);
                                }
                                None => {
                                    debug!(
                                        key = %sample.key_expr(),
                                        payload_len = payload.len(),
                                        "Failed to decode telemetry point"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Error receiving sample: {}", e);
                        }
                    }
                }
            }
        }

        // Clean shutdown
        subscriber
            .undeclare()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to undeclare subscriber: {}", e))?;
        session
            .close()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to close session: {}", e))?;

        info!("Subscriber stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full subscriber tests require a running Zenoh instance
    // These are basic unit tests for the subscriber configuration

    #[test]
    fn test_default_key_expr() {
        assert_eq!(DEFAULT_KEY_EXPR, "zensight/**");
    }
}
