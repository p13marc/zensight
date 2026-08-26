//! Zenoh subscriber for receiving telemetry points.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;
use tracing::{info, trace, warn};
use zenoh::sample::{Sample, SampleKind};
use zensight_common::alert::Alert;
use zensight_common::config::ZenohConfig;
use zensight_common::keyexpr::{all_alerts_wildcard, all_events_wildcard};

use crate::exporter::SharedExporter;

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
/// from the Prometheus exporter. It is now one registry-backed helper (#475).
#[cfg_attr(not(test), allow(unused_imports))]
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
        // An ADVANCED subscriber, not a plain one (#763). Sensors publish
        // telemetry through an `AdvancedPublisher` and the GUI has always
        // consumed with history + recovery; both exporters used a plain
        // subscriber, so one started after the sensors got no backfill and a
        // sample dropped in flight was simply lost. For a metrics pipeline that
        // is the wrong trade — a gap in a dashboard is a claim about the world.
        let subscriber =
            zensight_common::subscribe::declare_telemetry_subscriber(&session, &self.key_expr)
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

        // The `events` class (#534) — append-only records on
        // `v1/<origin>/events/<producer>/<subject…>/<ulid>`. Its own class, so
        // neither the telemetry selector nor the alerts selector can reach it,
        // which is why SNMP traps and systemd unit failures never reached OTLP
        // at all (#762).
        let event_subscriber = if self.exporter.export_events() {
            let events_key = all_events_wildcard();
            info!(key_expr = %events_key, "Subscribing to the events class");
            Some(
                session
                    .declare_subscriber(&events_key)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create event subscriber: {}", e))?,
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

                // Receive events-class records (only polled when enabled).
                sample = async { event_subscriber.as_ref().unwrap().recv_async().await },
                    if event_subscriber.is_some() =>
                {
                    match sample {
                        Ok(sample) => self.handle_event_sample(&sample),
                        Err(e) => warn!("Error receiving event sample: {}", e),
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
                            // Shared with the Prometheus exporter (#763): the
                            // class guard and the JSON-then-CBOR sniff were
                            // byte-identical in both, and a non-telemetry
                            // channel must NOT count as a decode failure.
                            self.stats.samples_received.fetch_add(1, Ordering::Relaxed);
                            let point = match zensight_common::subscribe::decode_telemetry(&sample)
                            {
                                Ok(p) => Some(p),
                                Err(zensight_common::subscribe::DecodeReject::NotTelemetry) => {
                                    trace!(key = %sample.key_expr(), "Ignoring non-telemetry key");
                                    continue;
                                }
                                Err(zensight_common::subscribe::DecodeReject::Undecodable) => None,
                            };

                            match point {
                                Some(point) => {
                                    self.stats.samples_decoded.fetch_add(1, Ordering::Relaxed);
                                    trace!(
                                        source = %point.source,
                                        protocol = %point.protocol,
                                        metric = %point.metric,
                                        "Received telemetry point"
                                    );
                                    // The KEY is what the registry refines (#764).
                                    self.exporter
                                        .record(sample.key_expr().as_str(), &point);
                                }
                                None => {
                                    self.stats.decode_failures.fetch_add(1, Ordering::Relaxed);
                                    warn!(
                                        key = %sample.key_expr(),
                                        payload_len = sample.payload().len(),
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
        if let Some(event_subscriber) = event_subscriber {
            event_subscriber
                .undeclare()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to undeclare event subscriber: {}", e))?;
        }
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
    /// Decode an events-class record and hand it to the exporter.
    ///
    /// Records are append-only and ULID-keyed — one key per record, nothing
    /// overwrites — so there is no tombstone to handle here, unlike alerts.
    fn handle_event_sample(&self, sample: &Sample) {
        if sample.kind() == SampleKind::Delete {
            return;
        }
        let payload = sample.payload().to_bytes();
        match zensight_common::decode_auto::<zensight_common::event::EventRecord>(&payload) {
            Ok(event) => self
                .exporter
                .record_event(sample.key_expr().as_str(), &event),
            Err(e) => warn!(
                key = %sample.key_expr(),
                payload_len = payload.len(),
                error = %e,
                "Failed to decode events-class record"
            ),
        }
    }

    fn handle_alert_sample(&self, sample: &Sample) {
        let payload = sample.payload().to_bytes();
        let alert: Option<Alert> = serde_json::from_slice(&payload)
            .ok()
            .or_else(|| ciborium::from_reader(&payload[..]).ok());

        match alert {
            Some(alert) => self
                .exporter
                .record_alert(sample.key_expr().as_str(), &alert),
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
        assert_eq!(DEFAULT_KEY_EXPR, "v1/*/telemetry/**");
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
    /// predicate only rejected the literal `/@/`, which would have let opaque
    /// media samples through to the TelemetryPoint decoder if the subscription
    /// ever covered them. Any `@`-prefixed chunk is non-telemetry.
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

    /// The events class needs its own subscription too (#762).
    ///
    /// `events` is a third class, disjoint from both `telemetry` and `state` by
    /// construction — which is exactly why SNMP traps and systemd unit failures
    /// reached the GUI and a Zenoh storage while never reaching OTLP at all.
    /// Neither existing selector could ever have seen them.
    #[test]
    fn the_events_class_needs_its_own_subscription() {
        use zenoh::key_expr::KeyExpr;

        let event =
            KeyExpr::new("v1/h-3fa9c2d41b7e/events/snmp/trap/01hqzz000000000000000000ab").unwrap();
        let telemetry = KeyExpr::new(DEFAULT_KEY_EXPR).unwrap();
        let alerts_sub = KeyExpr::new(all_alerts_wildcard()).unwrap();
        let events_sub = KeyExpr::new(all_events_wildcard()).unwrap();

        assert!(
            !telemetry.intersects(&event),
            "the telemetry selector cannot reach the events class"
        );
        assert!(
            !alerts_sub.intersects(&event),
            "the alerts selector cannot reach the events class either — which is \
             why a third subscriber is required, not optional"
        );
        assert!(
            events_sub.intersects(&event),
            "the events selector must match events-class keys"
        );
    }
}
