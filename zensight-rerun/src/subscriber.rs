//! Zenoh subscriptions feeding the sink worker.
//!
//! Four channels, same split as the frontend/exporters (docs/KEYSPACE.md):
//! - telemetry `zensight/**` (or a narrowed `filters.key_expr`),
//! - alerts `zensight/*/@/alerts/*` — the telemetry wildcard can NEVER match
//!   `@`-verbatim chunks, so alerts need their own subscriber (pinned below),
//! - health `zensight/*/*/@/health`,
//! - entities `zensight/_meta/entity/**`, seeded by a one-shot GET on the
//!   correlator's `zensight/_meta/query/entities` queryable.

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

/// Whether a key carries a [`TelemetryPoint`]. Control/metadata channels do
/// not: any `@`-prefixed chunk (`.../@/...` control plane, the opaque
/// `.../@media/...` and `@pdns` planes) and `zensight/_meta/...`. Local copy
/// by design — the exporters each carry one too; shared extraction is
/// deferred (03-sink-design.md §2).
pub(crate) fn is_telemetry_key(key: &str) -> bool {
    !key.starts_with("zensight/_meta/") && !key.split('/').any(|chunk| chunk.starts_with('@'))
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

    // Entities: subscribe first, then seed — no gap; the worker's upsert is
    // idempotent so a doc seen twice is harmless.
    let entity_sub = session
        .declare_subscriber(&all_entity_wildcard())
        .await
        .map_err(|e| anyhow::anyhow!("failed to create entity subscriber: {e}"))?;

    match session
        .get(entities_query_key())
        .timeout(ENTITY_SEED_TIMEOUT)
        .await
    {
        Ok(replies) => {
            let mut seeded = 0usize;
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result()
                    && let Ok(entities) =
                        decode_auto::<Vec<HostEntity>>(&sample.payload().to_bytes())
                {
                    seeded += entities.len();
                    for entity in entities {
                        // Seeding happens before the drain loop: awaiting the
                        // (blocking) control channel here is safe and correct.
                        if tx_control
                            .send(ControlItem::Entity(Box::new(entity)))
                            .await
                            .is_err()
                        {
                            anyhow::bail!("control channel closed during entity seed");
                        }
                    }
                }
            }
            info!(seeded, "Entity seed complete");
        }
        Err(e) => debug!("entity seed query failed (no correlator?): {e}"),
    }

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

    info!("Subscriber started, waiting for samples...");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("Shutdown signal received, stopping subscriber");
                    break;
                }
            }

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
            "zensight/netlink/host/iface/eth0/rx_bytes"
        ));
        assert!(!is_telemetry_key("zensight/netlink/@/alerts/foo-00"));
        assert!(!is_telemetry_key("zensight/sysinfo/host1/@/health"));
        assert!(!is_telemetry_key("zensight/sysinfo/host1/@/errors"));
        assert!(!is_telemetry_key("zensight/_meta/sensors/snmp"));
        assert!(!is_telemetry_key("zensight/_meta/entity/host/h_00"));
        // The media/pdns planes ride `@`-prefixed chunks too (#359/#310).
        assert!(!is_telemetry_key(
            "zensight/netring/host01/@media/cam0/video/h264/main"
        ));
        assert!(!is_telemetry_key("zensight/@pdns/10-0-0-9"));
        // ...while stream *stats* are ordinary telemetry.
        assert!(is_telemetry_key("zensight/netring/host01/cam0/stats/fps"));
    }

    /// The telemetry wildcard `zensight/**` must NOT match `@/alerts/*` (Zenoh
    /// treats an `@`-prefixed chunk as verbatim) — which is exactly why the
    /// adapter declares a separate alerts subscriber. A regression here means
    /// alerts silently stop reaching the sink.
    #[test]
    fn alerts_need_their_own_subscription() {
        use zenoh::key_expr::KeyExpr;

        let alert = KeyExpr::new("zensight/netlink/@/alerts/foo-00").unwrap();
        let telemetry = KeyExpr::new(all_telemetry_wildcard()).unwrap();
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

    /// Same for health and entities: three more subscribers, three disjoint
    /// planes.
    #[test]
    fn health_and_entities_need_their_own_subscriptions() {
        use zenoh::key_expr::KeyExpr;

        let telemetry = KeyExpr::new(all_telemetry_wildcard()).unwrap();

        let health = KeyExpr::new("zensight/sysinfo/host1/@/health").unwrap();
        assert!(!telemetry.intersects(&health));
        assert!(
            KeyExpr::new(all_health_wildcard())
                .unwrap()
                .intersects(&health)
        );

        // `_meta` is matched by `zensight/**` on the wire — the entity plane is
        // excluded by is_telemetry_key, not by keyexpr non-intersection; the
        // dedicated subscriber exists to survive narrowed `filters.key_expr`.
        let entity = KeyExpr::new("zensight/_meta/entity/host/h_0123456789ab").unwrap();
        assert!(telemetry.intersects(&entity));
        assert!(!is_telemetry_key(entity.as_str()));
        assert!(
            KeyExpr::new(all_entity_wildcard())
                .unwrap()
                .intersects(&entity)
        );
    }
}
