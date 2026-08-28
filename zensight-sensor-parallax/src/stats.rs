//! Per-stream stats: lock-free counters fed by the pipeline/egress tasks,
//! flushed to ordinary telemetry by a ticker.
//!
//! The registry's `Mutex<HashMap>` is touched only on stream open/close and
//! on ticker snapshots; the hot paths (egress per frame, encoder per frame)
//! bump `AtomicU64`s on a shared [`StreamStats`]. Stats are **per stream**,
//! aggregated over its open profiles (kbps is the stream's total media
//! bandwidth; fps counts every published frame, video + preview).
//!
//! Telemetry rides `zensight/v1/<origin>/telemetry/parallax/<stream>/stats/<metric>`
//! (fps / kbps / drops / sink_queue / rc_drops / viewers / encode_ms /
//! encode_p95_ms / encode_p99_ms), so existing charts
//! light up for free; `streams/advertised` is published every tick so a
//! parallax host shows up on the dashboard even before any stream is opened.
//!
//! `fps` and `kbps` are deliberately **egress**-sourced, not encoder-sourced
//! (#510): they count what actually crossed Zenoh — injected SPS/PPS included,
//! sink-shed frames excluded — and RTSP passthrough has no encoder to ask at
//! all. `rc_drops` is the one number only the encoder knows, and `drops` and
//! `sink_queue` are the two only the `AppSink` knows (#692).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zensight_common::TelemetryValue;
use zensight_sensor_core::Publisher;

/// Lock-free counters for one stream (shared by its open profiles).
#[derive(Debug, Default)]
pub struct StreamStats {
    /// Frames published on the media plane (cumulative).
    pub frames: AtomicU64,
    /// Payload bytes published (cumulative).
    pub bytes: AtomicU64,
    /// Buffers a profile's `AppSink` shed because the egress task did not pull
    /// in time (cumulative, summed over the stream's open profiles — video
    /// tiers **and** the preview).
    ///
    /// Read from `AppSinkHandle::stats().total_dropped` and folded as a delta
    /// per profile incarnation; never inferred (#692). It used to be counted
    /// from `FrameMeta.sequence` gaps observed at egress, which was a *proxy
    /// for this very number* and a worse one: blinded across every RTSP
    /// reconnect by the DISCONT reset, absent on the preview path, and
    /// structurally **zero** on RTSP passthrough, whose source never stamps a
    /// sequence at all.
    ///
    /// **Not transport loss.** Congestion discards frames inside Zenoh's own
    /// transport queue, upstream of every counter this sensor has — #713
    /// measured this reading 0 while 83 % of sequence numbers never arrived.
    pub drops: AtomicU64,
    /// Cumulative wall time spent inside encoder `process()` calls.
    pub encode_ns: AtomicU64,
    /// Frames that went through a timed encoder (denominator for encode_ms).
    pub encoded_frames: AtomicU64,
    /// Current matching-viewer count (gauge, set by the session actor).
    pub viewers: AtomicU64,
    /// Strictest per-frame encode budget among open profiles (ns);
    /// 0 = no encoder (e.g. RTSP passthrough) → no overrun evaluation.
    pub budget_ns: AtomicU64,
    /// Frames the H.264 rate controller swallowed to hold a tier's bitrate cap
    /// (cumulative, summed over the stream's open video tiers).
    ///
    /// **Disjoint from [`Self::drops`] by construction**, and on a stronger
    /// basis since #692: a swallowed frame produces no buffer at all, so it
    /// never reaches the sink and cannot be in the sink's shed count. `drops`
    /// is the `AppSink` shedding under a slow consumer; this is the bitrate cap
    /// biting. `skip_frames(true)` is set precisely so the encoder may do this,
    /// and until #510 nothing counted it.
    pub rc_drops: AtomicU64,
    /// 95th / 99th percentile encode latency in nanoseconds, as most recently
    /// read off the stream's live `EncoderStatsHandle`s (0 = none observed).
    ///
    /// **Not derived from [`Self::encode_ns`]** — a sum and a count cannot
    /// produce a percentile. These come from parallax's own 768-byte lock-free
    /// histogram inside the encoder, which is why they exist only for a stream
    /// with an H.264 encoder: a JPEG preview path is timed by `TimedElement`
    /// (so it has `encode_ms`) but has no `EncoderStatsHandle` to ask.
    ///
    /// The histogram is **all-time for that encoder incarnation**, not
    /// windowed — a tail needs history, and a 5 s window on a 30 fps tier holds
    /// 150 samples, of which p99 is one. It resets when a tier is torn down and
    /// rebuilt, which is what bounds how long a bad patch keeps the figure up.
    /// Percentiles are bucket upper bounds: at most 19% high, never low.
    pub encode_p95_ns: AtomicU64,
    /// See [`Self::encode_p95_ns`].
    pub encode_p99_ns: AtomicU64,
    /// Whether a rate-controlled encoder was ever attached to this stream.
    ///
    /// RTSP passthrough and preview-only streams have none, and publishing `0`
    /// for them would read as "the cap is not biting" when the truth is "there
    /// is no cap". The point is omitted instead — the precedent `encode_ms`
    /// already sets.
    pub rc_tracked: AtomicBool,
    /// Deepest `AppSink` backlog observed across the stream's open profiles at
    /// the last fold (0..`SINK_QUEUE`).
    ///
    /// A **store**, not an accumulate, and a **max**, not a sum: depth is
    /// bounded per sink, so summing three profiles would report a queue that
    /// does not exist, and storing means a torn-down profile's backlog stops
    /// being reported instead of sticking. Same discipline as
    /// [`Self::set_encode_tail`].
    ///
    /// The evidence it gives is **asymmetric**: a reading at the cap proves a
    /// backlog that survived a whole second, while a `0` proves nothing — a
    /// 1 Hz sample of a four-deep queue is mostly 0 even on a stream that is
    /// shedding. It is a leading indicator, never a duty cycle.
    pub sink_queue: AtomicU64,
}

/// Add `now - seen` to `counter` and advance `seen`.
///
/// `saturating_sub` absorbs a handle that reset under us, which is what makes
/// the published `Counter` monotone across a profile incarnation change.
fn fold_delta(counter: &AtomicU64, seen: &mut u64, now: u64) {
    counter.fetch_add(now.saturating_sub(*seen), Ordering::Relaxed);
    *seen = now;
}

impl StreamStats {
    /// Record one published frame of `bytes` payload.
    pub fn record_frame(&self, bytes: usize) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Record one encoder `process()` call's wall time.
    pub fn record_encode(&self, ns: u64) {
        self.encode_ns.fetch_add(ns, Ordering::Relaxed);
        self.encoded_frames.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark this stream as carrying a rate-controlled encoder, so its
    /// `rc_drops` becomes reportable (see [`Self::rc_tracked`]).
    pub fn track_rc(&self) {
        self.rc_tracked.store(true, Ordering::Relaxed);
    }

    /// Fold one encoder's cumulative RC-drop count in as a **delta** against
    /// what was last observed for that encoder incarnation, advancing `seen`.
    ///
    /// Deltas, not absolutes: several tiers feed one stream, and a torn-down
    /// tier's replacement gets a fresh handle that restarts at zero. Summing
    /// absolutes would make a published `Counter` go *backwards* on a tier
    /// switch. `saturating_sub` also absorbs a handle that reset under us.
    pub fn fold_rc_drops(&self, seen: &mut u64, now: u64) {
        fold_delta(&self.rc_drops, seen, now);
    }

    /// Fold one `AppSink`'s cumulative shed count in as a **delta** against
    /// what was last observed for that sink, advancing `seen` (#692).
    ///
    /// Same delta discipline as [`Self::fold_rc_drops`], for the same reason:
    /// several profiles feed one stream, and a torn-down tier's replacement
    /// gets a fresh sink that restarts at zero, so summing absolutes would make
    /// a published `Counter` walk *backwards* on a tier switch.
    pub fn fold_sink_drops(&self, seen: &mut u64, now: u64) {
        fold_delta(&self.drops, seen, now);
    }

    /// Publish the deepest backlog across this stream's open profiles.
    /// See [`Self::sink_queue`] for why it stores rather than accumulates.
    pub fn set_sink_queue(&self, depth: u64) {
        self.sink_queue.store(depth, Ordering::Relaxed);
    }

    /// Publish the tail latencies observed across this stream's live encoders.
    ///
    /// A **store**, not an accumulate: several tiers feed one stream and a
    /// percentile is not summable, so the caller takes the worst live tier and
    /// writes it whole each tick. Storing rather than max-ing also means a
    /// torn-down tier's tail stops being reported instead of sticking forever.
    pub fn set_encode_tail(&self, p95_ns: u64, p99_ns: u64) {
        self.encode_p95_ns.store(p95_ns, Ordering::Relaxed);
        self.encode_p99_ns.store(p99_ns, Ordering::Relaxed);
    }

    /// The stream's p95/p99 encode latency in milliseconds, or `None` when no
    /// encoder on this stream reports a histogram (preview-only, RTSP
    /// passthrough, or a tier that has not encoded a frame yet).
    pub fn encode_tail_ms(&self) -> Option<(f64, f64)> {
        let p95 = self.encode_p95_ns.load(Ordering::Relaxed);
        let p99 = self.encode_p99_ns.load(Ordering::Relaxed);
        (p95 > 0).then(|| (p95 as f64 / 1e6, p99 as f64 / 1e6))
    }

    /// The stream's RC drops, or `None` when it has no rate-controlled encoder.
    pub fn rc_drops(&self) -> Option<u64> {
        self.rc_tracked
            .load(Ordering::Relaxed)
            .then(|| self.rc_drops.load(Ordering::Relaxed))
    }

    /// Tighten the per-frame budget (keeps the strictest non-zero value).
    pub fn tighten_budget(&self, budget_ns: u64) {
        if budget_ns == 0 {
            return;
        }
        let mut current = self.budget_ns.load(Ordering::Relaxed);
        while current == 0 || budget_ns < current {
            match self.budget_ns.compare_exchange_weak(
                current,
                budget_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

/// Shared registry: one [`StreamStats`] per open stream.
#[derive(Debug, Clone, Default)]
pub struct StatsRegistry {
    inner: Arc<Mutex<HashMap<String, Arc<StreamStats>>>>,
}

impl StatsRegistry {
    /// Get (or create) the stats handle for `stream` — called at open.
    pub fn handle(&self, stream: &str) -> Arc<StreamStats> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(stream.to_string()).or_default().clone()
    }

    /// Drop a stream's stats — called when its last profile closes.
    pub fn remove(&self, stream: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(stream);
    }

    /// Snapshot the open streams' stats handles.
    pub fn snapshot(&self) -> Vec<(String, Arc<StreamStats>)> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

/// Counter values remembered between ticks, for rate derivation.
#[derive(Debug, Default, Clone, Copy)]
struct PrevCounters {
    frames: u64,
    bytes: u64,
    encode_ns: u64,
    encoded_frames: u64,
    drops: u64,
}

/// One tick's derived numbers for a stream (pure — unit-tested).
#[derive(Debug, PartialEq)]
struct TickDerived {
    fps: f64,
    kbps: f64,
    encode_ms: Option<f64>,
    /// Frames published this interval — parallax's QoS `processed`.
    published: u64,
    /// Buffers the sinks shed this interval — parallax's QoS `dropped`.
    shed: u64,
    /// `(published + shed) / published`, parallax's own QoS proportion
    /// (`1.0` keeping up, `2.0` only half made it).
    ///
    /// Computed here from the sink's counters rather than received as an
    /// `Event::Qos`, because **no graph this sensor builds can originate
    /// one** — see `docs/qos-and-latency.md`.
    ///
    /// `None` when nothing was published: a stream producing no frames at all
    /// is the first-frame watchdog's story and `rtsp_connect_failed`'s, not
    /// this rule's, and `0/0` is not a degradation.
    shed_proportion: Option<f64>,
}

/// Sustained shedding above which `stream_degraded` fires: more than ~9 % of
/// what the graph produced never left the sink (a proportion of 1.1 is ten
/// produced for every nine published).
///
/// Deliberately not zero. `drop_on_full` is the *designed* behaviour of a live
/// sink — an isolated shed under a scheduling hiccup is the mechanism working,
/// and a rule that fired on it would be noise. At the ladder's usual 15-30 fps
/// a sustained 10 % is 1.5-3 frames a second gone for a whole interval, which
/// against a 30-frame GOP is visible stutter rather than a blip.
const SHED_PROPORTION_LIMIT: f64 = 1.1;

fn derive(prev: PrevCounters, now: PrevCounters, interval_secs: f64) -> TickDerived {
    let dframes = now.frames.saturating_sub(prev.frames);
    let dbytes = now.bytes.saturating_sub(prev.bytes);
    let denc_ns = now.encode_ns.saturating_sub(prev.encode_ns);
    let denc_frames = now.encoded_frames.saturating_sub(prev.encoded_frames);
    let dshed = now.drops.saturating_sub(prev.drops);
    TickDerived {
        fps: dframes as f64 / interval_secs,
        kbps: (dbytes as f64 * 8.0) / 1000.0 / interval_secs,
        encode_ms: (denc_frames > 0).then(|| denc_ns as f64 / denc_frames as f64 / 1e6),
        published: dframes,
        shed: dshed,
        // On a stream's *first* tick the baseline is zero, so `fps`
        // over-reports (it divides a stream's whole life by one interval) —
        // but the ratio does not, because both terms cover the same span. It
        // is honest from the very first point.
        shed_proportion: (dframes > 0).then(|| (dframes + dshed) as f64 / dframes as f64),
    }
}

/// Run the stats ticker: every `interval`, publish per-open-stream telemetry
/// plus the always-on `streams/advertised` gauge, and (when an alert sink is
/// wired) evaluate the encoder-overrun rule.
pub async fn run_ticker(
    publisher: Publisher,
    source: String,
    registry: StatsRegistry,
    advertised_streams: usize,
    interval: Duration,
    alerts: Option<Arc<crate::alerts::ParallaxAlerts>>,
    reports: Arc<crate::reports::ReceiverReports>,
) {
    let interval_secs = interval.as_secs_f64();
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev: HashMap<String, PrevCounters> = HashMap::new();

    loop {
        tick.tick().await;

        // Baseline presence gauge: the parallax host exists on the dashboard
        // even with zero open streams.
        publish(
            &publisher,
            &source,
            "streams/advertised",
            TelemetryValue::Gauge(advertised_streams as f64),
        )
        .await;

        // The receiver-feedback aggregate (#715), before the per-stream loop
        // and independent of the open set.
        publish_rx_aggregate(&publisher, &source, &reports).await;

        let open = registry.snapshot();
        // Forget closed streams' rate baselines.
        prev.retain(|k, _| open.iter().any(|(name, _)| name == k));

        for (stream, stats) in open {
            let now = PrevCounters {
                frames: stats.frames.load(Ordering::Relaxed),
                bytes: stats.bytes.load(Ordering::Relaxed),
                encode_ns: stats.encode_ns.load(Ordering::Relaxed),
                encoded_frames: stats.encoded_frames.load(Ordering::Relaxed),
                drops: stats.drops.load(Ordering::Relaxed),
            };
            let baseline = prev.insert(stream.clone(), now).unwrap_or_default();
            let derived = derive(baseline, now, interval_secs);

            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/fps"),
                TelemetryValue::Gauge(derived.fps),
            )
            .await;
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/kbps"),
                TelemetryValue::Gauge(derived.kbps),
            )
            .await;
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/drops"),
                TelemetryValue::Counter(stats.drops.load(Ordering::Relaxed)),
            )
            .await;
            // Unconditional: every profile on every path ends in an `AppSink`,
            // so an open stream always has a real backlog to report. `0` means
            // nothing is backed up (or nothing is open yet) — see the field's
            // note on why that is a weaker statement than a reading at the cap.
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/sink_queue"),
                TelemetryValue::Gauge(stats.sink_queue.load(Ordering::Relaxed) as f64),
            )
            .await;
            // Evaluated here rather than inside the encoder block below: a
            // graph can shed on a path that has no encoder at all (RTSP
            // passthrough), and that is exactly a case worth alerting on.
            if let Some(alerts) = &alerts
                && let Some(proportion) = derived.shed_proportion
            {
                alerts
                    .stream_degraded(
                        &stream,
                        proportion,
                        derived.shed,
                        derived.published,
                        proportion > SHED_PROPORTION_LIMIT,
                    )
                    .await;
            }
            // Omitted, not zeroed, for a stream with no rate-controlled
            // encoder — see `StreamStats::rc_tracked`.
            if let Some(rc) = stats.rc_drops() {
                publish(
                    &publisher,
                    &source,
                    &format!("{stream}/stats/rc_drops"),
                    TelemetryValue::Counter(rc),
                )
                .await;
            }
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/viewers"),
                TelemetryValue::Gauge(stats.viewers.load(Ordering::Relaxed) as f64),
            )
            .await;
            // Tail latencies, from the encoder's own histogram (#729). Kept
            // beside `encode_ms` rather than replacing it: the mean is an
            // interval figure the tail cannot give, and it is the only encode
            // timing the JPEG preview paths have at all.
            let tail_ms = stats.encode_tail_ms();
            if let Some((p95_ms, p99_ms)) = tail_ms {
                publish(
                    &publisher,
                    &source,
                    &format!("{stream}/stats/encode_p95_ms"),
                    TelemetryValue::Gauge(p95_ms),
                )
                .await;
                publish(
                    &publisher,
                    &source,
                    &format!("{stream}/stats/encode_p99_ms"),
                    TelemetryValue::Gauge(p99_ms),
                )
                .await;
            }

            if let Some(encode_ms) = derived.encode_ms {
                publish(
                    &publisher,
                    &source,
                    &format!("{stream}/stats/encode_ms"),
                    TelemetryValue::Gauge(encode_ms),
                )
                .await;

                // Encoder overrun: judged on the **tail** where one exists
                // (#729). A mean under budget with a p95 over it is exactly the
                // stream that stutters, and "overrun" is what the rule is
                // named for. The interval mean remains the fallback for the
                // JPEG preview paths, which are timed but have no histogram.
                if let Some(alerts) = &alerts {
                    let budget_ns = stats.budget_ns.load(Ordering::Relaxed);
                    if budget_ns > 0 {
                        let budget_ms = budget_ns as f64 / 1e6;
                        let judged = tail_ms.map_or(encode_ms, |(p95, _)| p95);
                        alerts
                            .encoder_overrun(
                                &stream,
                                tail_ms.map(|(p95, _)| p95),
                                encode_ms,
                                budget_ms,
                                judged > budget_ms,
                            )
                            .await;
                    }
                }
            }
        }
    }
}

/// Publish the receiver-feedback aggregate (#715).
///
/// Independent of the open set on purpose: a report for a tier that closed a
/// moment ago still surfaces once and then ages out, rather than vanishing at
/// exactly the moment an operator wants to know why it closed.
///
/// The timing families are **omitted, not zeroed**, when no live report carried
/// one — same discipline as `stats/rc_drops`, and RFC 07 §1.3's rule that an
/// unstamped stream's frame age is *not asked* rather than zero.
async fn publish_rx_aggregate(
    publisher: &Publisher,
    source: &str,
    reports: &crate::reports::ReceiverReports,
) {
    for agg in reports.aggregate(std::time::Instant::now()) {
        let base = format!("{}/rx/{}", agg.stream, agg.tier);
        publish(
            publisher,
            source,
            &format!("{base}/consumers"),
            TelemetryValue::Gauge(f64::from(agg.consumers)),
        )
        .await;
        for (leaf, value) in [
            ("loss_pct_max", agg.loss_pct_max),
            ("loss_pct_p50", agg.loss_pct_p50),
            ("frame_age_ms_max", agg.frame_age_ms_max),
            ("frame_age_ms_p50", agg.frame_age_ms_p50),
            ("decode_queue_max", agg.decode_queue_max),
            ("decode_queue_p50", agg.decode_queue_p50),
        ] {
            if let Some(value) = value {
                publish(
                    publisher,
                    source,
                    &format!("{base}/{leaf}"),
                    TelemetryValue::Gauge(value),
                )
                .await;
            }
        }
    }
}

async fn publish(publisher: &Publisher, source: &str, metric: &str, value: TelemetryValue) {
    let point = crate::telemetry_guard::checked_point(source, metric, value);
    if let Err(e) = publisher.publish(metric, &point).await {
        tracing::warn!(error = %e, metric = %metric, "failed to publish stream stats");
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_encode_tail_is_reported_only_once_an_encoder_has_one() {
        let stats = StreamStats::default();
        assert_eq!(
            stats.encode_tail_ms(),
            None,
            "a preview-only or RTSP-passthrough stream has no histogram to ask"
        );

        stats.set_encode_tail(4_500_000, 9_000_000);
        assert_eq!(stats.encode_tail_ms(), Some((4.5, 9.0)));

        // A store, not an accumulate: the worst live tier is written whole
        // each tick, so a torn-down tier's tail stops being reported.
        stats.set_encode_tail(1_000_000, 2_000_000);
        assert_eq!(stats.encode_tail_ms(), Some((1.0, 2.0)));
        stats.set_encode_tail(0, 0);
        assert_eq!(stats.encode_tail_ms(), None, "last encoder closed");
    }
    use super::*;

    #[test]
    fn derive_computes_rates() {
        let prev = PrevCounters {
            frames: 100,
            bytes: 1_000_000,
            encode_ns: 1_000_000_000,
            encoded_frames: 100,
            drops: 0,
        };
        let now = PrevCounters {
            frames: 150,
            bytes: 2_000_000,
            encode_ns: 1_500_000_000,
            encoded_frames: 150,
            drops: 0,
        };
        let d = derive(prev, now, 5.0);
        assert_eq!(d.fps, 10.0);
        assert_eq!(d.kbps, 1600.0);
        assert_eq!(d.encode_ms, Some(10.0));
    }

    #[test]
    fn derive_handles_no_encoded_frames() {
        let d = derive(PrevCounters::default(), PrevCounters::default(), 5.0);
        assert_eq!(d.fps, 0.0);
        assert_eq!(d.encode_ms, None, "no encoder frames → no encode_ms point");
    }

    /// parallax's own QoS quantity, `(processed + dropped) / processed`,
    /// computed from the sink's counters because no `AppSink` graph can
    /// originate an `Event::Qos` (#692).
    #[test]
    fn derive_computes_the_shed_proportion() {
        let healthy = derive(
            PrevCounters::default(),
            PrevCounters {
                frames: 100,
                ..PrevCounters::default()
            },
            5.0,
        );
        assert_eq!(healthy.published, 100);
        assert_eq!(healthy.shed, 0);
        assert_eq!(healthy.shed_proportion, Some(1.0), "keeping up");

        // 90 published, 10 shed: the graph produced 100 and 10 % never left.
        let shedding = derive(
            PrevCounters {
                frames: 10,
                drops: 5,
                ..PrevCounters::default()
            },
            PrevCounters {
                frames: 100,
                drops: 15,
                ..PrevCounters::default()
            },
            5.0,
        );
        assert_eq!(shedding.published, 90, "deltas, not absolutes");
        assert_eq!(shedding.shed, 10, "deltas, not absolutes");
        assert_eq!(shedding.shed_proportion, Some(100.0 / 90.0));
        assert!(
            shedding.shed_proportion.unwrap() > SHED_PROPORTION_LIMIT,
            "10 % sustained shedding is what the rule is for"
        );
    }

    /// A stream producing nothing at all is the first-frame watchdog's story
    /// and `rtsp_connect_failed`'s, not `stream_degraded`'s — and `0/0` is not
    /// a degradation. The tempting "simplification" here is a `0.0`, which
    /// would make a dead stream look perfectly healthy.
    #[test]
    fn no_frames_published_means_no_shed_proportion() {
        let d = derive(PrevCounters::default(), PrevCounters::default(), 5.0);
        assert_eq!(d.shed_proportion, None);
    }

    /// The `Counter` must never walk backwards. Several profiles feed one
    /// stream and a torn-down tier's replacement gets a fresh sink starting at
    /// zero, so folding absolutes would make it do exactly that.
    #[test]
    fn sink_drops_are_monotonic_across_a_profile_switch() {
        let stats = StreamStats::default();

        // One incarnation, observed twice.
        let mut seen = 0;
        stats.fold_sink_drops(&mut seen, 12);
        stats.fold_sink_drops(&mut seen, 20);
        assert_eq!(stats.drops.load(Ordering::Relaxed), 20);

        // Its replacement gets a fresh baseline and starts from zero again.
        let mut fresh = 0;
        stats.fold_sink_drops(&mut fresh, 3);
        assert_eq!(
            stats.drops.load(Ordering::Relaxed),
            23,
            "a new sink adds, it does not reset the stream's counter"
        );

        // And a handle that reset under us cannot subtract.
        stats.fold_sink_drops(&mut fresh, 1);
        assert_eq!(stats.drops.load(Ordering::Relaxed), 23);
    }

    /// A store, not an accumulate: a closed profile's backlog must stop being
    /// reported rather than sticking at its last value forever.
    #[test]
    fn the_sink_queue_is_a_store_not_an_accumulate() {
        let stats = StreamStats::default();
        stats.set_sink_queue(4);
        assert_eq!(stats.sink_queue.load(Ordering::Relaxed), 4);
        stats.set_sink_queue(0);
        assert_eq!(stats.sink_queue.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn registry_open_close_lifecycle() {
        let registry = StatsRegistry::default();
        let a = registry.handle("cam0");
        let a2 = registry.handle("cam0");
        a.record_frame(100);
        assert_eq!(a2.frames.load(Ordering::Relaxed), 1, "same shared handle");
        assert_eq!(registry.snapshot().len(), 1);
        registry.remove("cam0");
        assert!(registry.snapshot().is_empty());
    }

    /// An RTSP passthrough or a preview-only stream has no rate control, and
    /// must report *nothing* rather than a zero that reads as "the cap is not
    /// biting".
    #[test]
    fn rc_drops_absent_without_an_encoder() {
        let stats = StreamStats::default();
        assert_eq!(stats.rc_drops(), None, "no encoder attached → no point");
        stats.track_rc();
        assert_eq!(stats.rc_drops(), Some(0), "attached but nothing shed yet");
    }

    /// The published value is a `Counter`, and a tier switch hands the stream a
    /// *fresh* encoder handle that restarts at zero. Folding absolutes would
    /// walk the counter backwards; folding deltas cannot.
    #[test]
    fn fold_rc_drops_is_monotonic_across_a_tier_switch() {
        let stats = StreamStats::default();
        stats.track_rc();

        // One tier's incarnation, observed twice.
        let mut seen = 0;
        stats.fold_rc_drops(&mut seen, 5);
        assert_eq!(stats.rc_drops(), Some(5));
        stats.fold_rc_drops(&mut seen, 7);
        assert_eq!(stats.rc_drops(), Some(7));

        // Its replacement starts its own handle from zero.
        let mut fresh = 0;
        stats.fold_rc_drops(&mut fresh, 2);
        assert_eq!(
            stats.rc_drops(),
            Some(9),
            "the stream total only ever grows"
        );

        // Defensive: a handle that went backwards under us adds nothing.
        stats.fold_rc_drops(&mut fresh, 1);
        assert_eq!(stats.rc_drops(), Some(9));
    }

    #[test]
    fn budget_keeps_strictest() {
        let stats = StreamStats::default();
        stats.tighten_budget(0);
        assert_eq!(stats.budget_ns.load(Ordering::Relaxed), 0);
        stats.tighten_budget(500_000_000);
        stats.tighten_budget(66_666_666);
        stats.tighten_budget(500_000_000); // looser: ignored
        assert_eq!(stats.budget_ns.load(Ordering::Relaxed), 66_666_666);
    }
}
