//! Per-tile receiver accounting for the `@media` plane (#717) and the
//! [`MediaReceiverReport`] a tile publishes from it (#718, RFC 07 §1.1).
//!
//! # Why this is not inside the H.264 tile
//!
//! Two reasons. It is **not feature-gated**, so a default build's JPEG preview
//! tiles account for themselves too — and an operator comparing a preview
//! tile's numbers against a video tile's is exactly how you tell "the camera is
//! fine, H.264 is not". And it is **pure**: no Zenoh, no decoder, no iced, so
//! the arithmetic that decides what an operator is told can be unit-tested
//! without standing up a wgpu device (`#687`).
//!
//! # The one rule: every frame that does not reach the screen has exactly one
//! cause
//!
//! Before this module the H.264 tile lost frames four ways and counted none of
//! them: a sequence gap (network), a non-keyframe while unsynced, an
//! `Ok(None)` that meant *either* decoder buffering *or* a starved arena, and
//! an oversize access unit that resync-looped forever. The preview tile lost
//! them a fifth way, in the latest-frame-wins drain. All five looked identical
//! from outside the process. [`Shed`] and [`DecodeLoss`] name them, and
//! [`ReceiverStats::snapshot`] folds them into the two wire fields that mean
//! different things:
//!
//! - [`MediaReceiverReport::lost_frames`] — inferred from sequence gaps.
//!   **Network** loss, or a sender-side drop; either way not our doing.
//! - [`MediaReceiverReport::dropped_frames`] — what this consumer shed *on
//!   purpose*, plus what its decoder could not take. Our doing.
//!
//! Merging them would tell a producer that its link is bad when the truth is
//! that the viewer's box is too slow, which is the diagnosis #712 exists to
//! make possible.
//!
//! # Absent is not zero
//!
//! Frame age is omitted, never zeroed, when the interval's samples arrived
//! unstamped (RFC 07 §1.3 — see [`zensight_common::media`]), and the decoder
//! queue depth is omitted by a tile that has no queue. `Some(0)` and `None` are
//! different observations and a controller must be able to tell them apart.

use std::time::{Duration, Instant};

use zensight_common::stream::{FrameMeta, MediaReceiverReport};

use super::parallax_detail::SEQ_RESTART_GAP;

/// How often an open tile reports to its producer.
///
/// The registry declares a *ceiling* of one report per second per
/// `(consumer_id, stream, tier)` (`rate = "burst(3600/h)"`, enforced by
/// `zensight-sensor-parallax`'s `REPORT_MIN_INTERVAL`); RFC 07 §1.1's
/// *reference cadence* is one per few seconds, and a consumer MUST NOT report
/// per frame. Three seconds sits inside the ceiling with enough headroom that a
/// scheduling hiccup cannot turn a well-behaved tile into an `error/busy`.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(3);

/// Smoothing divisor for the RFC 3550 inter-arrival jitter estimate.
const JITTER_GAIN: f64 = 16.0;

/// A frame this consumer shed on purpose — [`MediaReceiverReport::dropped_frames`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShedCounts {
    /// Older than the frame-age deadline on arrival (#716).
    pub deadline: u64,
    /// The bounded decode queue was full — the decoder is behind (#717).
    pub queue_full: u64,
    /// A non-keyframe while out of sync: undecodable, not lost.
    pub unsynced: u64,
    /// Superseded in the preview tile's latest-frame-wins drain.
    pub backlog: u64,
    /// The shared access-unit arena had no free slot.
    pub arena_full: u64,
    /// An access unit larger than a decoder slot.
    pub oversize: u64,
    /// The decoder refused the access unit.
    pub decode_error: u64,
}

impl ShedCounts {
    /// Every frame that entered the tile and did not reach the screen by this
    /// consumer's own doing.
    pub fn total(&self) -> u64 {
        self.deadline
            + self.queue_full
            + self.unsynced
            + self.backlog
            + self.arena_full
            + self.oversize
            + self.decode_error
    }
}

/// Why a frame was shed before it reached the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shed {
    /// Frame age exceeded the deadline (#716).
    Deadline,
    /// The bounded decode queue was full (#717).
    QueueFull,
    /// A delta frame arriving while out of sync — the reference chain is gone.
    Unsynced,
    /// The preview tile's latest-frame-wins drain superseded it.
    Backlog,
}

/// Why a frame that reached the decoder did not come out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeLoss {
    /// No free slot in the shared access-unit arena.
    ArenaFull,
    /// The access unit exceeds one arena slot.
    Oversize,
    /// The decoder refused the access unit.
    Failed,
}

/// What a sample's sequence number says about the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// Contiguous with the previous sample (or the first one).
    None,
    /// `n` samples never arrived — network loss or a sender-side drop.
    Missing(u64),
    /// The sequence regressed past [`SEQ_RESTART_GAP`]: the producer's
    /// pipeline restarted. Not loss, and re-anchored rather than counted.
    Restart,
    /// The sequence went backwards by less than [`SEQ_RESTART_GAP`] — a
    /// reorder, or a duplicate. Nothing is missing, but a codec with a
    /// reference chain must not feed it to the decoder either, so it is its
    /// own answer rather than being folded into [`Gap::None`].
    Backward,
}

/// One tile's view of how its stream is arriving.
///
/// Counters are **cumulative since the tile subscribed**, which is what makes
/// the `stream/report` procedure's `idempotent = true` true: a resend repeats a
/// statement rather than adding to one. Only the timing window is
/// interval-scoped, and [`Self::snapshot`] is the only thing that resets it.
#[derive(Debug)]
pub struct ReceiverStats {
    stream: String,
    codec: Option<String>,
    tier: Option<String>,
    consumer_id: String,

    received: u64,
    lost: u64,
    decoded: u64,
    sheds: ShedCounts,
    last_sequence: u64,
    prev_sequence: Option<u64>,

    interval_started: Instant,
    ages_ms: Vec<f64>,
    /// Samples this interval that arrived without an HLC timestamp. Kept so a
    /// tile can say "the deadline is inactive" instead of reporting age zero.
    unstamped: u64,

    jitter_ms: Option<f64>,
    prev_transit_ms: Option<f64>,

    queue_depth: Option<u32>,

    last_keyframe_sequence: Option<u64>,
    last_keyframe_at: Option<Instant>,
}

/// A `consumer_id` for one tile incarnation: stable for its lifetime,
/// regenerated on reopen because `allocate_generation` mints a fresh
/// generation per incarnation, and distinct between two GUI processes on one
/// host.
///
/// **In the payload, never in a key** (RFC 07 §1.1) — which is why it may be
/// this cheap. It has no cardinality budget to blow because nothing keys on it;
/// the sensor bounds it by length (64 bytes) and by evicting the stalest entry
/// per tier.
pub fn consumer_id(generation: u64) -> String {
    format!("zs-{}-{generation}", std::process::id())
}

impl ReceiverStats {
    pub fn new(
        stream: String,
        codec: Option<String>,
        tier: Option<String>,
        generation: u64,
        now: Instant,
    ) -> Self {
        Self {
            stream,
            codec,
            tier,
            consumer_id: consumer_id(generation),
            received: 0,
            lost: 0,
            decoded: 0,
            sheds: ShedCounts::default(),
            last_sequence: 0,
            prev_sequence: None,
            interval_started: now,
            ages_ms: Vec::new(),
            unstamped: 0,
            jitter_ms: None,
            prev_transit_ms: None,
            queue_depth: None,
            last_keyframe_sequence: None,
            last_keyframe_at: None,
        }
    }

    /// Fold one arriving sample in, and say what its sequence number implies
    /// about the ones before it.
    ///
    /// `age_ms` is [`zensight_common::media::observed_frame_age_ms`]'s verdict:
    /// `None` means the sample was unstamped, which is counted separately and
    /// never as zero.
    pub fn on_sample(&mut self, meta: &FrameMeta, age_ms: Option<f64>) -> Gap {
        self.received += 1;

        match age_ms {
            Some(age) => {
                self.ages_ms.push(age);
                // RFC 3550 inter-arrival jitter over the transit times: the
                // smoothed mean deviation of (arrival - publication) between
                // consecutive samples. It needs both clocks, so an unstamped
                // stream has no jitter either — and reports none, rather than
                // a confident zero.
                if let Some(prev) = self.prev_transit_ms {
                    let d = (age - prev).abs();
                    self.jitter_ms = Some(self.jitter_ms.map_or(d, |j| j + (d - j) / JITTER_GAIN));
                }
                self.prev_transit_ms = Some(age);
            }
            None => self.unstamped += 1,
        }

        let gap = match self.prev_sequence {
            None => Gap::None,
            Some(prev) if meta.sequence == prev.wrapping_add(1) => Gap::None,
            // A regression past the restart window is the producer's pipeline
            // counter going back to ~0, not loss: the same rule the tile's own
            // staleness guard applies (`SEQ_RESTART_GAP`).
            Some(prev) if meta.sequence <= prev && prev - meta.sequence >= SEQ_RESTART_GAP => {
                Gap::Restart
            }
            // A reorder inside the window: not loss, and not a restart. Nothing
            // is missing that was not already counted when it was skipped —
            // but a decoder with a reference chain still cannot take it.
            Some(prev) if meta.sequence <= prev => Gap::Backward,
            Some(prev) => Gap::Missing(meta.sequence - prev - 1),
        };
        match gap {
            Gap::Missing(n) => {
                self.lost += n;
                self.last_sequence = meta.sequence;
            }
            // The producer's counter went back to ~0: its old high-water mark
            // says nothing about where this consumer is in the new domain, and
            // reporting it would have the producer reading a sequence number
            // it will not emit again for hours.
            Gap::Restart => self.last_sequence = meta.sequence,
            Gap::None | Gap::Backward => self.last_sequence = self.last_sequence.max(meta.sequence),
        }
        self.prev_sequence = Some(meta.sequence);
        gap
    }

    /// A frame this consumer shed on purpose.
    pub fn on_shed(&mut self, why: Shed) {
        match why {
            Shed::Deadline => self.sheds.deadline += 1,
            Shed::QueueFull => self.sheds.queue_full += 1,
            Shed::Unsynced => self.sheds.unsynced += 1,
            Shed::Backlog => self.sheds.backlog += 1,
        }
    }

    /// A frame the decoder could not turn into a picture.
    pub fn on_decode_loss(&mut self, why: DecodeLoss) {
        match why {
            DecodeLoss::ArenaFull => self.sheds.arena_full += 1,
            DecodeLoss::Oversize => self.sheds.oversize += 1,
            DecodeLoss::Failed => self.sheds.decode_error += 1,
        }
    }

    /// A frame reached the screen.
    pub fn on_decoded(&mut self, sequence: u64, keyframe: bool, now: Instant) {
        self.decoded += 1;
        if keyframe {
            self.last_keyframe_sequence = Some(sequence);
            self.last_keyframe_at = Some(now);
        }
    }

    /// The decode queue's depth, or `None` for a tile that has no queue to
    /// report — a preview tile decodes latest-frame-wins with nothing pending,
    /// and `Some(0)` there would read "queue empty" where the truth is "no
    /// queue".
    pub fn set_queue_depth(&mut self, depth: Option<u32>) {
        self.queue_depth = depth;
    }

    /// Whether frame age was measurable this interval — false for a producer
    /// with timestamping off, where the deadline is *inactive* rather than
    /// permanently satisfied.
    pub fn frame_age_measured(&self) -> bool {
        !self.ages_ms.is_empty()
    }

    /// Samples this interval that arrived unstamped.
    pub fn unstamped(&self) -> u64 {
        self.unstamped
    }

    /// The drop taxonomy, cumulative. The wire carries one `dropped_frames`
    /// field; the stage that caused each one is kept here for the health
    /// surface and for the log line.
    pub fn sheds(&self) -> ShedCounts {
        self.sheds
    }

    /// The report to send, and the start of a fresh timing window.
    ///
    /// Resets **only** the interval-scoped state: the timing window, the frame
    /// ages, and the unstamped tally. The counters stay cumulative — that is
    /// what makes a resend idempotent.
    pub fn snapshot(&mut self, now: Instant) -> MediaReceiverReport {
        // The sensor refuses `interval_ms == 0` ("a zero-span snapshot has no
        // rate"), and a snapshot taken twice inside one millisecond is a
        // legitimate way to reach it. One millisecond is a lie of at most one
        // millisecond; a refusal loses the whole report.
        let interval_ms = now
            .saturating_duration_since(self.interval_started)
            .as_millis()
            .clamp(1, u128::from(u32::MAX)) as u32;

        let report = MediaReceiverReport {
            stream: self.stream.clone(),
            codec: self.codec.clone(),
            tier: self.tier.clone(),
            consumer_id: self.consumer_id.clone(),
            interval_ms,
            received_frames: self.received,
            lost_frames: self.lost,
            dropped_frames: self.sheds.total(),
            decoded_frames: self.decoded,
            last_sequence: self.last_sequence,
            interarrival_jitter_ms: self.jitter_ms.map(|j| j as f32),
            frame_age_ms: median_of(&self.ages_ms).map(|v| v as f32),
            frame_age_max_ms: max_of(&self.ages_ms).map(|v| v as f32),
            decoder_queue_depth: self.queue_depth,
            last_keyframe_sequence: self.last_keyframe_sequence,
            since_last_keyframe_ms: self.last_keyframe_at.map(|at| {
                now.saturating_duration_since(at)
                    .as_millis()
                    .min(u128::from(u32::MAX)) as u32
            }),
        };

        self.interval_started = now;
        self.ages_ms.clear();
        self.unstamped = 0;
        report
    }
}

/// `None` on an empty slice — the caller omits the field rather than reporting
/// zero. Lower median on an even count, matching the sensor's own fold so a
/// median-of-medians is computed the same way at both ends.
fn median_of(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(s[(s.len() - 1) / 2])
}

/// `None` on an empty slice.
fn max_of(v: &[f64]) -> Option<f64> {
    v.iter().copied().fold(None, |acc: Option<f64>, x| {
        Some(acc.map_or(x, |a: f64| a.max(x)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sensor's own bounds, restated so a change to either end fails here
    /// rather than at runtime with an `error/invalid-args` the GUI swallows.
    /// (`zensight-sensor-parallax/src/reports.rs`.)
    const SENSOR_MAX_CONSUMER_ID: usize = 64;
    const SENSOR_REPORT_MIN_INTERVAL: Duration = Duration::from_secs(1);

    fn stats() -> ReceiverStats {
        ReceiverStats::new(
            "cam0".into(),
            Some("h264".into()),
            Some("high".into()),
            7,
            Instant::now(),
        )
    }

    fn frame(sequence: u64, keyframe: bool) -> FrameMeta {
        FrameMeta {
            sequence,
            keyframe,
            width: 640,
            height: 480,
            ..Default::default()
        }
    }

    #[test]
    fn a_sequence_gap_is_loss_and_a_shed_is_not() {
        let mut s = stats();
        assert_eq!(s.on_sample(&frame(1, true), Some(10.0)), Gap::None);
        assert_eq!(s.on_sample(&frame(2, false), Some(10.0)), Gap::None);
        // 3 and 4 never arrived.
        assert_eq!(s.on_sample(&frame(5, false), Some(10.0)), Gap::Missing(2));
        s.on_shed(Shed::Deadline);
        s.on_decode_loss(DecodeLoss::ArenaFull);

        let r = s.snapshot(Instant::now());
        assert_eq!(r.lost_frames, 2, "the gap is NETWORK loss");
        assert_eq!(
            r.dropped_frames, 2,
            "a deadline shed and an arena starve are OUR doing, and must never \
             be reported as the producer's link losing frames"
        );
        assert_eq!(r.received_frames, 3);
        assert_eq!(r.last_sequence, 5);
    }

    #[test]
    fn a_pipeline_restart_re_anchors_instead_of_counting_a_billion_lost_frames() {
        let mut s = stats();
        s.on_sample(&frame(9_000, true), None);
        assert_eq!(
            s.on_sample(&frame(1, true), None),
            Gap::Restart,
            "a regression past SEQ_RESTART_GAP is the producer's counter resetting"
        );
        let r = s.snapshot(Instant::now());
        assert_eq!(r.lost_frames, 0);
        assert_eq!(
            r.last_sequence, 1,
            "the old high-water mark belongs to a sequence domain the producer \
             abandoned; reporting 9000 would have it reading a number it will \
             not emit again for hours"
        );
    }

    #[test]
    fn a_reorder_inside_the_window_is_not_loss() {
        let mut s = stats();
        s.on_sample(&frame(10, true), None);
        assert_eq!(
            s.on_sample(&frame(9, false), None),
            Gap::Backward,
            "nothing is missing — but it is not `None` either, because a codec \
             with a reference chain must not decode it"
        );
        assert_eq!(s.snapshot(Instant::now()).lost_frames, 0);
        // …and the highest sequence seen is still reported.
        let mut s2 = stats();
        s2.on_sample(&frame(10, true), None);
        s2.on_sample(&frame(9, false), None);
        assert_eq!(s2.snapshot(Instant::now()).last_sequence, 10);
    }

    #[test]
    fn an_unstamped_interval_omits_frame_age_rather_than_reporting_zero() {
        let mut s = stats();
        s.on_sample(&frame(1, true), None);
        s.on_sample(&frame(2, false), None);
        assert!(!s.frame_age_measured(), "the deadline is INACTIVE, not met");
        assert_eq!(s.unstamped(), 2);

        let r = s.snapshot(Instant::now());
        assert_eq!(
            r.frame_age_ms, None,
            "RFC 07 §1.3: unstamped is NOT ASKED — a Some(0.0) silently disables \
             every deadline built on it"
        );
        assert_eq!(r.frame_age_max_ms, None);
        assert_eq!(r.interarrival_jitter_ms, None, "no clock, no jitter either");
    }

    #[test]
    fn a_negative_frame_age_survives_into_the_report_unclamped() {
        let mut s = stats();
        s.on_sample(&frame(1, true), Some(-12.5));
        s.on_sample(&frame(2, false), Some(-11.0));
        let r = s.snapshot(Instant::now());
        assert!(
            r.frame_age_ms.is_some_and(|v| v < 0.0),
            "a negative age IS the clock-skew evidence (RFC 07 §1.3)"
        );
        assert!(r.frame_age_max_ms.is_some_and(|v| v < 0.0));
    }

    #[test]
    fn frame_age_reports_a_median_and_a_max_not_one_scalar() {
        let mut s = stats();
        for (i, age) in [10.0, 20.0, 400.0].iter().enumerate() {
            s.on_sample(&frame(i as u64 + 1, i == 0), Some(*age));
        }
        let r = s.snapshot(Instant::now());
        assert_eq!(r.frame_age_ms, Some(20.0), "median, not mean");
        assert_eq!(r.frame_age_max_ms, Some(400.0), "the outlier is the max");
    }

    #[test]
    fn snapshot_resets_the_window_and_keeps_the_counters() {
        let start = Instant::now();
        let mut s = ReceiverStats::new("cam0".into(), None, None, 1, start);
        s.on_sample(&frame(1, true), Some(5.0));
        let first = s.snapshot(start + Duration::from_secs(3));
        assert_eq!(first.received_frames, 1);
        assert_eq!(first.frame_age_ms, Some(5.0));

        // A second interval with no samples at all: the counters persist (they
        // are cumulative, which is what makes a resend idempotent) but the
        // timing window is gone — a tile receiving nothing must not keep
        // reporting the frame age it measured minutes ago.
        let second = s.snapshot(start + Duration::from_secs(6));
        assert_eq!(second.received_frames, 1, "counters are cumulative");
        assert_eq!(second.frame_age_ms, None, "the window reset");
        assert!(
            (2_500..=3_500).contains(&second.interval_ms),
            "interval_ms spans the LAST window, not the tile's life: {}",
            second.interval_ms
        );
    }

    #[test]
    fn a_keyframe_is_remembered_for_the_recovery_fields() {
        let start = Instant::now();
        let mut s = ReceiverStats::new("cam0".into(), None, None, 1, start);
        s.on_decoded(41, false, start);
        assert_eq!(s.snapshot(start).last_keyframe_sequence, None);
        s.on_decoded(42, true, start);
        let r = s.snapshot(start + Duration::from_millis(500));
        assert_eq!(r.last_keyframe_sequence, Some(42));
        assert!(
            r.since_last_keyframe_ms.is_some_and(|ms| ms >= 400),
            "monotonic elapsed time on ONE host — never negative, unlike frame age"
        );
        assert_eq!(r.decoded_frames, 2);
    }

    #[test]
    fn a_preview_tile_reports_no_queue_rather_than_an_empty_one() {
        let mut s = stats();
        assert_eq!(s.snapshot(Instant::now()).decoder_queue_depth, None);
        s.set_queue_depth(Some(0));
        assert_eq!(
            s.snapshot(Instant::now()).decoder_queue_depth,
            Some(0),
            "a video tile with an empty queue reports 0; a tile with NO queue \
             reports nothing, and the two must stay distinguishable"
        );
    }

    /// Every snapshot must clear the bar the sensor sets, or the report is
    /// refused with `error/invalid-args` and the loop silently stays open.
    #[test]
    fn every_snapshot_satisfies_what_the_sensor_accepts() {
        let start = Instant::now();
        let mut s = ReceiverStats::new("cam0".into(), None, None, u64::MAX, start);
        // The pathological case: two snapshots in the same instant.
        for r in [s.snapshot(start), s.snapshot(start)] {
            assert!(!r.consumer_id.is_empty());
            assert!(
                r.consumer_id.len() <= SENSOR_MAX_CONSUMER_ID,
                "consumer_id {:?} is {} bytes",
                r.consumer_id,
                r.consumer_id.len()
            );
            assert_ne!(
                r.interval_ms, 0,
                "the sensor refuses a zero-span snapshot outright"
            );
        }
        assert!(
            REPORT_INTERVAL >= SENSOR_REPORT_MIN_INTERVAL,
            "the cadence must sit inside the registry's declared rate ceiling"
        );
    }
}
