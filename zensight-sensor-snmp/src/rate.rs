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

/// The largest delta a counter of this width can express — its modulus.
///
/// The plausibility ceiling has to **scale with the width and the interval**,
/// and for two releases it was a constant (#1074):
///
/// ```text
/// MAX_PLAUSIBLE_RATE = 1e10        // "80 Gbit/s in octets, with headroom"
/// largest 32-bit modular delta = 2^32 ≈ 4.29e9
/// ```
///
/// 4.29e9 is *below* 1e10 even at `dt = 1 s`, so **the guard could never fire
/// for any Counter32** — which is exactly the population that needs it. Only
/// 64-bit counters were protected, and `implausible_delta_rebaselines` tests
/// only those. A `clear counters` on `ifInErrors` (4e9 → 0) produced a modular
/// delta of 294 967 296 and published ≈ 4.9 M errors/s over a 60 s poll, which
/// fires `interface_errors`. The Counter32 columns with no HC sibling —
/// `ifIn/OutDiscards`, `ifIn/OutErrors` — are precisely the ones with no
/// 64-bit fallback to prefer instead.
fn modulus(is_32bit: bool) -> f64 {
    if is_32bit {
        (1u64 << 32) as f64
    } else {
        2f64.powi(64)
    }
}

/// An absolute physical ceiling: 1e10 octets/s covers an 80 Gbit/s link with
/// headroom. This is the old constant, and it is kept — it is a real bound, and
/// removing it would make the 64-bit guard *weaker* than it was.
const MAX_PLAUSIBLE_RATE: f64 = 1e10;

/// The tightest ceiling this observation can be held to.
///
/// Three bounds, and the smallest wins:
///
/// 1. **The width.** A rate above one full wrap per interval cannot have come
///    from counting. This is the bound that was missing: it is ~71.6 M/s for a
///    32-bit counter at a 60 s poll, where the constant alone was 1e10 — above
///    the largest 32-bit modular delta there is, so unreachable (#1074).
/// 2. **The absolute.** 1e10, unchanged, which is what protects 64-bit
///    counters — 2⁶⁴/dt is astronomically larger than any real rate.
/// 3. **The caller's physical bound**, when it knows one and *the counter went
///    backwards*. Only the poller knows both the link speed and what the OID
///    counts, and that is what catches a `clear counters` on an errors column:
///    4e9 → 0 over 60 s is a modular delta of 294 967 296, which is a perfectly
///    credible number of **octets** and an absurd number of **errors**. No
///    width- or speed-agnostic rule can tell those apart, so the party that can
///    is asked.
///
/// **The third applies to a backwards step only, and that restriction is the
/// point.** A backwards step is genuinely ambiguous — wrap or reset — and a
/// physical bound is the right tie-breaker. A *forward* delta is what the
/// device reported, and refusing it because `ifSpeed` says it is impossible
/// would be second-guessing the device with a number that is wrong all the
/// time: aggregate members, mis-declared virtual interfaces, and stale
/// ifSpeed on a re-negotiated link all report a speed the traffic exceeds.
/// Suppressing real traffic is a worse failure than the one this guards.
fn max_plausible_rate(is_32bit: bool, dt: f64, caller: Option<f64>) -> f64 {
    if dt <= 0.0 {
        return f64::INFINITY;
    }
    let mut ceiling = (modulus(is_32bit) / dt).min(MAX_PLAUSIBLE_RATE);
    if let Some(c) = caller.filter(|c| *c > 0.0) {
        ceiling = ceiling.min(c);
    }
    ceiling
}

/// The largest rate an octet counter on a link of this speed can show.
pub fn octet_ceiling(speed_bits: u64) -> f64 {
    speed_bits as f64 / 8.0
}

/// The largest rate a *frame*-counting column (packets, errors, discards) on a
/// link of this speed can show.
///
/// Bounded by frames, not by octets: the smallest Ethernet frame on the wire is
/// 64 bytes of frame plus 20 of preamble and inter-frame gap, so a link carries
/// at most `speed / (84 × 8)` of them a second — 1.488 Mpps at 1 Gb/s, the
/// number every line-rate datasheet quotes. An error counter cannot exceed the
/// frames that carried the errors.
pub fn frame_ceiling(speed_bits: u64) -> f64 {
    speed_bits as f64 / (84.0 * 8.0)
}

/// Above this speed, RFC 2233 §3.1.6 says a 32-bit octet counter wraps too fast
/// to poll reliably and the 64-bit `ifXTable` columns MUST be used.
///
/// snmp_exporter and LibreNMS both force HC above it. A 1 Gb/s interface wraps
/// `ifInOctets` every ~34 s; the default poll is 60, so **two wraps land in one
/// interval** and modular subtraction — correct across at most one — publishes
/// the residue as a plausible, *lower* number. A utilisation alert then never
/// fires on a pinned link.
pub const HC_REQUIRED_ABOVE_BPS: u64 = 20_000_000;

struct CounterSample {
    value: u64,
    at: Instant,
}

/// One derived rate, and what deriving it had to assume.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observed {
    /// Units per second over the measured interval.
    pub rate: f64,
    /// The counter went backwards and the delta was decoded as **one** modular
    /// wrap. Nothing in the arithmetic can tell one wrap from three — see
    /// [`wrap_risk`].
    pub wrapped: bool,
    /// Seconds actually elapsed since the previous sample.
    pub dt_secs: f64,
}

/// Whether a 32-bit octet counter on a link of this speed can wrap more than
/// once between polls, which modular subtraction cannot decode (#1074).
///
/// At 1 Gb/s `ifInOctets` wraps every ~34 s and the default poll is 60, so two
/// wraps land in one interval and the residue is published as a plausible,
/// *lower* number — a utilisation alert that never fires on a pinned link.
/// `None` when the speed is unknown: this is a claim, and an unknown speed
/// supports none.
pub fn wrap_risk(is_32bit: bool, speed_bits: Option<u64>, interval_secs: f64) -> Option<bool> {
    if !is_32bit {
        return Some(false); // 2^64 octets is not reachable by any link
    }
    let speed = speed_bits?;
    if speed == 0 || interval_secs <= 0.0 {
        return None;
    }
    // Octets the link could carry in one interval, against the counter's
    // modulus. `>=` because a wrap landing exactly on the boundary is already
    // ambiguous.
    let octets = (speed as f64 / 8.0) * interval_secs;
    Some(octets >= modulus(true) || speed > HC_REQUIRED_ABOVE_BPS)
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
    /// `is_32bit` selects the modular width for wrap-correct deltas, and — since
    /// #1074 — the plausibility ceiling, which is one full wrap per measured
    /// interval rather than a constant that no 32-bit delta could ever exceed.
    pub fn observe(&mut self, oid: &str, value: u64, is_32bit: bool, at: Instant) -> Option<f64> {
        self.observe_detailed(oid, value, is_32bit, at, None)
            .map(|o| o.rate)
    }

    /// [`observe`](Self::observe), with what the derivation had to assume.
    pub fn observe_detailed(
        &mut self,
        oid: &str,
        value: u64,
        is_32bit: bool,
        at: Instant,
        ceiling: Option<f64>,
    ) -> Option<Observed> {
        let prev = self
            .samples
            .insert(oid.to_string(), CounterSample { value, at })?;

        let dt = at.duration_since(prev.at).as_secs_f64();
        if dt <= 0.0 {
            return None;
        }

        let wrapped = value < prev.value;
        let delta = if is_32bit {
            u64::from((value as u32).wrapping_sub(prev.value as u32))
        } else {
            value.wrapping_sub(prev.value)
        };

        let rate = delta as f64 / dt;
        // The caller's bound decides an ambiguous backwards step; a forward
        // delta is held only to the width and the absolute ceiling.
        let physical = if wrapped { ceiling } else { None };
        if rate > max_plausible_rate(is_32bit, dt, physical) {
            // A reset dressed up as a giant wrapped delta: the new sample is
            // already stored, so the counter re-baselines; no rate this time.
            return None;
        }
        Some(Observed {
            rate,
            wrapped,
            dt_secs: dt,
        })
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

    /// The guard has to be reachable for a Counter32, and for two releases it
    /// was not (#1074).
    ///
    /// `MAX_PLAUSIBLE_RATE = 1e10` sat **above** the largest possible 32-bit
    /// modular delta (2³² ≈ 4.29e9), so no Counter32 could ever trip it. A
    /// `clear counters` on `ifInErrors` (4e9 → 0) yields a modular delta of
    /// 294 967 296 and published ≈ 4.9 M errors/s over a 60 s poll — which
    /// fires `interface_errors`. And those columns (`ifIn/OutErrors`,
    /// `ifIn/OutDiscards`) have no HC sibling to prefer instead.
    #[test]
    fn a_counter32_reset_rebaselines_instead_of_publishing_millions() {
        let mut tr = RateTracker::new();
        let start = t0();
        // A 1 Gb/s interface carries at most ~1.488 M frames/s, so it cannot
        // show more errors than that. That bound is the caller's to supply —
        // 294 967 296 units in a minute is a credible number of OCTETS and an
        // absurd number of ERRORS, and no width- or speed-agnostic rule can
        // tell them apart.
        let errors = Some(frame_ceiling(1_000_000_000));
        tr.observe_detailed("ifInErrors", 4_000_000_000, true, start, errors);
        // `clear counters`: 4e9 → 0 over a 60 s poll. Modular delta 294 967 296
        // → 4.9 M errors/s.
        assert_eq!(
            tr.observe_detailed(
                "ifInErrors",
                0,
                true,
                start + Duration::from_secs(60),
                errors
            ),
            None,
            "a reset must re-baseline, not publish 4.9 M errors/s"
        );
        // And the next interval is sane from the new baseline.
        let o = tr
            .observe_detailed(
                "ifInErrors",
                60,
                true,
                start + Duration::from_secs(120),
                errors,
            )
            .unwrap();
        assert!((o.rate - 1.0).abs() < 1e-6, "rate {}", o.rate);

        // The same delta on an OCTET counter of the same link is legitimate —
        // 4.9 MB/s on a gigabit link — and must still be published.
        let octets = Some(octet_ceiling(1_000_000_000));
        let mut tr = RateTracker::new();
        tr.observe_detailed("ifInOctets", 4_000_000_000, true, start, octets);
        assert!(
            tr.observe_detailed(
                "ifInOctets",
                0,
                true,
                start + Duration::from_secs(60),
                octets
            )
            .is_some(),
            "294 M octets in a minute is 4.9 MB/s: ordinary traffic"
        );

        // A FORWARD delta is never held to the physical bound, even one the
        // link "cannot" carry: ifSpeed is wrong all the time (aggregate
        // members, mis-declared virtual interfaces, a stale re-negotiated
        // link), and suppressing real traffic is worse than the failure this
        // guards against.
        let mut tr = RateTracker::new();
        let tiny = Some(1.0);
        tr.observe_detailed("ifInErrors", 0, true, start, tiny);
        assert!(
            tr.observe_detailed(
                "ifInErrors",
                50_000,
                true,
                start + Duration::from_secs(1),
                tiny
            )
            .is_some(),
            "the device says 50 000; a speed the device also reported does not \
             get to overrule it"
        );
    }

    /// The ceiling scales with the width and with the measured interval, which
    /// is what the constant could not do (#1074).
    #[test]
    fn the_ceiling_scales_with_the_width_and_the_interval() {
        // A 32-bit counter cannot legitimately exceed one wrap per interval…
        assert!((max_plausible_rate(true, 60.0, None) - (1u64 << 32) as f64 / 60.0).abs() < 1.0);
        // …which at a 60 s poll is ~71.6 M/s — far under the old 1e10, which is
        // precisely why the old guard could never fire for a Counter32.
        assert!(max_plausible_rate(true, 60.0, None) < 1e10);
        // 64-bit counters keep the absolute ceiling they always had. Dropping
        // it for 2^64/dt would have made the guard WEAKER than the constant.
        assert_eq!(max_plausible_rate(false, 60.0, None), 1e10);
        // A shorter interval raises the width bound, up to the absolute one:
        // one wrap in one second is legal arithmetic, however implausible.
        assert!(max_plausible_rate(true, 1.0, None) > max_plausible_rate(true, 60.0, None));
        // The caller's bound wins when it is tighter, and is ignored when it is
        // not — it is a bound, not an override.
        assert_eq!(max_plausible_rate(true, 60.0, Some(1_000.0)), 1_000.0);
        assert_eq!(
            max_plausible_rate(true, 60.0, Some(1e30)),
            max_plausible_rate(true, 60.0, None)
        );
        // 1 Gb/s: 125 MB/s of octets, 1.488 M frames/s.
        assert!((octet_ceiling(1_000_000_000) - 125_000_000.0).abs() < 1.0);
        assert!((frame_ceiling(1_000_000_000) - 1_488_095.0).abs() < 1.0);
    }

    /// A single wrap is still continuous — the fix must not turn every wrap
    /// into a re-baseline. This is the boundary the ceiling sits on.
    #[test]
    fn one_full_wrap_in_an_interval_is_still_a_rate() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", u64::from(u32::MAX) - 99, true, start);
        // 100 to the wrap + 400 after, over 1 s: nowhere near the ceiling.
        assert!(
            tr.observe("1.1", 400, true, start + Duration::from_secs(1))
                .is_some()
        );
    }

    /// Multi-wrap is not decodable, and a link fast enough to do it is marked
    /// rather than guessed at (#1074).
    #[test]
    fn a_fast_link_on_a_32_bit_counter_carries_wrap_risk() {
        // 1 Gb/s at a 60 s poll: ifInOctets wraps every ~34 s, so two wraps
        // land in one interval and the residue is a plausible LOWER number.
        assert_eq!(wrap_risk(true, Some(1_000_000_000), 60.0), Some(true));
        // 10 Mb/s: under RFC 2233's threshold, and 75 MB in a minute is well
        // inside 2^32 octets.
        assert_eq!(wrap_risk(true, Some(10_000_000), 60.0), Some(false));
        // A 64-bit counter is never at risk: 2^64 octets is not reachable.
        assert_eq!(wrap_risk(false, Some(100_000_000_000), 60.0), Some(false));
        // An unknown speed supports no claim either way — `None`, not `false`.
        assert_eq!(wrap_risk(true, None, 60.0), None);
        assert_eq!(wrap_risk(true, Some(0), 60.0), None);
    }

    /// `observe_detailed` says the counter went backwards, which is what a
    /// caller needs to decide whether the wrap was decodable.
    #[test]
    fn a_wrapped_observation_says_so() {
        let mut tr = RateTracker::new();
        let start = t0();
        tr.observe("1.1", u64::from(u32::MAX) - 99, true, start);
        let o = tr
            .observe_detailed("1.1", 400, true, start + Duration::from_secs(1), None)
            .unwrap();
        assert!(o.wrapped);
        assert_eq!(o.dt_secs, 1.0);

        tr.observe("1.2", 100, true, start);
        let o = tr
            .observe_detailed("1.2", 200, true, start + Duration::from_secs(1), None)
            .unwrap();
        assert!(!o.wrapped);
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
