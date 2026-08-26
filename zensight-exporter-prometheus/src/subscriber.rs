//! Zenoh subscriber for receiving telemetry points.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;
use tracing::{info, trace, warn};
use zenoh::sample::{Sample, SampleKind};
use zensight_common::alert::Alert;
use zensight_common::config::ZenohConfig;
use zensight_common::keyexpr::{all_alerts_wildcard, all_liveliness_wildcard};
use zensight_common::telemetry::TelemetryPoint;

use crate::collector::SharedCollector;

/// Default key expression to subscribe to.
// v1 (RFC 04 §4): the telemetry class selector — the class chunk IS the
// filter, so nothing is discarded client-side (incumbent pain P6 retired).
pub const DEFAULT_KEY_EXPR: &str = "v1/*/telemetry/**";

/// Whether a key carries a [`TelemetryPoint`] — v1: exactly the telemetry
/// class keys (`zensight/v1/<origin>/telemetry/…`). With the class selector
/// as the subscription this is belt-and-braces (a narrowed `filters.key_expr`
/// override could still point anywhere).
///
/// This was a hand-rolled 4-chunk positional gate, copy-pasted byte-for-byte
/// into the OTel exporter. It is now one registry-backed helper (issue #475).
pub(crate) use zensight_common::keyexpr::is_telemetry_key;

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

    /// Fetch the currently-firing alert set with one GET, so a restarted
    /// exporter does not start blind.
    ///
    /// Alerts are LWW state (`…/state/<producer>/alert/<key>`) with a TTL, so
    /// the documents are on the bus. Taking only live `Put`s meant a restart
    /// silently lost every firing alert until its next state transition — the
    /// same "absence is not evidence" mistake the staleness sweep made, from
    /// the other end. The GUI has always seeded this way.
    async fn seed_alerts(session: &zenoh::Session, collector: &SharedCollector) -> usize {
        const SEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let replies = match session
            .get(all_alerts_wildcard())
            .target(zenoh::query::QueryTarget::All)
            .timeout(SEED_TIMEOUT)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Not fatal: without a storage on the state plane there is
                // nothing to answer, which is a normal deployment.
                warn!(error = %e, "Alert seed GET failed; starting with an empty firing set");
                return 0;
            }
        };

        let mut seeded = 0usize;
        while let Ok(reply) = replies.recv_async().await {
            let Ok(sample) = reply.result() else { continue };
            if sample.kind() == SampleKind::Delete {
                continue;
            }
            if let Some(alert) =
                zensight_common::decode_auto::<Alert>(&sample.payload().to_bytes()).ok()
            {
                collector.record_alert(alert);
                seeded += 1;
            }
        }
        seeded
    }

    /// Run the subscriber until the shutdown signal is received.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!("Connecting to Zenoh...");

        // The shared builder is the ONLY place the session `namespace` (= the
        // deployment base) is set (#466). An exporter with its own hand-rolled
        // `zenoh::Config` would subscribe to a keyspace no sensor publishes to,
        // and would simply export nothing — quietly.
        let session = zensight_common::session::connect(&self.zenoh_config)
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
        let alert_subscriber = if self.collector.export_alerts() {
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

        // Liveliness, which is how a departed sensor's alerts are dropped
        // (#758). RFC 04 §5: a producer holds a token at
        // `…/state/<producer>/alive`, so the token vanishing IS "the sensor
        // died" — the event a 300s staleness timer was only standing in for,
        // and which that timer got wrong for every alert older than five
        // minutes.
        //
        // `history(true)` so a sensor already alive when we start is known,
        // rather than only sensors that come up afterwards.
        let liveliness = if self.collector.export_alerts() {
            let alive_key = all_liveliness_wildcard();
            info!(key_expr = %alive_key, "Watching sensor liveliness");
            match session
                .liveliness()
                .declare_subscriber(&alive_key)
                .history(true)
                .await
            {
                Ok(sub) => Some(sub),
                Err(e) => {
                    // Not fatal: without it a dead sensor's alerts linger until
                    // it comes back and tombstones them, which is the old
                    // behaviour minus the false resolves.
                    warn!(error = %e, "Liveliness watch unavailable; departed sensors will keep their alerts");
                    None
                }
            }
        } else {
            None
        };

        // Seed the firing set (#758).
        //
        // Alerts are LWW state with a TTL, so the docs are on the bus to be
        // fetched — but this exporter only ever took live `Put`s, so a RESTART
        // lost every firing alert until its next state transition. Same class
        // of bug as the staleness sweep, from the other end.
        if self.collector.export_alerts() {
            let seeded = Self::seed_alerts(&session, &self.collector).await;
            if seeded > 0 {
                info!(seeded, "Seeded firing alerts from the bus");
            }
        }

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

                // Receive sensor alerts (only polled when export_alerts is on).
                sample = async { alert_subscriber.as_ref().unwrap().recv_async().await },
                    if alert_subscriber.is_some() =>
                {
                    match sample {
                        Ok(sample) => self.handle_alert_sample(&sample),
                        Err(e) => warn!("Error receiving alert sample: {}", e),
                    }
                }

                // A liveliness token vanishing is the real "this sensor died"
                // signal (#758), and the only thing that may drop a firing
                // alert other than the sensor itself.
                sample = async { liveliness.as_ref().unwrap().recv_async().await },
                    if liveliness.is_some() =>
                {
                    match sample {
                        Ok(sample) => {
                            if sample.kind() == SampleKind::Delete {
                                // `…/state/<producer>/alive` — the source is the
                                // ORIGIN chunk, which is what alerts carry as
                                // `source`. Parse it rather than splitting by
                                // hand (#475).
                                if let Some(parsed) =
                                    zensight_common::keyexpr::parse_key(sample.key_expr().as_str())
                                {
                                    let origin = parsed.origin.to_string();
                                    let dropped = self.collector.drop_source_alerts(&origin);
                                    if dropped > 0 {
                                        info!(
                                            origin = %origin,
                                            dropped,
                                            "Sensor liveliness lost; dropped its firing alerts"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => warn!("Error receiving liveliness sample: {}", e),
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
                                    // The KEY is what the registry refines
                                    // (#764) — the payload cannot supply the
                                    // origin or the producer instance.
                                    self.collector
                                        .record(sample.key_expr().as_str(), &point);
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

    /// Decode an alert sample and feed it to the collector. A `Delete` tombstone
    /// clears the firing alert keyed by the final key-expression segment.
    fn handle_alert_sample(&self, sample: &Sample) {
        let key = sample.key_expr().as_str();
        if sample.kind() == SampleKind::Delete {
            // The alert key is the `alert/{alert_key}` variable — ask the
            // registry for it rather than taking the last chunk on faith
            // (issue #475). A tombstone on any other state subject is not ours.
            if let Some((_, _, subject)) = zensight_common::keyexpr::refine_key(key)
                && let Some(zensight_common::CommonState::Alert { alert_key }) =
                    subject.common_state()
            {
                trace!(key = %key, "Alert tombstone");
                self.collector.remove_alert(alert_key);
            }
            return;
        }

        let payload = sample.payload().to_bytes();
        let alert: Option<Alert> = serde_json::from_slice(&payload)
            .ok()
            .or_else(|| ciborium::from_reader(&payload[..]).ok());

        match alert {
            Some(alert) => {
                trace!(source = %alert.source, rule = %alert.rule, "Received alert");
                self.collector.record_alert(alert);
            }
            None => {
                warn!(key = %key, payload_len = payload.len(), "Failed to decode alert");
            }
        }
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
            "v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage"
        ));
        assert!(!is_telemetry_key(
            "v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1"
        ));
        assert!(!is_telemetry_key("v1/h-3fa9c2d41b7e/state/snmp/health"));
        // Host-scoped control plane: the `@` chunk moves one level deeper but
        // stays excluded (any `@`-prefixed chunk is non-telemetry).
        assert!(!is_telemetry_key("v1/@catalog/state/entity/h-0123456789ab"));
        assert!(!is_telemetry_key(
            "v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
        ));
        assert!(!is_telemetry_key("zensight/legacy/host/cpu/usage"));
    }

    /// #359 regression: the media plane rides `@media/...` chunks. The old
    /// predicate only rejected the literal `/@/`, which would have fed opaque
    /// media samples to the TelemetryPoint decoder if a subscription ever
    /// covered them. Any `@`-prefixed chunk is non-telemetry.
    #[test]
    fn media_plane_keys_are_not_telemetry() {
        assert!(!is_telemetry_key(
            "v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main"
        ));
        assert!(!is_telemetry_key(
            "v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
        ));
        // ...while stream *stats* are ordinary telemetry.
        assert!(is_telemetry_key(
            "v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage"
        ));
    }

    /// The telemetry class selector must NOT match alert state keys (D3:
    /// classes are disjoint), which is exactly why alert export needs its own
    /// subscriber on `all_alerts_wildcard()`. Lock that in: a regression here
    /// means alerts silently stop reaching the exporter.
    #[test]
    fn alerts_need_their_own_subscription() {
        use zenoh::key_expr::KeyExpr;

        let alert = KeyExpr::new("v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1").unwrap();
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
