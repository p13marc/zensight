//! Zenoh subscriber for receiving telemetry points.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;
use tracing::{info, trace, warn};
use zenoh::sample::SampleKind;
use zensight_common::config::ZenohConfig;
use zensight_common::telemetry::TelemetryPoint;

use crate::collector::SharedCollector;

/// Default key expression to subscribe to.
pub const DEFAULT_KEY_EXPR: &str = "zensight/**";

/// Whether a key carries a [`TelemetryPoint`]. Control/metadata channels
/// (`.../@/...` health/liveness/errors/alerts and `zensight/_meta/...`) do not.
pub(crate) fn is_telemetry_key(key: &str) -> bool {
    !key.contains("/@/") && !key.starts_with("zensight/_meta/")
}

/// Statistics for the subscriber.
#[derive(Debug, Default)]
pub struct SubscriberStats {
    pub samples_received: AtomicU64,
    pub samples_decoded: AtomicU64,
    pub decode_failures: AtomicU64,
}

/// Zenoh subscriber that feeds telemetry to the collector.
pub struct TelemetrySubscriber {
    collector: SharedCollector,
    zenoh_config: ZenohConfig,
    key_expr: String,
    stats: SubscriberStats,
}

impl TelemetrySubscriber {
    /// Create a new subscriber.
    pub fn new(collector: SharedCollector, zenoh_config: ZenohConfig) -> Self {
        Self {
            collector,
            zenoh_config,
            key_expr: DEFAULT_KEY_EXPR.to_string(),
            stats: SubscriberStats::default(),
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
                // Default to peer
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
                // Check for shutdown signal
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Shutdown signal received, stopping subscriber");
                        break;
                    }
                }

                // Receive samples
                sample = subscriber.recv_async() => {
                    match sample {
                        Ok(sample) => {
                            if sample.kind() == SampleKind::Delete {
                                trace!(key = %sample.key_expr(), "Ignoring delete sample");
                                continue;
                            }

                            // Skip non-telemetry channels (health, liveness,
                            // errors, alerts, _meta) — they are not TelemetryPoints
                            // and must not count as decode failures.
                            if !is_telemetry_key(sample.key_expr().as_str()) {
                                trace!(key = %sample.key_expr(), "Ignoring non-telemetry key");
                                continue;
                            }

                            // Try to decode the payload
                            let payload = sample.payload().to_bytes();
                            self.stats.samples_received.fetch_add(1, Ordering::Relaxed);

                            // Try JSON first, then CBOR
                            let point: Option<TelemetryPoint> =
                                serde_json::from_slice(&payload).ok().or_else(|| {
                                    ciborium::from_reader(&payload[..]).ok()
                                });

                            match point {
                                Some(point) => {
                                    self.stats.samples_decoded.fetch_add(1, Ordering::Relaxed);
                                    trace!(
                                        source = %point.source,
                                        protocol = %point.protocol,
                                        metric = %point.metric,
                                        "Received telemetry point"
                                    );
                                    self.collector.record(&point);
                                }
                                None => {
                                    self.stats.decode_failures.fetch_add(1, Ordering::Relaxed);
                                    warn!(
                                        key = %sample.key_expr(),
                                        payload_len = payload.len(),
                                        "Failed to decode telemetry point as JSON or CBOR"
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
    use crate::collector::MetricCollector;
    use crate::config::{AggregationConfig, FilterConfig, PrometheusConfig};
    use std::sync::Arc;

    #[test]
    fn test_subscriber_creation() {
        let collector = Arc::new(MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig::default(),
            FilterConfig::default(),
        ));

        let subscriber = TelemetrySubscriber::new(collector, ZenohConfig::default());
        assert_eq!(subscriber.key_expr, DEFAULT_KEY_EXPR);
    }

    #[test]
    fn test_subscriber_custom_key_expr() {
        let collector = Arc::new(MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig::default(),
            FilterConfig::default(),
        ));

        let subscriber =
            TelemetrySubscriber::new(collector, ZenohConfig::default()).with_key_expr("custom/**");
        assert_eq!(subscriber.key_expr, "custom/**");
    }

    #[test]
    fn telemetry_key_guard() {
        assert!(is_telemetry_key(
            "zensight/netlink/host/iface/eth0/rx_bytes"
        ));
        assert!(!is_telemetry_key("zensight/netlink/@/alerts/foo-00"));
        assert!(!is_telemetry_key("zensight/snmp/@/health"));
        assert!(!is_telemetry_key("zensight/_meta/sensors/snmp"));
    }
}
