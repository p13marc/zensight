//! One unit's **sliding** restart window (#1083).
//!
//! Two rules count restarts over a window — `systemd-restart-storm` in
//! [`crate::alerts`] and the `max restarts / window_secs` expectation in
//! [`crate::sentinel`] — and both kept the same `{ start, base }` pair, whose
//! doc comments both said "sliding". Neither was.
//!
//! A `{ start, base }` pair is a **tumbling** window: when `window` elapses it
//! rebases wholesale, so the count snaps back to zero at fixed boundaries. A
//! unit restarting twice every 200 s against a 300 s window and a threshold of
//! 3 never fires — each tumble discards the first pair before the second
//! arrives — and a burst that straddles a boundary is split in half and reaches
//! the threshold in neither part. The rule exists to notice a restart loop, and
//! a restart loop is exactly the shape it could not see.
//!
//! The sensor observes a **counter**, not events, so a restart's instant is the
//! poll that first saw the count move. The window therefore holds one entry per
//! poll that saw an increase, and the count is the sum of the entries still
//! inside it — a faithful sliding window at poll resolution.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The most poll observations one unit's window will hold.
///
/// At most one entry is pushed per poll and entries expire out of the front, so
/// the natural bound is `window / poll_interval + 1` — a few dozen for the
/// shipped 15 s poll and 300 s window. This cap is the guard for a
/// pathologically short poll interval, not the normal mechanism: dropping the
/// *oldest* entry keeps the recent history, which is the half the threshold is
/// about.
const MAX_OBSERVATIONS: usize = 1024;

/// One unit's sliding restart window.
#[derive(Debug, Default)]
pub struct RestartWindow {
    /// `NRestarts` at the previous poll — the delta basis.
    prev: Option<u32>,
    /// `(poll instant, restarts seen at that poll)`, oldest first.
    events: VecDeque<(Instant, u32)>,
}

impl RestartWindow {
    /// Record this poll's counter reading and return the restarts inside
    /// `window`.
    ///
    /// The **first** sight of a unit only establishes the basis: a counter that
    /// already reads 40 because the unit has been flapping since last Tuesday
    /// is not something this process observed, and reporting it as a storm on
    /// the sensor's first sweep would be a lie about when it happened.
    ///
    /// A counter that goes **backwards** — `systemctl reset-failed`, a daemon
    /// reload — makes the prior history unattributable, so the window is
    /// cleared rather than folding a negative delta in.
    pub fn observe(&mut self, n_restarts: u32, now: Instant, window: Duration) -> u32 {
        match self.prev {
            None => {}
            Some(prev) if n_restarts < prev => self.events.clear(),
            Some(prev) if n_restarts > prev => {
                self.events.push_back((now, n_restarts - prev));
                if self.events.len() > MAX_OBSERVATIONS {
                    self.events.pop_front();
                }
            }
            Some(_) => {}
        }
        self.prev = Some(n_restarts);
        while let Some(&(t, _)) = self.events.front() {
            if now.duration_since(t) >= window {
                self.events.pop_front();
            } else {
                break;
            }
        }
        self.events.iter().map(|(_, n)| n).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(300);

    /// **The test that separates a sliding window from a tumbling one.**
    ///
    /// Two restarts every 200 s, threshold 3. Sliding: at t+400 both pairs are
    /// inside the trailing 300 s, so the count is 4 and the storm fires.
    /// Tumbling: the window rebased at t+300, discarding the first pair, so the
    /// count is 2 and it never fires — however long the loop runs.
    #[test]
    fn a_loop_that_straddles_the_boundary_is_seen() {
        let t0 = Instant::now();
        let mut w = RestartWindow::default();
        assert_eq!(
            w.observe(0, t0, WINDOW),
            0,
            "first sight only sets the basis"
        );
        assert_eq!(w.observe(2, t0 + Duration::from_secs(200), WINDOW), 2);
        assert_eq!(
            w.observe(4, t0 + Duration::from_secs(400), WINDOW),
            4,
            "a tumbling window would have rebased at t+300 and reported 2"
        );
    }

    /// Restarts age out of the trailing window on their own.
    #[test]
    fn restarts_age_out_of_the_window() {
        let t0 = Instant::now();
        let mut w = RestartWindow::default();
        w.observe(0, t0, WINDOW);
        assert_eq!(w.observe(3, t0 + Duration::from_secs(10), WINDOW), 3);
        assert_eq!(
            w.observe(3, t0 + Duration::from_secs(400), WINDOW),
            0,
            "the burst is older than the window and no longer counts"
        );
    }

    /// A restart that happened before this process started is not ours to
    /// report.
    #[test]
    fn a_counter_that_is_already_high_is_not_a_storm() {
        let t0 = Instant::now();
        let mut w = RestartWindow::default();
        assert_eq!(w.observe(40, t0, WINDOW), 0);
        assert_eq!(w.observe(40, t0 + Duration::from_secs(15), WINDOW), 0);
    }

    /// `systemctl reset-failed` or a daemon reload rewinds the counter, and the
    /// history before it cannot be attributed.
    #[test]
    fn a_counter_reset_clears_the_window() {
        let t0 = Instant::now();
        let mut w = RestartWindow::default();
        w.observe(0, t0, WINDOW);
        assert_eq!(w.observe(5, t0 + Duration::from_secs(10), WINDOW), 5);
        assert_eq!(w.observe(1, t0 + Duration::from_secs(20), WINDOW), 0);
        // …and the new basis is the rewound value, so the next increase counts
        // once rather than replaying the gap.
        assert_eq!(w.observe(2, t0 + Duration::from_secs(30), WINDOW), 1);
    }

    /// One restart per poll, which is what a real loop looks like, accumulates.
    #[test]
    fn one_restart_per_poll_accumulates_to_the_threshold() {
        let t0 = Instant::now();
        let mut w = RestartWindow::default();
        w.observe(0, t0, WINDOW);
        for i in 1..=3u32 {
            let seen = w.observe(i, t0 + Duration::from_secs(15 * i as u64), WINDOW);
            assert_eq!(seen, i, "poll {i}");
        }
    }

    /// The deque cannot grow without bound under a pathological poll rate.
    #[test]
    fn the_window_is_bounded() {
        let t0 = Instant::now();
        let mut w = RestartWindow::default();
        for i in 0..(MAX_OBSERVATIONS as u32 + 500) {
            // Same instant every time, so nothing ever ages out.
            w.observe(i, t0, WINDOW);
        }
        assert!(w.events.len() <= MAX_OBSERVATIONS);
    }
}
