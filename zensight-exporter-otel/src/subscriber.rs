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
// v1 (RFC 04 §4): the telemetry class selector — the class chunk IS the
// filter, so nothing is discarded client-side (incumbent pain P6 retired).
pub const DEFAULT_KEY_EXPR: &str = "zensight/@v1/*/telemetry/**";

/// Whether a key carries a [`TelemetryPoint`] — v1: exactly the telemetry
/// class keys (`zensight/@v1/<origin>/telemetry/…`). With the class selector
/// as the subscription this is belt-and-braces (a narrowed `filters.key_expr`
/// override could still point anywhere).
///
/// This was a hand-rolled 4-chunk positional gate, copy-pasted byte-for-byte
/// from the Prometheus exporter. It is now one registry-backed helper (#475).
pub(crate) use zensight_common::keyexpr::is_telemetry_key;

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

        // Firing alerts are state, not telemetry (`…/state/<producer>/alert/*`),
        // so the telemetry class selector never sees them — they need their
        // own subscriber on the alerts selector.
        let alert_subscriber = if self.exporter.wants_alert_stream() {
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

                // Sensor alerts (`state/*/alert/*`) are exported as OTLP log events
                // and/or synthesized trace spans (polled when either alert-log
                // export or the traces signal is on). A Delete tombstone
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
        assert_eq!(DEFAULT_KEY_EXPR, "zensight/@v1/*/telemetry/**");
    }

    #[test]
    fn telemetry_key_guard() {
        assert!(is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/state/snmp/health"
        ));
        // Host-scoped control plane: the `@` chunk moves one level deeper but
        // stays excluded (any `@`-prefixed chunk is non-telemetry).
        assert!(!is_telemetry_key(
            "zensight/@v1/@catalog/state/entity/h-0123456789ab"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
        ));
        assert!(!is_telemetry_key("zensight/legacy/host/cpu/usage"));
    }

    /// #359 regression: the media plane rides `@media/...` chunks. The old
    /// predicate only rejected the literal `/@/`, which would have let opaque
    /// media samples through to the TelemetryPoint decoder if the subscription
    /// ever covered them. Any `@`-prefixed chunk is non-telemetry.
    #[test]
    fn media_plane_keys_are_not_telemetry() {
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
        ));
        // ...while stream *stats* are ordinary telemetry.
        assert!(is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage"
        ));
    }

    /// The telemetry class selector must NOT match alert state keys (D3:
    /// classes are disjoint), which is exactly why alert export needs its own
    /// subscriber on `all_alerts_wildcard()`. Lock that in: a regression here
    /// means alerts silently stop reaching the exporter.
    #[test]
    fn alerts_need_their_own_subscription() {
        use zenoh::key_expr::KeyExpr;

        let alert =
            KeyExpr::new("zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1")
                .unwrap();
        let telemetry = KeyExpr::new(DEFAULT_KEY_EXPR).unwrap();
        let alerts_sub = KeyExpr::new(all_alerts_wildcard()).unwrap();

        assert!(
            !telemetry.intersects(&alert),
            "the telemetry class selector must not match alert state (D3)"
        );
        assert!(
            alerts_sub.intersects(&alert),
            "the alerts selector must match alert state keys"
        );
    }
}
