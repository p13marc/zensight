//! Recompute engine.
//!
//! For commit 1 this is a skeleton: it drains the evidence channel and logs
//! received counts. Commit 3 replaces the body with the store + debounced
//! recompute + publish pipeline.

use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::info;
use zensight_common::{HostEvidence, NameObservation};

use crate::config::CorrelatorConfig;

/// One decoded input to the engine, produced by the subscribers.
#[derive(Debug, Clone)]
pub enum EvidenceMsg {
    /// A host-identity claim (`_meta/evidence/host/<sensor>/<source>`).
    Host(HostEvidence),
    /// A passive-DNS name observation (`_meta/evidence/names/<sensor>/<ip>`).
    Name(NameObservation),
    /// A device-liveness update (`zensight/<proto>/@/devices/<dev>/liveness`);
    /// `source` is the device id, `status` the reported status string.
    Liveness { source: String, status: String },
}

/// The correlation engine.
pub struct Engine {
    config: CorrelatorConfig,
    rx: mpsc::Receiver<EvidenceMsg>,
}

impl Engine {
    /// Create a new engine reading from `rx`.
    pub fn new(config: CorrelatorConfig, rx: mpsc::Receiver<EvidenceMsg>) -> Self {
        Self { config, rx }
    }

    /// Run until the shutdown signal fires. Skeleton: count received evidence.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        let mut host = 0u64;
        let mut name = 0u64;
        let mut liveness = 0u64;
        info!(
            evidence_ttl_secs = self.config.evidence_ttl_secs,
            recompute_debounce_ms = self.config.recompute_debounce_ms,
            "correlation engine started (skeleton)"
        );
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                msg = self.rx.recv() => {
                    match msg {
                        Some(EvidenceMsg::Host(_)) => host += 1,
                        Some(EvidenceMsg::Name(_)) => name += 1,
                        Some(EvidenceMsg::Liveness { .. }) => liveness += 1,
                        None => break,
                    }
                    info!(host, name, liveness, "evidence received");
                }
            }
        }
        info!(host, name, liveness, "correlation engine stopped");
        Ok(())
    }
}
