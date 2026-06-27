//! Zenoh subscriber for receiving telemetry points.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;
use tracing::{info, trace, warn};
use zenoh::sample::{Sample, SampleKind};
use zensight_common::alert::Alert;
use zensight_common::config::ZenohConfig;
use zensight_common::keyexpr::all_alerts_wildcard;
use zensight_common::telemetry::TelemetryPoint;

use crate::exporter::SharedExporter;

/// Default key expression to subscribe to.
pub const DEFAULT_KEY_EXPR: &str = "zensight/**";

/// Whether a key carries a [`TelemetryPoint`]. Control/metadata channels
/// (`.../@/...` and `zensight/_meta/...`) do not.
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

/// Zenoh subscriber that feeds telemetry to the OTEL exporter.
pub struct TelemetrySubscriber {
    exporter: SharedExporter,
    zenoh_config: ZenohConfig,
    key_expr: String,
    stats: SubscriberStats,
}

impl TelemetrySubscriber {
    /// Create a new subscriber.
    pub fn new(exporter: SharedExporter, zenoh_config: ZenohConfig) -> Self {
        Self {
            exporter,
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

        // Sensor alerts live on the `@/alerts/*` control channel. The telemetry
        // wildcard `zensight/**` does NOT match `@/`-prefixed chunks (Zenoh
        // treats a chunk starting with `@` as verbatim), so firing alerts need
        // their own subscriber on `zensight/*/@/alerts/*`.
        let alert_subscriber = if self.exporter.export_alerts() {
            let alerts_key = all_alerts_wildcard();
            info!(key_expr = %alerts_key, "Subscribing to sensor alerts");
            Some(
                session
                    .declare_subscriber(&alerts_key)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create alert subscriber: {}", e))?,
            )
        } else {
            None
        };

        info!("Subscriber started, waiting for telemetry...");

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Shutdown signal received, stopping subscriber");
                        break;
                    }
                }

                // Sensor alerts (`@/alerts/*`) are exported as OTLP log events
                // (only polled when export_alerts is on). A Delete tombstone
                // carries no payload — the prior Resolved Put already emitted the
                // resolved event — so it's ignored.
                sample = async { alert_subscriber.as_ref().unwrap().recv_async().await },
                    if alert_subscriber.is_some() =>
                {
                    match sample {
                        Ok(sample) if sample.kind() != SampleKind::Delete => {
                            self.handle_alert_sample(&sample);
                        }
                        Ok(_) => {}
                        Err(e) => warn!("Error receiving alert sample: {}", e),
                    }
                }

                sample = subscriber.recv_async() => {
                    match sample {
                        Ok(sample) => {
                            if sample.kind() == SampleKind::Delete {
                                trace!(key = %sample.key_expr(), "Ignoring delete sample");
                                continue;
                            }

                            // Skip non-telemetry channels (health/liveness/errors/
                            // alerts/_meta) so they don't count as decode failures.
                            if !is_telemetry_key(sample.key_expr().as_str()) {
                                trace!(key = %sample.key_expr(), "Ignoring non-telemetry key");
                                continue;
                            }

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
                                    self.exporter.record(&point);
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
        if let Some(alert_subscriber) = alert_subscriber {
            alert_subscriber
                .undeclare()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to undeclare alert subscriber: {}", e))?;
        }
        session
            .close()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to close session: {}", e))?;

        info!("Subscriber stopped");
        Ok(())
    }

    /// Decode an alert sample (a firing/resolved Put) and emit it as an OTLP
    /// log event.
    fn handle_alert_sample(&self, sample: &Sample) {
        let payload = sample.payload().to_bytes();
        let alert: Option<Alert> = serde_json::from_slice(&payload)
            .ok()
            .or_else(|| ciborium::from_reader(&payload[..]).ok());

        match alert {
            Some(alert) => self.exporter.record_alert(&alert),
            None => warn!(
                key = %sample.key_expr(),
                payload_len = payload.len(),
                "Failed to decode alert"
            ),
        }
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

    #[test]
    fn telemetry_key_guard() {
        assert!(is_telemetry_key(
            "zensight/netlink/host/iface/eth0/rx_bytes"
        ));
        assert!(!is_telemetry_key("zensight/netlink/@/alerts/foo-00"));
        assert!(!is_telemetry_key("zensight/snmp/@/health"));
        assert!(!is_telemetry_key("zensight/_meta/sensors/snmp"));
    }

    /// The telemetry wildcard `zensight/**` must NOT match `@/alerts/*` (Zenoh
    /// treats an `@`-prefixed chunk as verbatim), which is exactly why alert
    /// export needs its own subscriber on `all_alerts_wildcard()`. Lock that in:
    /// a regression here means alerts silently stop reaching the exporter.
    #[test]
    fn alerts_need_their_own_subscription() {
        use zenoh::key_expr::KeyExpr;

        let alert = KeyExpr::new("zensight/netlink/@/alerts/foo-00").unwrap();
        let telemetry = KeyExpr::new(DEFAULT_KEY_EXPR).unwrap();
        let alerts_sub = KeyExpr::new(all_alerts_wildcard()).unwrap();

        assert!(
            !telemetry.intersects(&alert),
            "zensight/** must not match @/alerts/* — a single telemetry subscriber cannot see alerts"
        );
        assert!(
            alerts_sub.intersects(&alert),
            "the alerts wildcard must match @/alerts/* so the dedicated subscriber receives them"
        );
    }
}
