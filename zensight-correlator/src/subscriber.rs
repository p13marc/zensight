//! Zenoh subscribers feeding the engine.
//!
//! Two AdvancedSubscribers on the evidence keyspace (host claims + name
//! observations). Each decodes its samples and forwards an [`EvidenceMsg`]
//! into the engine's mpsc.
//!
//! The evidence subscribers use `history()` (+ `detect_late_publishers`) so a
//! freshly-started correlator immediately receives the sensors' cached
//! self-reports — this is what makes the entity view stateless-recomputable
//! across restarts. Like the frontend's telemetry subscriber, they use an
//! **unbounded** channel so the startup history burst can't deadlock the session.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::{info, trace, warn};
use zenoh::Session;
use zenoh::sample::{Sample, SampleKind};
use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};
use zensight_common::{
    HostEvidence, NameObservation, all_evidence_wildcard, all_name_evidence_wildcard,
};

use crate::engine::EvidenceMsg;

/// Decode a payload as JSON first, then CBOR.
fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Option<T> {
    serde_json::from_slice(payload)
        .ok()
        .or_else(|| ciborium::from_reader(payload).ok())
}

/// Declare the evidence subscribers and run the forwarding loop until shutdown.
///
pub async fn run(
    session: Arc<Session>,
    tx: mpsc::Sender<EvidenceMsg>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Host evidence. `all_evidence_wildcard()` also matches the names subtree, so
    // this subscriber deliberately skips `/names/` keys (handled by the dedicated
    // names subscriber below) to avoid double-processing.
    let host_key = all_evidence_wildcard();
    info!(key = %host_key, "subscribing to host evidence");
    let host_sub = session
        .declare_subscriber(&host_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare host-evidence subscriber: {e}"))?;

    let names_key = all_name_evidence_wildcard();
    info!(key = %names_key, "subscribing to name observations");
    let names_sub = session
        .declare_subscriber(&names_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare name-evidence subscriber: {e}"))?;

    info!("evidence subscribers ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("shutdown signal received, stopping subscribers");
                    break;
                }
            }
            sample = host_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_host(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "host-evidence recv error"),
                }
            }
            sample = names_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_name(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "name-evidence recv error"),
                }
            }
        }
    }

    Ok(())
}

/// Whether a sample is a live (non-tombstone) `PUT`.
fn is_put(sample: &Sample) -> bool {
    sample.kind() == SampleKind::Put
}

/// Extract `(sensor, device)` from a v1 device-evidence key (used to resolve
/// a tombstone into the claim it withdraws).
fn parse_host_evidence_key(key: &str) -> Option<(String, String)> {
    // v1: zensight/v1/<origin>/state/<sensor>/evidence/device/<device>.
    // A `…/evidence/self` tombstone carries only the origin — the store keys
    // claims by the payload's source, so it just ages out by TTL instead.
    let mut chunks = key.split('/');
    if chunks.next() != Some("zensight") || chunks.next() != Some("v1") {
        return None;
    }
    let _origin = chunks.next()?;
    if chunks.next() != Some("state") {
        return None;
    }
    let sensor = chunks.next()?;
    if chunks.next() != Some("evidence") || chunks.next() != Some("device") {
        return None;
    }
    let device = chunks.next()?;
    if sensor.is_empty() || device.is_empty() || chunks.next().is_some() {
        return None;
    }
    Some((sensor.to_string(), device.to_string()))
}

async fn handle_host(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    // The names subtree is handled by its own subscriber.
    if key.contains("/evidence/names/") {
        return;
    }
    if !is_put(sample) {
        // Evidence tombstone (a `Delete`): drop that claim from the store now
        // instead of waiting for it to age out by TTL.
        if let Some((sensor, source)) = parse_host_evidence_key(key) {
            let _ = tx.send(EvidenceMsg::RemoveHost { sensor, source }).await;
        } else {
            trace!(key = %key, "ignoring malformed host-evidence tombstone");
        }
        return;
    }
    match decode::<HostEvidence>(&sample.payload().to_bytes()) {
        Some(ev) => {
            let _ = tx.send(EvidenceMsg::Host(Box::new(ev))).await;
        }
        None => warn!(key = %key, "failed to decode HostEvidence"),
    }
}

async fn handle_name(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    if !is_put(sample) {
        return;
    }
    match decode::<NameObservation>(&sample.payload().to_bytes()) {
        Some(obs) => {
            let _ = tx.send(EvidenceMsg::Name(obs)).await;
        }
        None => warn!(key = %key, "failed to decode NameObservation"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_host_evidence_key;

    #[test]
    fn parses_host_evidence_key() {
        assert_eq!(
            parse_host_evidence_key(
                "zensight/v1/h-3fa9c2d41b7e/state/netlink/evidence/device/host1"
            ),
            Some(("netlink".to_string(), "host1".to_string()))
        );
        // A MAC-slug device (third-party evidence) stays a single chunk.
        assert_eq!(
            parse_host_evidence_key(
                "zensight/v1/h-3fa9c2d41b7e/state/netring/evidence/device/aa-bb-cc-00-00-02"
            ),
            Some(("netring".to_string(), "aa-bb-cc-00-00-02".to_string()))
        );
        // The names subtree is not a device key.
        assert_eq!(
            parse_host_evidence_key(
                "zensight/v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-5"
            ),
            None
        );
        // A self-evidence tombstone carries only the origin — ages out by TTL.
        assert_eq!(
            parse_host_evidence_key("zensight/v1/h-3fa9c2d41b7e/state/netlink/evidence/self"),
            None
        );
        // Trailing chunks make it malformed.
        assert_eq!(
            parse_host_evidence_key(
                "zensight/v1/h-3fa9c2d41b7e/state/netlink/evidence/device/host1/extra"
            ),
            None
        );
    }
}
