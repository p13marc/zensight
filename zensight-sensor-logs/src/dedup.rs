//! Ingest-level repeat collapse (#546).
//!
//! A single screaming line (a flapping daemon, a full disk) can arrive
//! thousands of times a second. Left alone it burns ring capacity, rate budget,
//! and downstream attention one identical copy at a time. This is syslog's
//! classic "last message repeated N times", applied at the receiver: consecutive
//! identical `(source, message)` lines are folded into one record carrying a
//! `repeat_count` label.
//!
//! The collapse is **idle-gap based**: a run stays open while matching lines
//! keep arriving and is emitted once either a *different* line arrives or no
//! matching line has arrived for `window` (the idle gap). A lone line with no
//! follow-up is therefore delayed by at most `window` — which is why the whole
//! feature is opt-in ([`crate::config::IngestConfig::collapse_repeats`]).
//!
//! Only what reaches the ring/telemetry is collapsed; the `received` counter is
//! incremented per wire frame *before* this stage, so rollup totals still
//! reflect every copy.

use std::time::{Duration, Instant};

use crate::receiver::ReceivedMessage;

/// A run of identical lines held open for possible folding.
struct Pending {
    /// The first line of the run — emitted (with `count`) when the run closes.
    received: ReceivedMessage,
    /// How many identical lines the run has folded (≥1).
    count: u64,
    /// Emit the run once `now >= deadline` with no matching line — extended on
    /// each fold, so a continuous scream stays folded until it pauses.
    deadline: Instant,
}

/// Folds consecutive identical lines into one record + a repeat count (#546).
///
/// Feed every post-filter line through [`observe`](Self::observe); drive
/// [`flush_due`](Self::flush_due) on a timer to close a run that ended without a
/// following (different) line. Both return the line(s) to emit downstream, each
/// paired with its repeat count (`1` when nothing was folded).
pub struct RepeatCollapser {
    window: Duration,
    pending: Option<Pending>,
}

impl RepeatCollapser {
    /// `window` is the idle gap that closes a run (clamped to ≥1ms).
    pub fn new(window: Duration) -> Self {
        Self {
            window: window.max(Duration::from_millis(1)),
            pending: None,
        }
    }

    /// Observe one line. Returns the line to emit *now*, if any, with its repeat
    /// count — this is the previously-pending run that the new line closed (or
    /// the new line itself passing straight through when nothing was pending and
    /// no fold is possible). A folded (suppressed) line returns `None`.
    pub fn observe(
        &mut self,
        received: ReceivedMessage,
        now: Instant,
    ) -> Option<(ReceivedMessage, u64)> {
        let matches = self
            .pending
            .as_ref()
            .is_some_and(|p| now <= p.deadline && same_line(&p.received, &received));

        if matches {
            let p = self.pending.as_mut().expect("matches ⇒ pending is Some");
            p.count += 1;
            p.deadline = now + self.window;
            return None;
        }

        // The run (if any) ended: emit it, and start a fresh run with this line.
        let closed = self.pending.take().map(|p| (p.received, p.count));
        self.pending = Some(Pending {
            received,
            count: 1,
            deadline: now + self.window,
        });
        closed
    }

    /// If the open run's idle gap has elapsed, close and return it. Call on a
    /// timer so a run that ended without a trailing different line still emits.
    pub fn flush_due(&mut self, now: Instant) -> Option<(ReceivedMessage, u64)> {
        if self.pending.as_ref().is_some_and(|p| now >= p.deadline) {
            let p = self.pending.take().expect("checked Some");
            return Some((p.received, p.count));
        }
        None
    }

    /// The next instant [`flush_due`](Self::flush_due) could produce something,
    /// for sizing a timer. `None` when nothing is pending. (The binary uses a
    /// fixed flush tick; this is for adaptive callers and tests.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.deadline)
    }
}

/// Two lines are "the same" for collapse when they share the resolved host and
/// the exact message text. Keying on the resolved hostname (not the raw socket
/// address) means a device behind changing ephemeral ports — or several hosts
/// multiplexed over one unix socket — dedup by their real identity.
fn same_line(a: &ReceivedMessage, b: &ReceivedMessage) -> bool {
    a.resolved_hostname == b.resolved_hostname && a.message.message == b.message.message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::receiver::MessageSource;

    fn rec(host: &str, body: &str) -> ReceivedMessage {
        ReceivedMessage {
            message: parse(&format!("<14>{body}")).unwrap(),
            source: MessageSource::Unix,
            resolved_hostname: host.to_string(),
        }
    }

    #[test]
    fn folds_a_burst_into_one_record() {
        let mut c = RepeatCollapser::new(Duration::from_millis(100));
        let t0 = Instant::now();
        // 10k identical lines arriving fast (well within the window).
        for i in 0..10_000u64 {
            let emitted = c.observe(rec("h", "disk full"), t0 + Duration::from_micros(i));
            assert!(emitted.is_none(), "every line folds into the open run");
        }
        // Idle gap passes → one record with the full count.
        let (msg, count) = c
            .flush_due(t0 + Duration::from_millis(200))
            .expect("run flushes");
        assert_eq!(count, 10_000);
        assert!(msg.message.message.contains("disk full"));
    }

    #[test]
    fn a_different_line_closes_the_run() {
        let mut c = RepeatCollapser::new(Duration::from_millis(100));
        let t = Instant::now();
        assert!(c.observe(rec("h", "aaa"), t).is_none()); // first held
        assert!(c.observe(rec("h", "aaa"), t).is_none()); // folded (count 2)
        // A different line closes the "aaa" run (count 2) and holds "bbb".
        let (closed, count) = c.observe(rec("h", "bbb"), t).expect("run closes");
        assert_eq!(count, 2);
        assert_eq!(closed.message.message, "aaa");
    }

    #[test]
    fn distinct_sources_do_not_fold_together() {
        let mut c = RepeatCollapser::new(Duration::from_millis(100));
        let t = Instant::now();
        assert!(c.observe(rec("h1", "same"), t).is_none());
        // Same text, different host → not a fold; closes h1's run (count 1).
        let (closed, count) = c.observe(rec("h2", "same"), t).expect("closes h1");
        assert_eq!(count, 1);
        assert_eq!(closed.resolved_hostname, "h1");
    }

    #[test]
    fn window_expiry_splits_a_run() {
        let mut c = RepeatCollapser::new(Duration::from_millis(100));
        let t = Instant::now();
        assert!(c.observe(rec("h", "x"), t).is_none());
        // A matching line after the idle gap does NOT fold; it closes the first
        // run (which flush would also have emitted) and starts a new one.
        let (closed, count) = c
            .observe(rec("h", "x"), t + Duration::from_millis(200))
            .expect("stale run closes");
        assert_eq!(count, 1);
        assert_eq!(closed.message.message, "x");
    }

    #[test]
    fn nothing_pending_flushes_to_nothing() {
        let mut c = RepeatCollapser::new(Duration::from_millis(100));
        assert!(c.flush_due(Instant::now()).is_none());
        assert!(c.next_deadline().is_none());
    }
}
