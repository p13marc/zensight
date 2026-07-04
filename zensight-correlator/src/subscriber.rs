//! Zenoh subscribers feeding the engine.
//!
//! Two AdvancedSubscribers on the evidence keyspace (host claims + name
//! observations) plus, optionally, a plain subscriber on device liveness. Each
//! decodes its samples and forwards an [`EvidenceMsg`] into the engine's mpsc.
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
    DeviceLiveness, HostEvidence, NameObservation, all_evidence_wildcard, all_liveness_wildcard,
    all_name_evidence_wildcard,
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
/// `status_from_liveness` gates the extra device-liveness subscription.
pub async fn run(
    session: Arc<Session>,
    tx: mpsc::Sender<EvidenceMsg>,
    status_from_liveness: bool,
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

    // Device liveness → entity status. Plain subscriber (no history needed).
    let liveness_sub = if status_from_liveness {
        let key = all_liveness_wildcard();
        info!(key = %key, "subscribing to device liveness");
        match session
            .declare_subscriber(&key)
            .with(flume::unbounded())
            .await
        {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "failed to declare liveness subscriber; status disabled");
                None
            }
        }
    } else {
        None
    };

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
            sample = async { liveness_sub.as_ref().unwrap().recv_async().await },
                if liveness_sub.is_some() =>
            {
                match sample {
                    Ok(sample) => handle_liveness(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "liveness recv error"),
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

async fn handle_host(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    // The names subtree is handled by its own subscriber.
    if key.contains("/evidence/names/") {
        return;
    }
    if !is_put(sample) {
        // TODO(commit 3): evidence tombstones drop a claim from the store.
        trace!(key = %key, "ignoring non-PUT host-evidence sample");
        return;
    }
    match decode::<HostEvidence>(&sample.payload().to_bytes()) {
        Some(ev) => {
            let _ = tx.send(EvidenceMsg::Host(ev)).await;
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

async fn handle_liveness(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    if !is_put(sample) {
        return;
    }
    let key = sample.key_expr().as_str();
    match decode::<DeviceLiveness>(&sample.payload().to_bytes()) {
        Some(dl) => {
            let _ = tx
                .send(EvidenceMsg::Liveness {
                    source: dl.device,
                    status: dl.status.to_string(),
                })
                .await;
        }
        None => trace!(key = %key, "failed to decode DeviceLiveness"),
    }
}
