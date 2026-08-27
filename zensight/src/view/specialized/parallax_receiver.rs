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
/// How many access units may wait for the decoder.
///
/// This is the tile's `decodeQueueSize` — the browser tile's field, the same
/// meaning — and it is deliberately shallow. A deep queue on a live plane buys
/// nothing: `frame`'s QoS already declares a stale frame worthless, so depth
/// beyond "cover a scheduling hiccup" only converts a visible drop into
/// invisible latency, which is the exact failure #716 exists to stop. Eight
/// frames is a quarter-second at 30 fps.
///
/// It lives here, beside the accounting, rather than inside the `h264`-gated
/// decoder: `decoder_queue_depth` is a *report* field, and the tier controller
/// (#720) needs the denominator to turn a depth into an occupancy in a default
/// build too.
pub const DECODE_QUEUE_CAP: usize = 8;

const JITTER_GAIN: f64 = 16.0;

/// Stamped samples a tile must see before it will call its own deadline
/// unreachable.
///
/// The floor below is only meaningful once it has had a chance to fall. A
/// tile that opens mid-burst sees a few late frames first; at 15–30 fps this
/// is a second or two of evidence, which is long enough for one fresh frame to
/// arrive if fresh frames exist at all.
const MIN_SAMPLES_FOR_SKEW: u64 = 30;

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
    /// Arrived with no readable `FrameMeta`.
    pub malformed: u64,
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
            + self.malformed
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
    /// The sample carried no readable `FrameMeta`, so nothing downstream could
    /// place it in the sequence. Counted rather than skipped: a producer
    /// emitting malformed attachments would otherwise look, on the wire,
    /// exactly like a producer emitting nothing at all.
    Malformed,
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
    /// Stamped samples over the tile's life, and the smallest age any of them
    /// showed. See [`Self::deadline_is_reachable`].
    stamped: u64,
    min_age_ms: Option<f64>,

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
            stamped: 0,
            min_age_ms: None,
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

        self.fold_age(age_ms);

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

    /// Fold one sample's observed age into the interval window, the jitter
    /// estimate and the lifetime floor.
    fn fold_age(&mut self, age_ms: Option<f64>) {
        let Some(age) = age_ms else {
            self.unstamped += 1;
            return;
        };
        self.ages_ms.push(age);
        self.stamped += 1;
        self.min_age_ms = Some(self.min_age_ms.map_or(age, |m: f64| m.min(age)));
        // RFC 3550 inter-arrival jitter over the transit times: the smoothed
        // mean deviation of (arrival - publication) between consecutive
        // samples. It needs both clocks, so an unstamped stream has no jitter
        // either — and reports none, rather than a confident zero.
        if let Some(prev) = self.prev_transit_ms {
            let d = (age - prev).abs();
            self.jitter_ms = Some(self.jitter_ms.map_or(d, |j| j + (d - j) / JITTER_GAIN));
        }
        self.prev_transit_ms = Some(age);
    }

    /// A frame this consumer shed on purpose.
    pub fn on_shed(&mut self, why: Shed) {
        match why {
            Shed::Deadline => self.sheds.deadline += 1,
            Shed::QueueFull => self.sheds.queue_full += 1,
            Shed::Unsynced => self.sheds.unsynced += 1,
            Shed::Backlog => self.sheds.backlog += 1,
            Shed::Malformed => self.sheds.malformed += 1,
        }
    }

    /// A sample arrived that carries no readable `FrameMeta`.
    ///
    /// Counted as received and shed, but deliberately left out of the sequence
    /// state: there is no sequence number to anchor to, and inventing one
    /// (`FrameMeta::default()` is sequence 0) would fabricate a gap the size of
    /// the whole stream. Its **age** is still folded in — the middleware
    /// stamped the sample whatever the producer put in the attachment.
    pub fn on_unreadable_sample(&mut self, age_ms: Option<f64>) {
        self.received += 1;
        self.fold_age(age_ms);
        self.sheds.malformed += 1;
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

    /// Whether a frame-age deadline of `limit` is one any frame could ever
    /// meet on this stream.
    ///
    /// # Why a deadline needs this guard
    ///
    /// Frame age is `arrival − publisher HLC`, and RFC 07 §1.3 is explicit
    /// that it is *observed skewed latency* — an **observation**, never a
    /// verdict on the transport. A deadline turns it into a verdict anyway,
    /// which is fine while the two clocks agree and catastrophic when they do
    /// not: a fleet host whose clock trails the viewer's by three seconds
    /// makes every one of its frames read as three seconds old. Every delta
    /// would be shed, every tile would degrade to a keyframe slideshow, and
    /// `dropped_frames` would blame the viewer for a clock.
    ///
    /// The **smallest age ever observed** separates the two cases. Under a
    /// real backlog at least some frames arrive fresh, so the floor is small;
    /// under a systematic offset the floor *is* the offset and never falls
    /// below it. A deadline under that floor is one no frame can ever meet,
    /// and a deadline nothing can meet is not a deadline — it is an off
    /// switch with extra steps. So the tile disarms it and keeps playing.
    ///
    /// This deliberately does **not** correct the age it reports. Subtracting
    /// the floor would launder skew into a latency number, which is precisely
    /// what §1.3 forbids; the report still carries the raw observation, and an
    /// operator reading "frame age 3000 ms" on a LAN has been told exactly
    /// what is wrong.
    pub fn deadline_is_reachable(&self, limit: Duration) -> bool {
        match self.min_age_ms {
            Some(floor) if self.stamped >= MIN_SAMPLES_FOR_SKEW => {
                floor <= limit.as_millis() as f64
            }
            // Not enough evidence yet: the deadline stays armed. A guard that
            // defaults to "off" would never come on.
            _ => true,
        }
    }

    /// The smallest frame age observed over this tile's life, in milliseconds
    /// — a lower bound on the clock offset between the two hosts plus the
    /// path's true minimum latency.
    pub fn min_frame_age_ms(&self) -> Option<f64> {
        self.min_age_ms
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
            // Same omission rule as frame age, and for the same reason: a tile
            // whose stream stopped must not keep publishing the jitter it
            // measured minutes ago as though it were current. The estimate
            // itself survives the reset so a resumed stream does not restart
            // its smoothing from nothing.
            interarrival_jitter_ms: if self.ages_ms.is_empty() {
                None
            } else {
                self.jitter_ms.map(|j| j as f32)
            },
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

    /// A producer whose clock trails ours by more than the deadline makes
    /// every one of its frames read as older than the deadline. Shedding them
    /// all would degrade the tile to keyframes forever and blame the viewer.
    #[test]
    fn a_deadline_no_frame_can_ever_meet_is_disarmed() {
        let limit = Duration::from_millis(1_500);
        let mut skewed = stats();
        for i in 0..MIN_SAMPLES_FOR_SKEW {
            // A 3-second clock offset, jittering a little.
            skewed.on_sample(&frame(i + 1, i == 0), Some(3_000.0 + i as f64));
        }
        assert!(
            !skewed.deadline_is_reachable(limit),
            "no frame on this stream has EVER been younger than the deadline: \
             that is a clock offset, not a backlog"
        );
        assert!(skewed.min_frame_age_ms().is_some_and(|f| f >= 3_000.0));

        // A genuine backlog looks different: some frames DO arrive fresh, so
        // the floor falls below the deadline and the deadline stays armed.
        let mut backlogged = stats();
        for i in 0..MIN_SAMPLES_FOR_SKEW {
            let age = if i % 10 == 0 { 40.0 } else { 4_000.0 };
            backlogged.on_sample(&frame(i + 1, i == 0), Some(age));
        }
        assert!(
            backlogged.deadline_is_reachable(limit),
            "frames that arrive fresh prove the deadline is meetable"
        );
    }

    /// The guard must not fire before it has evidence, or it would disarm the
    /// deadline on the first late frame of every stream and never come back.
    #[test]
    fn the_skew_guard_waits_for_evidence_before_disarming() {
        let limit = Duration::from_millis(1_500);
        let mut s = stats();
        s.on_sample(&frame(1, true), Some(9_000.0));
        assert!(
            s.deadline_is_reachable(limit),
            "one late frame is not proof of a clock offset"
        );
        // An unstamped stream never accumulates evidence either, and its
        // deadline is inactive for a different reason entirely.
        let mut unstamped = stats();
        for i in 0..MIN_SAMPLES_FOR_SKEW * 2 {
            unstamped.on_sample(&frame(i + 1, i == 0), None);
        }
        assert!(unstamped.deadline_is_reachable(limit));
        assert_eq!(unstamped.min_frame_age_ms(), None);
    }

    /// #718's rule, applied to jitter as well as to frame age: a tile whose
    /// stream stopped must not keep publishing what it measured minutes ago.
    #[test]
    fn a_silent_interval_reports_no_jitter_rather_than_the_last_one() {
        let start = Instant::now();
        let mut s = ReceiverStats::new("cam0".into(), None, None, 1, start);
        for i in 0..4 {
            s.on_sample(&frame(i + 1, i == 0), Some(10.0 + i as f64 * 3.0));
        }
        let live = s.snapshot(start + Duration::from_secs(3));
        assert!(live.interarrival_jitter_ms.is_some());

        let silent = s.snapshot(start + Duration::from_secs(6));
        assert_eq!(
            silent.interarrival_jitter_ms, None,
            "stale is not current, exactly as absent is not zero"
        );
        assert_eq!(silent.frame_age_ms, None);
    }

    /// A sample nothing can read still arrived. Leaving it out of every
    /// counter makes a producer emitting malformed attachments look, on the
    /// wire, exactly like one emitting nothing at all.
    #[test]
    fn an_unreadable_sample_is_counted_rather_than_skipped() {
        let mut s = stats();
        s.on_sample(&frame(10, true), Some(5.0));
        s.on_unreadable_sample(Some(6.0));
        s.on_unreadable_sample(None);
        // The next readable frame is contiguous with sequence 10: the
        // unreadable ones carried no sequence to anchor to, and inventing one
        // would fabricate a gap the size of the whole stream.
        assert_eq!(s.on_sample(&frame(11, false), Some(5.0)), Gap::None);

        let r = s.snapshot(Instant::now());
        assert_eq!(r.received_frames, 4);
        assert_eq!(r.dropped_frames, 2, "counted, and counted as ours");
        assert_eq!(r.lost_frames, 0, "nothing was lost on the wire");
        assert_eq!(s.sheds().malformed, 2);
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
