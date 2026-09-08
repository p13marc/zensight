//! `CounterTracker` — one rate derivation, with the elapsed time it was
//! actually measured over (#1152).
//!
//! **The problem it replaces.** Five places in the workspace kept their own
//! previous-value map and their own delta arithmetic, and no copy got both
//! halves right:
//!
//! | Copy | Measures elapsed | Handles a reset | Handles a wrap |
//! |---|---|---|---|
//! | `netlink/bandwidth.rs` | yes | yes | n/a |
//! | `systemd/map.rs::counter_bps` | caller's | yes | n/a |
//! | `snmp/rate.rs` | yes | rebaseline | yes (32-bit) |
//! | `sysinfo/collector.rs` (×4) | **no** | **no** | n/a |
//! | `container/poller.rs` baselines | n/a | yes | n/a |
//!
//! The sysinfo row is the bug this exists for (#1069): every derived rate there
//! divided by `poll_interval_secs`, the *nominal* period, while the loop runs
//! `collect_and_publish().await` and *then* sleeps the interval — so the true
//! period is `interval + collection_time`. Under load a 5 s tick takes 12 s and
//! `rx_rate` reads 2.4× the truth; `util_percent` reads 240 % and clamps to a
//! flat 100, so a disk at 40 % is charted saturated. The sensor *measures* the
//! error — `record_poll_duration` — and published it without using it.
//!
//! **Three rules, and why each is a rule.**
//!
//! - **Elapsed time travels with the sample.** A rate divided by what the
//!   scheduler was *asked* for is not wrong by a little under load; it is wrong
//!   by exactly the amount that makes the load interesting.
//! - **A backwards step yields no rate, and re-baselines.** A counter that
//!   fell was reset, and one missing reading is cheaper than a spike nobody can
//!   distinguish from a real one. The exception is a declared wrap width —
//!   below.
//! - **A wrap is only decodable if the width is declared.** Modular subtraction
//!   at 32 bits is correct across at most *one* wrap. Nothing in the arithmetic
//!   can tell one wrap from three, so the tracker reports the largest rate the
//!   width could legitimately produce and leaves the "is this plausible"
//!   judgement to the caller, which is the only party that knows the link speed
//!   (see `zensight-sensor-snmp`, #1074).

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

/// The width a counter wraps at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CounterWidth {
    /// 64-bit, or wide enough that a wrap is not a real event. A backwards
    /// step is a reset.
    #[default]
    Wide,
    /// 32-bit, as SNMP's `Counter32` and every un-extended `ifTable` column
    /// are. A backwards step is decoded as one modular wrap.
    Bits32,
}

impl CounterWidth {
    /// The largest delta this width can express — the modulus.
    fn modulus(self) -> u128 {
        match self {
            CounterWidth::Wide => 1u128 << 64,
            CounterWidth::Bits32 => 1u128 << 32,
        }
    }
}

/// One observation's outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Units per second over the measured interval.
    pub per_sec: f64,
    /// Seconds actually elapsed since the previous sample — not the interval
    /// the caller asked its scheduler for.
    pub elapsed_secs: f64,
    /// The delta was decoded as a modular wrap rather than a plain increase.
    /// Correct across at most one wrap; the caller decides whether more than
    /// one could have fitted in `elapsed_secs`.
    pub wrapped: bool,
}

/// Per-key counter state and the rate between consecutive samples.
///
/// `K` is whatever identifies the series to the caller: an interface name, an
/// OID, a `(pid, start_time)` pair. Nothing here interprets it.
#[derive(Debug)]
pub struct CounterTracker<K: Eq + Hash> {
    prev: HashMap<K, (u64, Instant)>,
    width: CounterWidth,
}

impl<K: Eq + Hash> Default for CounterTracker<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash> CounterTracker<K> {
    /// A tracker for counters wide enough that a wrap is not a real event.
    pub fn new() -> Self {
        CounterTracker {
            prev: HashMap::new(),
            width: CounterWidth::Wide,
        }
    }

    /// A tracker for counters of a declared width, which decodes a backwards
    /// step as a wrap rather than a reset.
    pub fn with_width(width: CounterWidth) -> Self {
        CounterTracker {
            prev: HashMap::new(),
            width,
        }
    }

    /// Record a sample and return the rate since the previous one.
    ///
    /// `None` on the first sample for a key, on a non-advancing clock, and on a
    /// backwards step at [`CounterWidth::Wide`] — in every case the sample is
    /// still stored, so the *next* observation has a baseline. A reset that
    /// silently kept the old baseline would publish one enormous rate and then
    /// look correct forever, which is the failure that is hardest to notice.
    pub fn observe(&mut self, key: K, value: u64, at: Instant) -> Option<Rate> {
        let prev = self.prev.insert(key, (value, at));
        let (prev_value, prev_at) = prev?;
        let elapsed_secs = at.duration_since(prev_at).as_secs_f64();
        if elapsed_secs <= 0.0 {
            return None;
        }
        let (delta, wrapped) = match value.checked_sub(prev_value) {
            Some(d) => (u128::from(d), false),
            None => match self.width {
                // A counter that fell was reset. One missing reading beats a
                // spike nobody can tell from a real one.
                CounterWidth::Wide => return None,
                // Modular subtraction at the declared width. Correct across at
                // most one wrap — and nothing here can tell one from three.
                CounterWidth::Bits32 => {
                    let m = self.width.modulus();
                    ((u128::from(value) + m - u128::from(prev_value)) % m, true)
                }
            },
        };
        Some(Rate {
            per_sec: delta as f64 / elapsed_secs,
            elapsed_secs,
            wrapped,
        })
    }

    /// The largest rate this width could legitimately produce over
    /// `elapsed_secs` — one full wrap, no more.
    ///
    /// A rate above it cannot have come from counting: it is a reset decoded as
    /// a wrap. The ceiling has to scale with the width and the interval, which
    /// is what a fixed constant could not do — `MAX_PLAUSIBLE_RATE = 1e10` in
    /// the SNMP sensor sat *above* the largest possible 32-bit modular delta
    /// (2³² ≈ 4.29e9), so the guard it existed to be could never fire for the
    /// counters that needed it most (#1074).
    pub fn max_plausible_rate(&self, elapsed_secs: f64) -> f64 {
        if elapsed_secs <= 0.0 {
            return f64::INFINITY;
        }
        self.width.modulus() as f64 / elapsed_secs
    }

    /// Forget a key — a device that went away, an interface that was removed.
    /// Without this the map is a slow leak on a host that churns.
    pub fn forget(&mut self, key: &K) {
        self.prev.remove(key);
    }

    /// Keep only the keys the caller still cares about.
    pub fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        self.prev.retain(|k, _| keep(k));
    }

    /// Keys currently tracked.
    pub fn len(&self) -> usize {
        self.prev.len()
    }

    /// Whether nothing is tracked yet.
    pub fn is_empty(&self) -> bool {
        self.prev.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The whole point: the divisor is what elapsed, not what was asked for.
    ///
    /// This is #1069 as a unit test. A 5 s poll that took 12 s used to publish
    /// `delta / 5`, which is 2.4× the truth — and the further behind the sensor
    /// falls, the more it overstates the load that put it there.
    #[test]
    fn the_rate_divides_by_the_measured_interval_not_the_nominal_one() {
        let t0 = Instant::now();
        let mut t = CounterTracker::new();
        assert!(t.observe("eth0", 1_000, t0).is_none(), "first sample");
        let r = t
            .observe("eth0", 13_000, t0 + Duration::from_secs(12))
            .expect("a rate");
        assert_eq!(r.elapsed_secs, 12.0);
        assert_eq!(r.per_sec, 1_000.0, "12000 bytes over 12 s, not over 5");
        assert!(!r.wrapped);
    }

    /// A counter that fell was reset: no rate this tick, and the next one is
    /// measured from the new baseline rather than from the pre-reset value.
    #[test]
    fn a_reset_costs_one_reading_and_re_baselines() {
        let t0 = Instant::now();
        let mut t = CounterTracker::new();
        t.observe("if", 4_000_000_000u64, t0);
        assert!(
            t.observe("if", 10, t0 + Duration::from_secs(60)).is_none(),
            "a backwards step is a reset, not a rate"
        );
        let r = t
            .observe("if", 6_010, t0 + Duration::from_secs(120))
            .expect("the next tick is measured from the new baseline");
        assert_eq!(r.per_sec, 100.0);
    }

    /// At a declared 32-bit width the same backwards step is a wrap, decoded
    /// modularly and flagged.
    #[test]
    fn a_declared_width_decodes_a_wrap_instead_of_re_baselining() {
        let t0 = Instant::now();
        let mut t = CounterTracker::with_width(CounterWidth::Bits32);
        t.observe("if", u32::MAX as u64 - 99, t0);
        let r = t
            .observe("if", 100, t0 + Duration::from_secs(2))
            .expect("a wrap is a rate");
        // 99 to the top, the top itself, then 100 more: 200 over two seconds.
        assert_eq!(r.per_sec, 100.0);
        assert!(r.wrapped);
    }

    /// The ceiling scales with the width and the interval, which is what a
    /// fixed constant could not do (#1074).
    #[test]
    fn the_plausibility_ceiling_scales_with_the_width_and_the_interval() {
        let wide: CounterTracker<&str> = CounterTracker::new();
        let narrow: CounterTracker<&str> = CounterTracker::with_width(CounterWidth::Bits32);
        // A 32-bit counter cannot legitimately exceed one wrap per interval.
        assert_eq!(narrow.max_plausible_rate(60.0), (1u64 << 32) as f64 / 60.0);
        // ~71.6 M/s — well under the 1e10 constant that made the old guard
        // unreachable for every Counter32.
        assert!(narrow.max_plausible_rate(60.0) < 1e10);
        assert!(wide.max_plausible_rate(60.0) > 1e10);
    }

    /// A clock that did not advance carries no rate: dividing by zero would
    /// carry a lie, and two samples with one timestamp are one sample.
    #[test]
    fn a_non_advancing_clock_carries_no_rate() {
        let t0 = Instant::now();
        let mut t = CounterTracker::new();
        t.observe("x", 1, t0);
        assert!(t.observe("x", 2, t0).is_none());
    }

    /// Keys are forgettable, or the map is a slow leak on a host that churns
    /// interfaces, containers or processes.
    #[test]
    fn keys_can_be_dropped_and_retained() {
        let t0 = Instant::now();
        let mut t = CounterTracker::new();
        for k in ["a", "b", "c"] {
            t.observe(k, 1, t0);
        }
        assert_eq!(t.len(), 3);
        t.forget(&"b");
        assert_eq!(t.len(), 2);
        t.retain(|k| *k == "a");
        assert_eq!(t.len(), 1);
        assert!(!t.is_empty());
    }
}
