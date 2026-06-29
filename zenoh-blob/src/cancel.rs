//! A cheap, cloneable cancellation flag for pausing/cancelling a download.
//!
//! Pause and cancel are the same mechanism at the transport layer: stop fetching
//! and leave the `.part` + sidecar on disk. The *caller* decides what it means —
//! a paused transfer keeps the partial and resumes later (the normal resume
//! path); a cancelled transfer additionally deletes the partial.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A shared cancellation flag. Clone it freely; setting it on any clone signals
/// the in-flight download to stop after the current chunk.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    /// Signal cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_state_across_clones() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }
}
