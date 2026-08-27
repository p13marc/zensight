//! Receiver-driven tier selection (#720): the viewer moves *itself* down a
//! rung when its link degrades, and back up only after sustained recovery.
//!
//! # The viewer changes its subscription; it never re-tunes an encoder
//!
//! RFC 07 §1.2 is normative on this: a producer MUST NOT re-tune a shared tier
//! from one consumer's report. Two operators on different links watch the same
//! camera, and one of them asking for less must not degrade the other. So the
//! only lever here is *which `<tier>` key this viewer subscribes to* — which is
//! what the tier ladder was built for (#494, #497, #502, #507), and needs no
//! new wire surface at all.
//!
//! # Why a lower tier is the right lever, and not just a cheaper one
//!
//! The #713 measurement (`docs/plans/adaptive-media/loss-measurement.md`) found
//! that on `quic/…?mixed_rel=1`, loss is amplified by **access-unit size**:
//! Zenoh fragments an access unit across datagrams and defragmentation is
//! all-or-nothing, so at 1 % packet loss an 842 B unit was lost 1.5 % of the
//! time, a 34 KB unit 20 %, and a 136 KB unit 41 %. Halving the bytes per frame
//! roughly halves the probability that a frame is lost at all. That is a
//! stronger justification for downgrading than "it uses less bandwidth", and it
//! is why the controller acts on loss rather than treating loss as something
//! only a wider pipe can fix.
//!
//! On today's `tcp/` deployments the same measurement found congestion showing
//! up as multi-second **frame age** with the sensor's `stats/drops` at zero, so
//! age is a first-class input here and not a secondary one.
//!
//! # Why the viewer's own sheds are not an input
//!
//! `dropped_frames` looks like the most direct evidence there is — the tile
//! saying "I threw this away". It is deliberately not read here, because every
//! shed cause a *downgrade* could fix is already one of the three inputs, and
//! the shed is the symptom rather than the cause:
//!
//! - a deadline shed means frame age was over the limit → **age** already says
//!   so, from the same report;
//! - a queue-full shed means the decoder is behind → **queue depth** already
//!   says so;
//! - an unsynced shed (waiting for the first IDR) and a preview backlog shed
//!   are about this tile starting up, and a lower tier does not help either.
//!
//! Adding sheds as a fourth input would double-count the first two and let the
//! third move a tier for a reason a tier cannot change. The live proof is the
//! zero-deadline case in `zensight/tests/media_receiver_live.rs`: 26 of 27
//! frames shed with a *measured frame age of 0.4 ms*. That is a configuration
//! saying "nothing is ever fresh enough", not a link the ladder can rescue.
//!
//! # Anti-flapping is most of the design
//!
//! A switch is not free: it closes a profile, opens another, rebuilds a decoder
//! and costs a keyframe. A controller that flaps is worse than no controller,
//! so every threshold below is *two* numbers and never one comparison flipped:
//!
//! - separate [`DOWN_LOSS_PCT`] / [`UP_LOSS_PCT`] (and the same for age and
//!   queue), leaving a band in which the answer is "stay put";
//! - [`MIN_DWELL`] in a tier before any further move;
//! - [`COOLDOWN`] after a switch during which reports are **discarded, not
//!   merely ignored** — the first reports after a switch describe the decoder
//!   rebuild and its resync keyframe, and folding them into the average teaches
//!   the controller that switching causes the problem switching just fixed;
//! - downgrade on the first degraded window, upgrade only after
//!   [`UP_SUSTAIN`] of continuous health.
//!
//! And the human always wins: an operator's explicit tier click pins the
//! stream, and only an explicit release un-pins it.

use std::time::{Duration, Instant};

use zensight_common::stream::{MediaReceiverReport, TierSpec};

/// Loss (percent of frames the receiver inferred missing) that triggers a
/// downgrade. Chosen against #713's burst statistics rather than a round
/// number: at 1 % packet loss the measured per-frame loss for a mid-size access
/// unit was ~20 %, and isolated single-frame gaps at 1–2 % were invisible in
/// the picture. 4 % is above the noise of a healthy link and well below the
/// point where gaps become bursts of three or more.
pub const DOWN_LOSS_PCT: f32 = 4.0;
/// Loss below which a link counts as healthy enough to try a higher tier.
/// Deliberately far below [`DOWN_LOSS_PCT`] — the gap *is* the hysteresis.
pub const UP_LOSS_PCT: f32 = 0.5;

/// Fraction of the viewer's frame-age deadline (#716) above which the link is
/// judged degraded. Not 1.0: at the deadline the tile is already shedding, and
/// a controller that waits for the symptom it is meant to prevent is late.
pub const DOWN_AGE_FRACTION: f32 = 0.75;
/// Fraction of the deadline below which frame age counts as healthy.
pub const UP_AGE_FRACTION: f32 = 0.30;

/// Fraction of the decode queue's capacity above which the *decoder*, not the
/// link, is the bottleneck — a downgrade helps there too, since a smaller tier
/// is cheaper to decode.
pub const DOWN_QUEUE_FRACTION: f32 = 0.60;
/// Fraction below which the decoder is comfortably keeping up.
pub const UP_QUEUE_FRACTION: f32 = 0.20;

/// Minimum time in a tier before any further move, up or down.
pub const MIN_DWELL: Duration = Duration::from_secs(12);
/// After a switch, reports covering this window are discarded outright.
/// Three report cadences (#718 reports every 3 s), which is about how long a
/// rebuilt tile takes to reach steady state.
pub const COOLDOWN: Duration = Duration::from_secs(9);
/// How long health must hold before an upgrade is attempted.
pub const UP_SUSTAIN: Duration = Duration::from_secs(30);

/// EWMA weight for the newest window. High enough that a genuinely bad link is
/// acted on within two or three reports, low enough that one unlucky window
/// does not move a tier on its own.
const ALPHA: f32 = 0.45;

/// What the controller decided this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Stay on this tier.
    Hold,
    /// The link is degraded: drop a rung.
    Down,
    /// Health has held for [`UP_SUSTAIN`]: try a rung up.
    Up,
}

/// One window's measurements, derived from two consecutive reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signals {
    /// Frames inferred missing, as a percentage of frames the window should
    /// have carried.
    pub loss_pct: f32,
    /// Median frame age at the end of the window. `None` is *not asked* (the
    /// samples arrived unstamped), never zero — RFC 07 §1.3.
    pub age_ms: Option<f32>,
    /// Decode queue depth at the end of the window; `None` for a tile with no
    /// queue to report.
    pub queue: Option<u32>,
}

/// Derive one window from a consecutive report pair.
///
/// `None` when the pair cannot be differenced:
///
/// - a different `consumer_id` — the counters are cumulative *per consumer*, so
///   across a reopen (which is exactly what a tier switch causes) the
///   difference is meaningless and would read as a huge negative;
/// - counters that went backwards, which means the same thing and should never
///   be papered over with a saturating subtraction;
/// - an empty window, which cannot produce a rate.
pub fn signals(prev: &MediaReceiverReport, last: &MediaReceiverReport) -> Option<Signals> {
    if prev.consumer_id != last.consumer_id || last.interval_ms == 0 {
        return None;
    }
    let received = last.received_frames.checked_sub(prev.received_frames)?;
    let lost = last.lost_frames.checked_sub(prev.lost_frames)?;
    let offered = received + lost;
    if offered == 0 {
        return None;
    }
    Some(Signals {
        loss_pct: 100.0 * lost as f32 / offered as f32,
        age_ms: last.frame_age_ms,
        queue: last.decoder_queue_depth,
    })
}

/// Per-stream controller state. Lives on the *view* state, not on the tile:
/// a tier switch replaces the tile, and a controller that died with the tile
/// would forget its dwell and cooldown at the exact moment they matter.
#[derive(Debug)]
pub struct TierController {
    loss: f32,
    age: Option<f32>,
    queue: Option<f32>,
    seeded: bool,
    tier_since: Instant,
    switched_at: Option<Instant>,
    healthy_since: Option<Instant>,
    pinned: bool,
}

impl TierController {
    /// A controller for a tile that has just opened on some tier.
    pub fn new(now: Instant) -> Self {
        Self {
            loss: 0.0,
            age: None,
            queue: None,
            seeded: false,
            tier_since: now,
            switched_at: None,
            healthy_since: None,
            pinned: false,
        }
    }

    /// Whether an operator has pinned this stream's tier.
    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// An operator chose a tier by hand. The controller stops deciding until
    /// they say otherwise — "never fight the human" is the rule, and a
    /// controller that quietly moved a tier back after a deliberate click would
    /// be indistinguishable from a bug.
    pub fn pin(&mut self, now: Instant) {
        self.pinned = true;
        self.note_switch(now);
    }

    /// The operator handed control back.
    pub fn unpin(&mut self, now: Instant) {
        self.pinned = false;
        // Not a fresh switch, but the measurements that accumulated while
        // pinned describe a tier the controller did not choose; start clean
        // rather than act on them.
        self.seeded = false;
        self.healthy_since = None;
        self.tier_since = now;
    }

    /// Record that the tile just switched tier (by whatever cause).
    pub fn note_switch(&mut self, now: Instant) {
        self.tier_since = now;
        self.switched_at = Some(now);
        self.seeded = false;
        self.healthy_since = None;
    }

    /// Fold one window in and say what to do.
    ///
    /// `deadline` is the viewer's frame-age deadline (#716); `None` disables the
    /// age input entirely rather than substituting a default — an unset
    /// deadline means the operator asked for no latency policy, and inventing
    /// one here would be the same "absent is not zero" mistake the report type
    /// is careful about.
    pub fn observe(
        &mut self,
        s: &Signals,
        deadline: Option<Duration>,
        queue_cap: u32,
        now: Instant,
    ) -> Move {
        // Discarded, not merely ignored: a report covering the rebuild after a
        // switch describes the switch, not the link.
        if self
            .switched_at
            .is_some_and(|at| now.duration_since(at) < COOLDOWN)
        {
            return Move::Hold;
        }
        self.switched_at = None;
        self.fold(s);
        if self.pinned || now.duration_since(self.tier_since) < MIN_DWELL {
            return Move::Hold;
        }

        let age_over = |f: f32| {
            deadline
                .zip(self.age)
                .is_some_and(|(d, age)| age > f * d.as_millis() as f32)
        };
        let age_under = |f: f32| {
            deadline
                .zip(self.age)
                .is_none_or(|(d, age)| age < f * d.as_millis() as f32)
        };
        let queue_frac = |f: f32| self.queue.map(|q| (q, f * queue_cap.max(1) as f32));

        let degraded = self.loss > DOWN_LOSS_PCT
            || age_over(DOWN_AGE_FRACTION)
            || queue_frac(DOWN_QUEUE_FRACTION).is_some_and(|(q, lim)| q > lim);
        if degraded {
            self.healthy_since = None;
            return Move::Down;
        }

        let healthy = self.loss < UP_LOSS_PCT
            && age_under(UP_AGE_FRACTION)
            && queue_frac(UP_QUEUE_FRACTION).is_none_or(|(q, lim)| q < lim);
        if !healthy {
            // The band between the two thresholds. Not "nearly healthy" —
            // undecided, and an upgrade must start its clock again.
            self.healthy_since = None;
            return Move::Hold;
        }
        let since = *self.healthy_since.get_or_insert(now);
        if now.duration_since(since) >= UP_SUSTAIN {
            Move::Up
        } else {
            Move::Hold
        }
    }

    fn fold(&mut self, s: &Signals) {
        if !self.seeded {
            self.loss = s.loss_pct;
            self.age = s.age_ms;
            self.queue = s.queue.map(|q| q as f32);
            self.seeded = true;
            return;
        }
        self.loss += ALPHA * (s.loss_pct - self.loss);
        // An unmeasured input does not decay the average towards zero; it
        // simply adds nothing. "Not asked" is not "asked and got 0".
        if let Some(a) = s.age_ms {
            self.age = Some(self.age.map_or(a, |cur| cur + ALPHA * (a - cur)));
        }
        if let Some(q) = s.queue.map(|q| q as f32) {
            self.queue = Some(self.queue.map_or(q, |cur| cur + ALPHA * (q - cur)));
        }
    }

    /// The smoothed loss the last decision used, for the health surface.
    pub fn smoothed_loss_pct(&self) -> Option<f32> {
        self.seeded.then_some(self.loss)
    }
}

/// Order a stream's tiers worst-to-best.
///
/// By `bitrate_kbps`, not by the order the catalogue happens to list them in:
/// the ladder's *rung order* is a property of the tiers, and reading it off an
/// array's order would make a config file's formatting load-bearing. Ties break
/// on fps then on height, so two tiers at the same bitrate still order.
fn ladder(tiers: &[TierSpec]) -> Vec<&TierSpec> {
    let mut v: Vec<&TierSpec> = tiers.iter().collect();
    v.sort_by_key(|t| (t.bitrate_kbps, t.fps, t.max_height.unwrap_or(u32::MAX)));
    v
}

/// The tier one rung from `current` in the direction of `mv`.
///
/// `None` at the ends of the ladder, and `None` for [`Move::Hold`] — the caller
/// sends nothing, and in particular does **not** reset the dwell timer for a
/// move that never happened.
pub fn next_tier(tiers: &[TierSpec], current: &str, mv: Move) -> Option<String> {
    let rungs = ladder(tiers);
    let at = rungs.iter().position(|t| t.name == current)?;
    let to = match mv {
        Move::Hold => return None,
        Move::Down => at.checked_sub(1)?,
        Move::Up => at + 1,
    };
    rungs.get(to).map(|t| t.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(consumer: &str, received: u64, lost: u64, age: Option<f32>) -> MediaReceiverReport {
        MediaReceiverReport {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            tier: Some("high".into()),
            consumer_id: consumer.into(),
            interval_ms: 3000,
            received_frames: received,
            lost_frames: lost,
            dropped_frames: 0,
            decoded_frames: received,
            last_sequence: received + lost,
            interarrival_jitter_ms: None,
            frame_age_ms: age,
            frame_age_max_ms: age,
            decoder_queue_depth: None,
            last_keyframe_sequence: None,
            since_last_keyframe_ms: None,
        }
    }

    fn tiers() -> Vec<TierSpec> {
        vec![
            TierSpec {
                name: "high".into(),
                max_height: None,
                fps: 30,
                bitrate_kbps: 4000,
            },
            TierSpec {
                name: "low".into(),
                max_height: Some(240),
                fps: 10,
                bitrate_kbps: 400,
            },
            TierSpec {
                name: "medium".into(),
                max_height: Some(480),
                fps: 20,
                bitrate_kbps: 1200,
            },
        ]
    }

    /// The rung order comes from the tiers, not from the array's order — the
    /// fixture above is deliberately shuffled.
    #[test]
    fn the_ladder_is_ordered_by_what_a_tier_costs_not_by_config_order() {
        let t = tiers();
        assert_eq!(next_tier(&t, "high", Move::Down).as_deref(), Some("medium"));
        assert_eq!(next_tier(&t, "medium", Move::Down).as_deref(), Some("low"));
        assert_eq!(next_tier(&t, "medium", Move::Up).as_deref(), Some("high"));
        assert_eq!(next_tier(&t, "low", Move::Down), None, "already lowest");
        assert_eq!(next_tier(&t, "high", Move::Up), None, "already highest");
        assert_eq!(next_tier(&t, "high", Move::Hold), None);
        assert_eq!(next_tier(&t, "nonesuch", Move::Down), None);
    }

    #[test]
    fn a_window_across_a_reopen_is_not_a_window() {
        // A tier switch mints a new consumer id and restarts the counters. The
        // difference across that boundary is not a small error, it is a
        // fabricated 100% loss on the tier we just moved to.
        let a = report("zs-1-1", 100, 0, Some(20.0));
        let b = report("zs-1-2", 5, 0, Some(20.0));
        assert!(signals(&a, &b).is_none());
        // Same consumer, counters gone backwards: same conclusion.
        let c = report("zs-1-1", 50, 0, Some(20.0));
        assert!(signals(&a, &c).is_none());
    }

    #[test]
    fn a_window_with_no_frames_produces_no_signal() {
        let a = report("zs-1-1", 100, 3, Some(20.0));
        let b = report("zs-1-1", 100, 3, Some(20.0));
        assert!(signals(&a, &b).is_none());
    }

    #[test]
    fn loss_is_a_share_of_what_the_window_should_have_carried() {
        let a = report("zs-1-1", 100, 0, Some(20.0));
        let b = report("zs-1-1", 190, 10, Some(20.0));
        let s = signals(&a, &b).unwrap();
        assert_eq!(s.loss_pct, 10.0, "10 lost out of 100 offered");
    }

    fn bad() -> Signals {
        Signals {
            loss_pct: 25.0,
            age_ms: Some(30.0),
            queue: None,
        }
    }
    fn good() -> Signals {
        Signals {
            loss_pct: 0.0,
            age_ms: Some(30.0),
            queue: Some(0),
        }
    }
    const DEADLINE: Option<Duration> = Some(Duration::from_millis(1500));

    #[test]
    fn a_degraded_link_moves_down_but_not_before_the_dwell_is_served() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0);
        // Inside MIN_DWELL: measured, not acted on.
        assert_eq!(
            c.observe(&bad(), DEADLINE, 8, t0 + Duration::from_secs(9)),
            Move::Hold
        );
        assert_eq!(
            c.observe(&bad(), DEADLINE, 8, t0 + MIN_DWELL + Duration::from_secs(1)),
            Move::Down
        );
    }

    #[test]
    fn the_reports_that_describe_a_switch_are_discarded_not_averaged() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - Duration::from_secs(60));
        c.note_switch(t0);
        // A resync burst right after the switch. If this were folded in, the
        // controller would learn that switching causes loss.
        assert_eq!(
            c.observe(&bad(), DEADLINE, 8, t0 + Duration::from_secs(3)),
            Move::Hold
        );
        assert_eq!(c.smoothed_loss_pct(), None, "not folded in at all");
    }

    #[test]
    fn an_upgrade_needs_sustained_health_not_one_good_window() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - MIN_DWELL - Duration::from_secs(1));
        let mut now = t0;
        for _ in 0..10 {
            assert_eq!(
                c.observe(&good(), DEADLINE, 8, now),
                Move::Hold,
                "still inside UP_SUSTAIN at {now:?}"
            );
            now += Duration::from_secs(3);
        }
        assert_eq!(c.observe(&good(), DEADLINE, 8, now), Move::Up);
    }

    #[test]
    fn one_bad_window_restarts_the_upgrade_clock() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - MIN_DWELL - Duration::from_secs(1));
        let mut now = t0;
        for _ in 0..9 {
            c.observe(&good(), DEADLINE, 8, now);
            now += Duration::from_secs(3);
        }
        // In the hysteresis band: above UP_LOSS_PCT, below DOWN_LOSS_PCT.
        let middling = Signals {
            loss_pct: 2.0,
            ..good()
        };
        assert_eq!(c.observe(&middling, DEADLINE, 8, now), Move::Hold);
        now += Duration::from_secs(3);
        // The clock restarted, so the window that would have been the tenth is
        // now the first.
        assert_eq!(c.observe(&good(), DEADLINE, 8, now), Move::Hold);
    }

    #[test]
    fn a_pinned_stream_is_never_moved() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - Duration::from_secs(600));
        c.pin(t0);
        let mut now = t0 + COOLDOWN + Duration::from_secs(1);
        for _ in 0..20 {
            assert_eq!(c.observe(&bad(), DEADLINE, 8, now), Move::Hold);
            now += Duration::from_secs(3);
        }
        assert!(c.pinned());
        // Releasing does not act on what accumulated while pinned, and serves
        // a fresh dwell first.
        c.unpin(now);
        assert_eq!(c.observe(&bad(), DEADLINE, 8, now), Move::Hold);
        assert_eq!(
            c.observe(
                &bad(),
                DEADLINE,
                8,
                now + MIN_DWELL + Duration::from_secs(1)
            ),
            Move::Down
        );
    }

    #[test]
    fn an_unmeasured_frame_age_disables_the_age_input_rather_than_reading_zero() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - MIN_DWELL - Duration::from_secs(1));
        // Unstamped samples, and a link that is otherwise perfect. A zero would
        // read as "wonderfully fresh" and would be just as wrong as reading it
        // as "hopelessly late".
        let unstamped = Signals {
            age_ms: None,
            ..good()
        };
        let mut now = t0;
        for _ in 0..10 {
            assert_eq!(c.observe(&unstamped, DEADLINE, 8, now), Move::Hold);
            now += Duration::from_secs(3);
        }
        assert_eq!(
            c.observe(&unstamped, DEADLINE, 8, now),
            Move::Up,
            "an absent age must not block an upgrade the other inputs earned"
        );
    }

    #[test]
    fn no_deadline_means_no_latency_policy_at_all() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - MIN_DWELL - Duration::from_secs(1));
        // Five seconds of frame age, and no deadline set: the operator asked
        // for no latency policy, so this is not the controller's business.
        let late = Signals {
            age_ms: Some(5000.0),
            ..good()
        };
        assert_eq!(c.observe(&late, None, 8, t0), Move::Hold);
    }

    #[test]
    fn a_full_decode_queue_is_a_reason_to_downgrade_on_its_own() {
        let t0 = Instant::now();
        let mut c = TierController::new(t0 - MIN_DWELL - Duration::from_secs(1));
        let backed_up = Signals {
            queue: Some(7),
            ..good()
        };
        assert_eq!(c.observe(&backed_up, DEADLINE, 8, t0), Move::Down);
    }

    /// The acceptance criterion, as a trace: degrade, recover, and watch the
    /// ladder. A flapping controller is worse than none, so what matters is not
    /// "did it move" but *how often* and *where it stopped*.
    #[test]
    fn a_degrade_and_recover_trace_walks_the_ladder_down_and_back_without_flapping() {
        let t = tiers();
        let t0 = Instant::now();
        let mut c = TierController::new(t0);
        let mut tier = "high".to_string();
        let mut now = t0;
        let mut path = vec![tier.clone()];
        // 60 s degraded, then 120 s healthy, sampled at the report cadence.
        for i in 0..60 {
            let s = if i < 20 { bad() } else { good() };
            let mv = c.observe(&s, DEADLINE, 8, now);
            // The caller's rule: a move at the end of the ladder is not a move,
            // and must not restart the dwell for a switch that never happened.
            if let Some(to) = next_tier(&t, &tier, mv) {
                tier = to;
                path.push(tier.clone());
                c.note_switch(now);
            }
            now += Duration::from_secs(3);
        }
        assert_eq!(
            path,
            vec!["high", "medium", "low", "medium", "high"],
            "expected a walk down and back, one rung at a time"
        );
    }
}
