//! Per-device counter→rate derivation with wrap and reset handling (#527).
//!
//! SNMP counters are lifetime totals; consumers want rates. This module keeps
//! the previous sample per (device, OID) and derives a per-second rate each
//! poll cycle, handling the two classic failure modes:
//!
//! - **Counter wrap**: modular subtraction in the counter's own width gives
//!   the correct delta across a single wrap (Counter32 wraps in ~5.7 min on a
//!   saturated 100 Mb/s link).
//! - **Counter reset** (agent restart, interface re-create): detected either
//!   by sysUpTime going backwards (clears *all* samples — one suppressed
//!   interval, no garbage rates) or by an implausibly large delta on one
//!   counter (re-baselines just that counter).

use std::collections::HashMap;
use std::time::Instant;

/// Rates above this are considered a counter reset in disguise, not traffic.
/// 1e10/s covers an 80 Gbit/s link counted in octets with headroom.
const MAX_PLAUSIBLE_RATE: f64 = 1e10;

struct CounterSample {
    value: u64,
    at: Instant,
}

/// Tracks previous counter samples for one device.
#[derive(Default)]
pub struct RateTracker {
    samples: HashMap<String, CounterSample>,
    last_uptime_ticks: Option<u32>,
}

impl RateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a poll cycle, handing in the freshly-read sysUpTime (TimeTicks).
    ///
    /// Returns `true` when the device rebooted / the agent restarted since
    /// the previous cycle (sysUpTime went backwards): all samples are
    /// dropped, so this cycle re-baselines and publishes no rates.
    ///
    /// A sysUpTime wrap (2^32 centiseconds ≈ 497 days) is indistinguishable
    /// from a reset here and costs one suppressed interval — acceptable.
    pub fn begin_cycle(&mut self, uptime_ticks: Option<u32>) -> bool {
        let reset = match (self.last_uptime_ticks, uptime_ticks) {
            (Some(prev), Some(now)) => now < prev,
            _ => false,
        };
        if uptime_ticks.is_some() {
            self.last_uptime_ticks = uptime_ticks;
        }
        if reset {
            self.samples.clear();
        }
        reset
    }

    /// Feed one counter observation; returns the per-second rate when a
    /// previous sample exists and the delta is plausible.
    ///
    /// `is_32bit` selects the modular width for wrap-correct deltas.
    pub fn observe(&mut self, oid: &str, value: u64, is_32bit: bool, at: Instant) -> Option<f64> {
        let prev = self
            .samples
            .insert(oid.to_string(), CounterSample { value, at })?;

        let dt = at.duration_since(prev.at).as_secs_f64();
        if dt <= 0.0 {
            return None;
        }

        let delta = if is_32bit {
            u64::from((value as u32).wrapping_sub(prev.value as u32))
        } else {
            value.wrapping_sub(prev.value)
        };

        let rate = delta as f64 / dt;
        if rate > MAX_PLAUSIBLE_RATE {
            // A reset dressed up as a giant wrapped delta: the new sample is
            // already stored, so the counter re-baselines; no rate this time.
            return None;
        }
        Some(rate)
    }

    /// Drop samples for OIDs not seen this cycle (vanished table rows).
    pub fn retain(&mut self, seen: &std::collections::HashSet<String>) {
        self.samples.retain(|oid, _| seen.contains(oid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_sample_yields_no_rate() {
        let mut tr = RateTracker::new();
        assert_eq!(tr.observe("1.1", 100, true, t0()), None);
    }

    #[test]
    fn steady_rate() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", 1_000, true, start);
        let rate = tr
            .observe("1.1", 11_000, true, start + Duration::from_secs(10))
            .unwrap();
        assert!((rate - 1_000.0).abs() < 1e-6);
    }

    #[test]
    fn counter32_wrap_is_continuous() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", u64::from(u32::MAX) - 99, true, start);
        // 100 to the wrap point + 400 after = 500 in 1 s.
        let rate = tr
            .observe("1.1", 400, true, start + Duration::from_secs(1))
            .unwrap();
        assert!((rate - 500.0).abs() < 1e-6, "rate {rate}");
    }

    #[test]
    fn counter64_wrap_is_continuous() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", u64::MAX - 4, false, start);
        let rate = tr
            .observe("1.1", 5, false, start + Duration::from_secs(1))
            .unwrap();
        assert!((rate - 10.0).abs() < 1e-6, "rate {rate}");
    }

    #[test]
    fn implausible_delta_rebaselines() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", u64::MAX / 2, false, start);
        // Counter reset to a small value: 64-bit modular delta is astronomical.
        assert_eq!(
            tr.observe("1.1", 10, false, start + Duration::from_secs(1)),
            None
        );
        // Next interval is sane again from the new baseline.
        let rate = tr
            .observe("1.1", 110, false, start + Duration::from_secs(2))
            .unwrap();
        assert!((rate - 100.0).abs() < 1e-6);
    }

    #[test]
    fn uptime_backwards_clears_everything() {
        let mut tr = RateTracker::new();
        let start = t0();
        assert!(!tr.begin_cycle(Some(500_000)));
        tr.observe("1.1", 1_000, true, start);

        // Reboot: uptime restarts near zero.
        assert!(tr.begin_cycle(Some(300)));
        // Sample was dropped → no rate, fresh baseline.
        assert_eq!(
            tr.observe("1.1", 2_000, true, start + Duration::from_secs(1)),
            None
        );

        assert!(!tr.begin_cycle(Some(1_300)));
        let rate = tr
            .observe("1.1", 3_000, true, start + Duration::from_secs(2))
            .unwrap();
        assert!((rate - 1_000.0).abs() < 1e-6);
    }

    #[test]
    fn missing_uptime_never_resets() {
        let mut tr = RateTracker::new();
        assert!(!tr.begin_cycle(None));
        assert!(!tr.begin_cycle(Some(100)));
        assert!(!tr.begin_cycle(None));
        assert!(!tr.begin_cycle(Some(200)));
    }

    #[test]
    fn vanished_rows_are_pruned() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", 100, true, start);
        tr.observe("1.2", 100, true, start);

        let seen: std::collections::HashSet<String> = ["1.1".to_string()].into();
        tr.retain(&seen);

        // 1.2 was pruned: no rate on its next observation.
        assert!(
            tr.observe("1.1", 200, true, start + Duration::from_secs(1))
                .is_some()
        );
        assert!(
            tr.observe("1.2", 200, true, start + Duration::from_secs(1))
                .is_none()
        );
    }
}
