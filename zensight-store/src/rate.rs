//! Counter → rate, in one place.
//!
//! A counter is monotonic until the thing counting it restarts, and then it is
//! not: the next reading is smaller than the last. Every consumer that charts a
//! counter has to decide what to say about that sample, and until #904 each one
//! decided separately — the GUI alone carried three copies of the same
//! `last - prev` arithmetic, in `view/topology/model.rs`,
//! `view/specialized/netlink.rs` and (a near-relative) `parallax_health.rs`.
//!
//! They agreed, as it happens. The reason to have one is not that they
//! disagreed but that nothing *made* them agree, and the store had thrown away
//! the counter/gauge distinction that would have let it answer the question
//! once: a flattened `f64` cannot tell a counter reset from a gauge that fell.
//! Schema v3 keeps the kind, so the rate can be computed where the kind is
//! known — server-side in the historian, for every consumer at once.
//!
//! Two shapes, because there are two questions:
//!
//! - [`counter_rate`] — "what is the rate *right now*", from the last two
//!   samples. A reset yields one missing reading.
//! - [`rate_series`] — "what did the rate do over this window", for a whole
//!   series. A reset restarts the accumulation from zero, which is what
//!   Prometheus' `rate()` does and the only choice that does not either invent
//!   a negative rate or swallow the traffic that came after the restart.

use crate::Sample;

/// Bytes/sec from the last two samples of a monotonic counter series (#391).
/// `None` on short series, non-advancing clocks, or counter resets (negative
/// delta) — a reset yields one missing reading, not a bogus spike. Pure.
pub fn counter_rate(samples: &[Sample]) -> Option<f64> {
    let [.., prev, last] = samples else {
        return None;
    };
    let dt_ms = last.ts - prev.ts;
    if dt_ms <= 0 {
        return None;
    }
    let dv = last.value - prev.value;
    if dv < 0.0 {
        return None; // counter reset
    }
    Some(dv / (dt_ms as f64 / 1000.0))
}

/// Per-second rates across a whole counter series, one point per adjacent pair,
/// timestamped at the *later* sample of the pair (the rate is what happened up
/// to that instant, so stamping it at the earlier one would shift every chart
/// left by a bucket).
///
/// A **reset restarts from zero**: when the counter goes backwards the new
/// value is taken as the amount accumulated since the restart, so the pair
/// yields `new / dt` rather than `None` or a negative. That is Prometheus'
/// `rate()` rule, and it is the one that neither invents a negative rate nor
/// throws away the traffic that arrived after the process came back — which
/// matters most for exactly the counters that restart, like a sensor's own
/// `published_total`.
///
/// Pairs with a non-advancing clock are skipped: two samples in the same
/// millisecond carry no rate, and dividing by zero would carry a lie.
/// Returns an empty vec for a series shorter than two samples.
pub fn rate_series(samples: &[Sample]) -> Vec<Sample> {
    let mut out = Vec::with_capacity(samples.len().saturating_sub(1));
    for pair in samples.windows(2) {
        let (prev, last) = (&pair[0], &pair[1]);
        let dt_ms = last.ts - prev.ts;
        if dt_ms <= 0 {
            continue;
        }
        let dv = if last.value < prev.value {
            // Reset: everything on the clock now arrived since the restart.
            last.value
        } else {
            last.value - prev.value
        };
        out.push(Sample {
            ts: last.ts,
            value: dv / (dt_ms as f64 / 1000.0),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ts: i64, value: f64) -> Sample {
        Sample { ts, value }
    }

    /// Moved verbatim from `zensight/src/view/topology/model.rs` with the
    /// function it tests (#904) — the behaviour must not shift in the move.
    #[test]
    fn counter_rate_deltas_and_resets() {
        // 1000 bytes over 2 s → 500 B/s (uses the last two samples).
        assert_eq!(
            counter_rate(&[s(0, 0.0), s(1_000, 100.0), s(3_000, 1_100.0)]),
            Some(500.0)
        );
        // Counter reset → None, not a negative spike.
        assert_eq!(counter_rate(&[s(0, 5_000.0), s(1_000, 10.0)]), None);
        // Too short / non-advancing clock.
        assert_eq!(counter_rate(&[s(0, 1.0)]), None);
        assert_eq!(counter_rate(&[]), None);
        assert_eq!(counter_rate(&[s(5, 1.0), s(5, 2.0)]), None);
    }

    #[test]
    fn a_series_yields_one_rate_per_pair_stamped_at_the_later_sample() {
        let out = rate_series(&[s(0, 0.0), s(1_000, 100.0), s(3_000, 1_100.0)]);
        assert_eq!(out, vec![s(1_000, 100.0), s(3_000, 500.0)]);
    }

    /// The difference from [`counter_rate`], and the reason both exist: a
    /// window that contains a restart must still account for the traffic that
    /// arrived after it.
    #[test]
    fn a_reset_restarts_the_accumulation_from_zero() {
        // 5000 → 10 over one second: the process restarted and has since
        // counted 10, so 10 B/s — not a negative, and not a hole.
        assert_eq!(
            rate_series(&[s(0, 5_000.0), s(1_000, 10.0)]),
            vec![s(1_000, 10.0)]
        );
    }

    #[test]
    fn pairs_with_a_non_advancing_clock_carry_no_rate() {
        assert_eq!(rate_series(&[s(5, 1.0), s(5, 2.0)]), vec![]);
        // …and do not stop the pairs around them from being counted.
        let out = rate_series(&[s(0, 0.0), s(5, 1.0), s(5, 2.0), s(1_005, 1_002.0)]);
        assert_eq!(out, vec![s(5, 200.0), s(1_005, 1_000.0)]);
    }

    #[test]
    fn a_series_shorter_than_two_samples_has_no_rate() {
        assert_eq!(rate_series(&[s(0, 1.0)]), vec![]);
        assert_eq!(rate_series(&[]), vec![]);
    }
}
