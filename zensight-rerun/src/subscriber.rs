//! Zenoh subscriptions feeding the sink worker.
//!
//! Four channels, same split as the frontend/exporters (v1, RFC 04):
//! - telemetry `zensight/@v1/*/telemetry/**` (or a narrowed
//!   `filters.key_expr`),
//! - alerts `zensight/@v1/*/state/*/alert/*` — classes are disjoint (D3), so
//!   the telemetry selector can NEVER see alerts and they need their own
//!   subscriber (pinned below),
//! - health `zensight/@v1/*/state/*/health`,
//! - entities `zensight/@v1/@catalog/state/entity/*`, seeded by a one-shot
//!   storage-shaped GET on the same selector (concurrent with
//!   the drain loop — all subscribers are declared before it starts).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, trace, warn};
use zenoh::Session;
use zenoh::sample::{Sample, SampleKind};
use zensight_common::alert::Alert;
use zensight_common::decode_auto;
use zensight_common::entity::HostEntity;
use zensight_common::health::HealthSnapshot;
use zensight_common::keyexpr::{
    all_alerts_wildcard, all_entity_wildcard, all_health_wildcard, all_telemetry_wildcard,
    entities_query_key,
};
use zensight_common::telemetry::TelemetryPoint;

use crate::config::FilterConfig;
use crate::sink::ControlItem;

/// How long the late-joiner entity seed GET waits for the correlator.
pub const ENTITY_SEED_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether a key carries a [`TelemetryPoint`] — v1: exactly the telemetry
/// class keys (`zensight/@v1/<origin>/telemetry/…`). With the class selector
/// as the subscription this is belt-and-braces (a narrowed `filters.key_expr`
/// override could still point anywhere). Local copy by design — the exporters
/// each carry one too; shared extraction is deferred (03-sink-design.md §2).
pub(crate) fn is_telemetry_key(key: &str) -> bool {
    let mut chunks = key.split('/');
    chunks.next() == Some("zensight")
        && chunks.next() == Some("@v1")
        && chunks.next().is_some_and(|origin| !origin.starts_with('@'))
        && chunks.next() == Some("telemetry")
}

/// Subscriber-side counters (shared, readable while running).
#[derive(Debug, Default)]
pub struct SubscriberStats {
    pub telemetry_received: AtomicU64,
    pub telemetry_decode_failures: AtomicU64,
    /// Telemetry dropped because the (bounded) worker queue was full.
    pub telemetry_dropped: AtomicU64,
    pub control_received: AtomicU64,
    pub control_decode_failures: AtomicU64,
}

/// Run the subscription loop over an existing session until shutdown.
///
/// The session is taken by argument (not opened here) so tests and demos can
/// share one lone isolated session between publisher and adapter.
pub async fn run_with_session(
    session: Arc<Session>,
    filters: FilterConfig,
    tx_telemetry: mpsc::Sender<TelemetryPoint>,
    tx_control: mpsc::Sender<ControlItem>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<Arc<SubscriberStats>> {
    let stats = Arc::new(SubscriberStats::default());

    // Declare ALL subscribers before the entity-seed GET: the GET can block
    // up to ENTITY_SEED_TIMEOUT when no correlator is running (the common
    // case), and plain subscribers have no history — anything published
    // before they exist is lost. Entities: subscribe first, then seed — no
    // gap; the worker's upsert is idempotent so a doc seen twice is harmless.
    let entity_sub = session
        .declare_subscriber(&all_entity_wildcard())
        .await
        .map_err(|e| anyhow::anyhow!("failed to create entity subscriber: {e}"))?;

    let telemetry_key = filters
        .key_expr
        .clone()
        .unwrap_or_else(all_telemetry_wildcard);
    info!(key_expr = %telemetry_key, "Subscribing to telemetry");
    let telemetry_sub = session
        .declare_subscriber(&telemetry_key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create telemetry subscriber: {e}"))?;

    let alerts_key = all_alerts_wildcard();
    info!(key_expr = %alerts_key, "Subscribing to alerts");
    let alert_sub = session
        .declare_subscriber(&alerts_key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create alert subscriber: {e}"))?;

    let health_key = all_health_wildcard();
    info!(key_expr = %health_key, "Subscribing to health");
    let health_sub = session
        .declare_subscriber(&health_key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create health subscriber: {e}"))?;

    // Late-joiner entity seed, concurrent with the drain loop: the GET can
    // stall up to ENTITY_SEED_TIMEOUT when no correlator answers, and neither
    // the subscribers (already declared) nor the loop below should wait on
    // it. Runtime is bounded by the GET timeout; the clone of `tx_control` it
    // holds is dropped when it finishes, so the worker's shutdown drain still
    // terminates.
    let seed_task = tokio::spawn(seed_entities(session.clone(), tx_control.clone()));

    info!("Subscriber started, waiting for samples...");

    loop {
        tokio::select! {
            changed = shutdown.changed() => match changed {
                // A dropped sender (Err) is a shutdown too — treating it as a
                // no-op would busy-spin this permanently-ready arm. Exiting
                // also drops tx_telemetry/tx_control, which is what lets the
                // worker's shutdown drain terminate.
                Err(_) => {
                    info!("Shutdown channel closed, stopping subscriber");
                    break;
                }
                Ok(()) if *shutdown.borrow() => {
                    info!("Shutdown signal received, stopping subscriber");
                    break;
                }
                Ok(()) => {}
            },

            sample = alert_sub.recv_async() => match sample {
                // A Delete tombstone carries no payload — the prior Resolved
                // Put already produced the resolved transition; ignore it.
                Ok(sample) if sample.kind() != SampleKind::Delete => {
                    handle_control_sample::<Alert>(&sample, &tx_control, &stats, ControlItem::Alert)
                        .await;
                }
                Ok(_) => {}
                Err(e) => warn!("error receiving alert sample: {e}"),
            },

            sample = health_sub.recv_async() => match sample {
                Ok(sample) if sample.kind() != SampleKind::Delete => {
                    handle_control_sample::<HealthSnapshot>(
                        &sample, &tx_control, &stats, ControlItem::Health,
                    )
                    .await;
                }
                Ok(_) => {}
                Err(e) => warn!("error receiving health sample: {e}"),
            },

            sample = entity_sub.recv_async() => match sample {
                // Entity tombstones (retire/merge) are ignored for now: the
                // EntityIndex keeps routing by the last known membership.
                Ok(sample) if sample.kind() != SampleKind::Delete => {
                    handle_control_sample::<HostEntity>(&sample, &tx_control, &stats, |e| {
                        ControlItem::Entity(Box::new(e))
                    })
                    .await;
                }
                Ok(_) => {}
                Err(e) => warn!("error receiving entity sample: {e}"),
            },

            sample = telemetry_sub.recv_async() => match sample {
                Ok(sample) => handle_telemetry_sample(&sample, &filters, &tx_telemetry, &stats),
                Err(e) => warn!("error receiving telemetry sample: {e}"),
            },
        }
    }

    // Shutting down: a still-running seed GET has nothing left to seed.
    seed_task.abort();

    let dropped = stats.telemetry_dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        warn!(dropped, "telemetry samples dropped on a full worker queue");
    }

    telemetry_sub
        .undeclare()
        .await
        .map_err(|e| anyhow::anyhow!("failed to undeclare telemetry subscriber: {e}"))?;
    alert_sub
        .undeclare()
        .await
        .map_err(|e| anyhow::anyhow!("failed to undeclare alert subscriber: {e}"))?;
    health_sub
        .undeclare()
        .await
        .map_err(|e| anyhow::anyhow!("failed to undeclare health subscriber: {e}"))?;
    entity_sub
        .undeclare()
        .await
        .map_err(|e| anyhow::anyhow!("failed to undeclare entity subscriber: {e}"))?;

    info!("Subscriber stopped");
    Ok(stats)
}

/// One-shot late-joiner entity seed: GET the catalog's
/// entity state selector (storage-shaped: one `HostEntity` per reply) and
/// forward the docs as control items. Best-effort — no correlator (the common case) is a debug, not an
/// error.
async fn seed_entities(session: Arc<Session>, tx_control: mpsc::Sender<ControlItem>) {
    match session
        .get(entities_query_key())
        .timeout(ENTITY_SEED_TIMEOUT)
        .await
    {
        Ok(replies) => {
            let mut seeded = 0usize;
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result()
                    && let Ok(entity) = decode_auto::<HostEntity>(&sample.payload().to_bytes())
                {
                    seeded += 1;
                    {
                        // Must-arrive: blocking send (worker upserts are
                        // idempotent, so racing the live subscription is
                        // harmless).
                        if tx_control
                            .send(ControlItem::Entity(Box::new(entity)))
                            .await
                            .is_err()
                        {
                            warn!("control channel closed during entity seed");
                            return;
                        }
                    }
                }
            }
            info!(seeded, "Entity seed complete");
        }
        Err(e) => debug!("entity seed query failed (no correlator?): {e}"),
    }
}

/// Decode + forward one must-arrive control sample (blocking send).
async fn handle_control_sample<T: serde::de::DeserializeOwned>(
    sample: &Sample,
    tx_control: &mpsc::Sender<ControlItem>,
    stats: &SubscriberStats,
    wrap: impl FnOnce(T) -> ControlItem,
) {
    stats.control_received.fetch_add(1, Ordering::Relaxed);
    let payload = sample.payload().to_bytes();
    match decode_auto::<T>(&payload) {
        Ok(value) => {
            if tx_control.send(wrap(value)).await.is_err() {
                warn!("control channel closed; dropping sample");
            }
        }
        Err(e) => {
            stats
                .control_decode_failures
                .fetch_add(1, Ordering::Relaxed);
            warn!(key = %sample.key_expr(), payload_len = payload.len(), "failed to decode control sample: {e}");
        }
    }
}

/// Decode + forward one telemetry sample (drop-newest on a full queue).
fn handle_telemetry_sample(
    sample: &Sample,
    filters: &FilterConfig,
    tx_telemetry: &mpsc::Sender<TelemetryPoint>,
    stats: &SubscriberStats,
) {
    if sample.kind() == SampleKind::Delete {
        trace!(key = %sample.key_expr(), "ignoring delete sample");
        return;
    }
    // Skip non-telemetry channels (health/alerts/@media/_meta) so they don't
    // count as decode failures.
    if !is_telemetry_key(sample.key_expr().as_str()) {
        trace!(key = %sample.key_expr(), "ignoring non-telemetry key");
        return;
    }

    let payload = sample.payload().to_bytes();
    stats.telemetry_received.fetch_add(1, Ordering::Relaxed);
    match decode_auto::<TelemetryPoint>(&payload) {
        Ok(point) => {
            if !filters.allows_protocol(point.protocol.as_str()) {
                return;
            }
            if tx_telemetry.try_send(point).is_err() {
                // Full (or closed) — drop-newest, count it, never block.
                stats.telemetry_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) => {
            stats
                .telemetry_decode_failures
                .fetch_add(1, Ordering::Relaxed);
            warn!(key = %sample.key_expr(), payload_len = payload.len(), "failed to decode telemetry point: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_key_guard() {
        assert!(is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/telemetry/netlink/iface/eth0/rx_bytes"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/state/sysinfo/health"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/@catalog/state/entity/h-0123456789ab"
        ));
        assert!(!is_telemetry_key("zensight/legacy/host/cpu/usage"));
        // The media plane rides a verbatim `@media` chunk (#359).
        assert!(!is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main"
        ));
        assert!(!is_telemetry_key(
            "zensight/@v1/@catalog/state/pdns/10-0-0-9"
        ));
        // ...while stream *stats* are ordinary telemetry.
        assert!(is_telemetry_key(
            "zensight/@v1/h-3fa9c2d41b7e/telemetry/parallax/cam0/stats/fps"
        ));
    }

    /// The telemetry class selector must NOT match alert state keys (D3:
    /// classes are disjoint) — which is exactly why the adapter declares a
    /// separate alerts subscriber. A regression here means alerts silently
    /// stop reaching the sink.
    #[test]
    fn alerts_need_their_own_subscription() {
        use zenoh::key_expr::KeyExpr;

        let alert =
            KeyExpr::new("zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1")
                .unwrap();
        let telemetry = KeyExpr::new(all_telemetry_wildcard()).unwrap();
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

    /// Same for health and entities: three more subscribers, three disjoint
    /// planes.
    #[test]
    fn health_and_entities_need_their_own_subscriptions() {
        use zenoh::key_expr::KeyExpr;

        let telemetry = KeyExpr::new(all_telemetry_wildcard()).unwrap();

        let health = KeyExpr::new("zensight/@v1/h-3fa9c2d41b7e/state/sysinfo/health").unwrap();
        assert!(!telemetry.intersects(&health));
        assert!(
            KeyExpr::new(all_health_wildcard())
                .unwrap()
                .intersects(&health)
        );

        // v1: the entity plane sits under the verbatim `@catalog` origin —
        // structurally invisible to the telemetry firehose (RFC D4), so the
        // dedicated subscriber is a keyexpr necessity, not just a filter.
        let entity = KeyExpr::new("zensight/@v1/@catalog/state/entity/h-0123456789ab").unwrap();
        assert!(!telemetry.intersects(&entity));
        assert!(!is_telemetry_key(entity.as_str()));
        assert!(
            KeyExpr::new(all_entity_wildcard())
                .unwrap()
                .intersects(&entity)
        );
    }
}
