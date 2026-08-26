//! Whether an exporter's ingest pipeline is actually alive (#757).
//!
//! # The failure this exists to make visible
//!
//! Both exporters ran their Zenoh subscriber in a spawned task and, on a fatal
//! error, logged it and let the process live:
//!
//! ```ignore
//! if let Err(e) = subscriber.run(shutdown).await {
//!     error!("Subscriber error: {}", e);
//! }
//! // ... and main carries on
//! ```
//!
//! So a failed session, an invalid key expression or a dropped bus left the
//! process serving an empty `/metrics` and a **200 `/health`** forever. To
//! everything watching — a load balancer, a Kubernetes probe, an operator
//! reading a dashboard — that is indistinguishable from "connected, no data
//! yet". A monitoring component that reports healthy while it monitors nothing
//! is worse than one that is plainly down.
//!
//! # The three states, and why not two
//!
//! `Starting` is not `Live`. An exporter that has connected but seen no
//! telemetry is *working*; one that never connected is not, and collapsing
//! them would either fail readiness for every cold start or pass it for a dead
//! pipeline. `/ready` already distinguishes "no data yet"; this distinguishes
//! "no pipeline".

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// The ingest pipeline's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Connecting, or connected and not yet carrying data.
    Starting,
    /// Subscribed and receiving.
    Live,
    /// The pipeline died. The process should stop reporting healthy, and
    /// should exit non-zero so a supervisor restarts it.
    Failed,
}

/// A cheap, shareable pipeline-health flag.
#[derive(Debug, Clone, Default)]
pub struct PipelineHealth(Arc<AtomicU8>);

const STARTING: u8 = 0;
const LIVE: u8 = 1;
const FAILED: u8 = 2;

impl PipelineHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the pipeline as receiving.
    ///
    /// Deliberately ignored once `Failed`: recovery is a restart, not a flag
    /// flip, and a pipeline that flapped back to healthy without reconnecting
    /// would hide the failure it just had.
    pub fn set_live(&self) {
        let _ = self
            .0
            .compare_exchange(STARTING, LIVE, Ordering::Release, Ordering::Relaxed);
    }

    /// Mark the pipeline as dead. Terminal.
    pub fn set_failed(&self) {
        self.0.store(FAILED, Ordering::Release);
    }

    pub fn get(&self) -> Health {
        match self.0.load(Ordering::Acquire) {
            LIVE => Health::Live,
            FAILED => Health::Failed,
            _ => Health::Starting,
        }
    }

    /// Whether the process should still claim to be healthy.
    pub fn is_healthy(&self) -> bool {
        self.get() != Health::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_healthy_but_not_live() {
        let h = PipelineHealth::new();
        assert_eq!(h.get(), Health::Starting);
        assert!(
            h.is_healthy(),
            "a cold start is not a failure — /ready reports the no-data-yet case"
        );
    }

    #[test]
    fn failure_is_terminal() {
        let h = PipelineHealth::new();
        h.set_live();
        assert_eq!(h.get(), Health::Live);

        h.set_failed();
        assert_eq!(h.get(), Health::Failed);
        assert!(!h.is_healthy());

        // Recovery is a restart. A flag flip back to healthy would hide the
        // failure that just happened.
        h.set_live();
        assert_eq!(h.get(), Health::Failed, "failure must be terminal");
    }

    #[test]
    fn the_flag_is_shared_across_clones() {
        let a = PipelineHealth::new();
        let b = a.clone();
        b.set_failed();
        assert!(!a.is_healthy(), "clones share one flag");
    }
}
