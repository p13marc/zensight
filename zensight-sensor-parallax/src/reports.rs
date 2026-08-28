//! Receiver feedback: `@rpc/parallax/stream/report` (#715, RFC 07 §1.1).
//!
//! # The §1.2 boundary is why this is its own module
//!
//! RFC 07 §1.2 is **normative**: a producer MUST NOT re-tune a shared tier from
//! one consumer's report, and where it acts on aggregate feedback it must state
//! an arbitration rule that is not "the most recent report". The failure it
//! forbids is concrete — two viewers share a tier, one reports loss, the
//! bitrate drops, and the *healthy* viewer's picture degrades for a reason it
//! cannot see, caused by a peer it does not know exists.
//!
//! [`crate::command`] takes a [`crate::session::SessionHandle`], because that
//! channel is how a `StreamControl` reaches the encoder. **This module takes an
//! [`Arc<ReceiverReports>`] and nothing else.** It does not `use crate::session`
//! except for the pure selector resolver, it holds no handle, no sender and no
//! [`crate::pipeline::PipelineControls`] — so the type that holds receiver
//! feedback has no path to the encoder knobs at all.
//!
//! That is deliberate, and it is the difference between a rule and a comment. A
//! comment saying "do not re-tune from a report" is obeyed until the next
//! person wires up something helpful; a module that cannot reach the knobs is
//! an invariant. `tests/rfc07_receiver_driven.rs` greps this file for the
//! control types and fails if one appears.
//!
//! # What it does instead
//!
//! Keeps the most recent report per `(consumer_id, stream, profile)`, bounded
//! and aged out, and folds them into a per-tier aggregate that
//! [`crate::stats`] publishes on `telemetry/parallax/{stream}/rx/{tier}/…`.
//! Feedback informs an operator and a receiver-side controller; it does not
//! drive an encoder.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zensight_common::command::stream_report_key;
use zensight_common::decode_auto;
use zensight_common::stream::MediaReceiverReport;

use crate::session::{Profile, resolve_profile_in};

/// The minimum interval between two reports from one consumer about one tier.
///
/// Declared in the registry as the procedure's `rate` ceiling, and pinned
/// against it by `tests/registry_conformance.rs`. RFC 07 §1.1's *reference*
/// cadence is one report per few seconds; this is the ceiling a misbehaving
/// viewer hits, not the cadence a well-behaved one should choose.
pub const REPORT_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Most reports kept for one `(stream, tier)`.
///
/// The key space is already bounded by the selector refusal — a consumer
/// cannot invent a tier name — so this bounds only the `consumer_id` dimension,
/// which is the one a caller controls.
const MAX_CONSUMERS_PER_TIER: usize = 64;

/// Longest a `consumer_id` may be. Length only: it is never slugged and never
/// placed in a key, because per-consumer telemetry is precisely the
/// viewer-origin design RFC 07 §1.1 rejected on its way to a payload field.
const MAX_CONSUMER_ID: usize = 64;

/// Why a report was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Malformed, or naming a key this producer does not serve.
    InvalidArgs(String),
    /// Over the rate ceiling for this `(consumer, stream, tier)`.
    Busy(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReportKey {
    consumer: String,
    stream: String,
    profile: Profile,
}

struct Stored {
    report: MediaReceiverReport,
    arrived: Instant,
}

/// One tier's folded view of every live report about it.
///
/// Every field is `Option` for the same reason the report's are: **absent is
/// not zero.** A tier whose consumers measured no frame age publishes no frame
/// age, rather than publishing a confident `0` that means "perfectly fresh".
#[derive(Debug, Clone, PartialEq)]
pub struct TierAggregate {
    pub stream: String,
    pub tier: String,
    /// Consumers with a live report. A **lower bound** on viewers: one that
    /// never reports is invisible here.
    pub consumers: u32,
    pub loss_pct_max: Option<f64>,
    pub loss_pct_p50: Option<f64>,
    pub frame_age_ms_max: Option<f64>,
    pub frame_age_ms_p50: Option<f64>,
    pub decode_queue_max: Option<f64>,
    pub decode_queue_p50: Option<f64>,
}

/// The bounded per-consumer receiver state.
pub struct ReceiverReports {
    inner: Mutex<HashMap<ReportKey, Stored>>,
    /// The tier reaper's own window — `ParallaxConfig::idle_timeout_secs`, the
    /// same field `ProfileSessionActor::reap_idle` reads. One field, two
    /// readers, and `the_report_window_is_the_tier_reaper_window` asserts they
    /// agree: a browser tab that closes never says goodbye, and the tier
    /// reaper already assumes that. Inventing a second window here would mean
    /// two different answers to "is this viewer gone".
    idle: Duration,
    tier_names: Vec<String>,
    default_tier: String,
}

impl ReceiverReports {
    pub fn new(idle: Duration, tier_names: Vec<String>, default_tier: String) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            idle,
            tier_names,
            default_tier,
        }
    }

    /// Build from the sensor's config, so the idle window cannot drift from
    /// the tier reaper's.
    pub fn from_config(config: &crate::config::ParallaxConfig) -> Self {
        Self::new(
            Duration::from_secs(config.idle_timeout_secs),
            config
                .video
                .tiers
                .iter()
                .map(|t| t.spec.name.clone())
                .collect(),
            config.video.default_tier.clone(),
        )
    }

    fn names(&self) -> Vec<&str> {
        self.tier_names.iter().map(String::as_str).collect()
    }

    /// Accept one report, or say why not.
    ///
    /// The refusals are not cosmetic. The selector refusal is what bounds the
    /// key space — without it a consumer could mint an unbounded set of tier
    /// names — and the `consumer_id` length check bounds the other dimension.
    ///
    /// A well-formed but internally inconsistent report (`decoded > received`)
    /// is **accepted**: that is a consumer lying about itself, which the
    /// aggregate should show, not a malformation the producer should hide.
    pub fn accept(&self, report: MediaReceiverReport, now: Instant) -> Result<(), Refusal> {
        if report.consumer_id.is_empty() || report.consumer_id.len() > MAX_CONSUMER_ID {
            return Err(Refusal::InvalidArgs(format!(
                "consumer_id must be 1..={MAX_CONSUMER_ID} bytes"
            )));
        }
        if report.interval_ms == 0 {
            return Err(Refusal::InvalidArgs(
                "interval_ms must be non-zero — a zero-span snapshot has no rate".into(),
            ));
        }
        let profile = resolve_profile_in(
            report.codec.as_deref(),
            report.tier.as_deref(),
            &self.names(),
            &self.default_tier,
        )
        .ok_or_else(|| {
            Refusal::InvalidArgs(format!(
                "no offered profile for codec={:?} tier={:?}",
                report.codec, report.tier
            ))
        })?;

        let key = ReportKey {
            consumer: report.consumer_id.clone(),
            stream: report.stream.clone(),
            profile,
        };

        let mut map = self.inner.lock().unwrap();
        if let Some(prev) = map.get(&key)
            && now.duration_since(prev.arrived) < REPORT_MIN_INTERVAL
        {
            return Err(Refusal::Busy(format!(
                "at most one report per {}s per (consumer, stream, tier)",
                REPORT_MIN_INTERVAL.as_secs()
            )));
        }

        // Bound the consumer dimension. Evict the STALEST rather than refusing
        // the newcomer: turning away a live viewer to keep a dead tab's entry
        // is the wrong trade.
        let same_tier = |k: &ReportKey| k.stream == key.stream && k.profile == key.profile;
        if !map.contains_key(&key)
            && map.keys().filter(|k| same_tier(k)).count() >= MAX_CONSUMERS_PER_TIER
            && let Some(stalest) = map
                .iter()
                .filter(|(k, _)| same_tier(k))
                .min_by_key(|(_, v)| v.arrived)
                .map(|(k, _)| k.clone())
        {
            map.remove(&stalest);
        }

        map.insert(
            key,
            Stored {
                report,
                arrived: now,
            },
        );
        Ok(())
    }

    /// Drop reports whose consumer has gone quiet for the tier reaper's window.
    ///
    /// Called at the top of each aggregation pass rather than on a timer of its
    /// own: a departed consumer stops counting in the same tick it stops being
    /// live, and there is one clock instead of two.
    fn reap(&self, map: &mut HashMap<ReportKey, Stored>, now: Instant) {
        map.retain(|_, v| now.duration_since(v.arrived) < self.idle);
    }

    /// Fold the live reports into one aggregate per `(stream, tier)`.
    pub fn aggregate(&self, now: Instant) -> Vec<TierAggregate> {
        let mut map = self.inner.lock().unwrap();
        self.reap(&mut map, now);

        let names = self.names();
        let mut by_tier: HashMap<(String, String), Vec<&MediaReceiverReport>> = HashMap::new();
        for (key, stored) in map.iter() {
            let Some(tier) = key.profile.tier_label(&names) else {
                continue;
            };
            by_tier
                .entry((key.stream.clone(), tier))
                .or_default()
                .push(&stored.report);
        }

        let mut out: Vec<TierAggregate> = by_tier
            .into_iter()
            .map(|((stream, tier), reports)| {
                let loss: Vec<f64> = reports.iter().map(|r| loss_pct(r)).collect();
                let age_p50: Vec<f64> = reports
                    .iter()
                    .filter_map(|r| r.frame_age_ms.map(f64::from))
                    .collect();
                let age_max: Vec<f64> = reports
                    .iter()
                    .filter_map(|r| r.frame_age_max_ms.map(f64::from))
                    .collect();
                let queue: Vec<f64> = reports
                    .iter()
                    .filter_map(|r| r.decoder_queue_depth.map(f64::from))
                    .collect();
                TierAggregate {
                    stream,
                    tier,
                    consumers: reports.len() as u32,
                    loss_pct_max: max_of(&loss),
                    loss_pct_p50: median_of(&loss),
                    // max-of-maxes and median-of-medians: each has a defined
                    // meaning, which is why the report carries both and not one
                    // scalar a producer would have to guess at.
                    frame_age_ms_max: max_of(&age_max),
                    frame_age_ms_p50: median_of(&age_p50),
                    decode_queue_max: max_of(&queue),
                    decode_queue_p50: median_of(&queue),
                }
            })
            .collect();
        out.sort_by(|a, b| (&a.stream, &a.tier).cmp(&(&b.stream, &b.tier)));
        out
    }
}

/// `lost / (received + lost)` as a percentage; 0 when nothing arrived and
/// nothing was lost, which is a real observation rather than a missing one.
fn loss_pct(r: &MediaReceiverReport) -> f64 {
    let denom = r.received_frames.saturating_add(r.lost_frames);
    if denom == 0 {
        return 0.0;
    }
    r.lost_frames as f64 * 100.0 / denom as f64
}

/// `None` on an empty slice — the caller publishes nothing rather than zero.
fn max_of(v: &[f64]) -> Option<f64> {
    v.iter().copied().fold(None, |acc: Option<f64>, x| {
        Some(acc.map_or(x, |a| a.max(x)))
    })
}

/// `None` on an empty slice. Lower median on an even count: a deterministic
/// choice beats an interpolated value nobody reported.
fn median_of(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(s[(s.len() - 1) / 2])
}

/// Serve `@rpc/parallax/stream/report` until the session closes.
///
/// Takes `reports` and nothing else. See the module header.
pub async fn run(session: Arc<zenoh::Session>, producer: String, reports: Arc<ReceiverReports>) {
    let key = stream_report_key(&producer);
    let queryable = match zensight_common::served::serve_queryable(&session, &key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "stream/report: failed to declare queryable");
            return;
        }
    };
    tracing::info!(reports = %key, "stream/report: receiver feedback ready");

    while let Ok(query) = queryable.recv_async().await {
        let payload = query
            .payload()
            .map(|p| p.to_bytes().to_vec())
            .unwrap_or_default();
        let outcome = match decode_auto::<MediaReceiverReport>(&payload) {
            Ok(report) => reports.accept(report, Instant::now()),
            Err(e) => Err(Refusal::InvalidArgs(e.to_string())),
        };
        match outcome {
            Ok(()) => {
                if let Err(e) = query.reply(key.as_str(), Vec::<u8>::new()).await {
                    tracing::warn!(error = %e, "stream/report: failed to ack");
                }
            }
            Err(refusal) => {
                let err = match &refusal {
                    Refusal::InvalidArgs(m) => zensight_common::rpc::RpcError::invalid_args(m),
                    Refusal::Busy(m) => zensight_common::rpc::RpcError::busy(m),
                };
                tracing::debug!(?refusal, "stream/report: refused");
                let _ = query
                    .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ParallaxConfig;

    fn store() -> ReceiverReports {
        ReceiverReports::new(
            Duration::from_secs(30),
            vec!["low".into(), "high".into()],
            "high".into(),
        )
    }

    fn report(consumer: &str, tier: Option<&str>) -> MediaReceiverReport {
        MediaReceiverReport {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            tier: tier.map(str::to_string),
            consumer_id: consumer.into(),
            interval_ms: 1000,
            received_frames: 100,
            lost_frames: 0,
            dropped_frames: 0,
            decoded_frames: 100,
            last_sequence: 100,
            interarrival_jitter_ms: None,
            frame_age_ms: None,
            frame_age_max_ms: None,
            decoder_queue_depth: None,
            last_keyframe_sequence: None,
            since_last_keyframe_ms: None,
        }
    }

    #[test]
    fn a_second_report_replaces_the_first_for_one_consumer_tier() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("c1", Some("high")), t0).unwrap();
        let mut later = report("c1", Some("high"));
        later.lost_frames = 5;
        s.accept(later, t0 + Duration::from_secs(2)).unwrap();

        let agg = s.aggregate(t0 + Duration::from_secs(2));
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].consumers, 1, "one consumer, not two entries");
        assert!(agg[0].loss_pct_max.unwrap() > 0.0, "the latest report wins");
    }

    #[test]
    fn an_idle_consumer_ages_out_on_the_tier_reaper_window() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("c1", Some("high")), t0).unwrap();
        assert_eq!(s.aggregate(t0 + Duration::from_secs(29)).len(), 1);
        assert!(
            s.aggregate(t0 + Duration::from_secs(31)).is_empty(),
            "a browser tab that closes never says goodbye"
        );
    }

    /// The report window and the tier reaper's window are the same config
    /// field, so "is this viewer gone" has one answer and not two.
    #[test]
    fn the_report_window_is_the_tier_reaper_window() {
        let config = ParallaxConfig::default();
        let s = ReceiverReports::from_config(&config);
        assert_eq!(
            s.idle,
            Duration::from_secs(config.idle_timeout_secs),
            "reports must age out on ProfileSessionActor::reap_idle's window"
        );
    }

    #[test]
    fn the_map_is_bounded_per_tier_and_evicts_the_stalest() {
        let s = store();
        let t0 = Instant::now();
        // Fill the tier, oldest first.
        for i in 0..MAX_CONSUMERS_PER_TIER {
            s.accept(
                report(&format!("c{i}"), Some("high")),
                t0 + Duration::from_millis(i as u64),
            )
            .unwrap();
        }
        let at = t0 + Duration::from_secs(2);
        s.accept(report("newcomer", Some("high")), at).unwrap();

        let agg = s.aggregate(at);
        assert_eq!(
            agg[0].consumers as usize, MAX_CONSUMERS_PER_TIER,
            "bounded: the newcomer displaced one rather than growing the map"
        );
        let map = s.inner.lock().unwrap();
        assert!(
            map.keys().any(|k| k.consumer == "newcomer"),
            "the live viewer is kept"
        );
        assert!(
            !map.keys().any(|k| k.consumer == "c0"),
            "the stalest is what gets evicted"
        );
    }

    #[test]
    fn an_over_rate_report_is_refused_and_not_stored() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("c1", Some("high")), t0).unwrap();
        let mut spam = report("c1", Some("high"));
        spam.lost_frames = 99;
        let err = s
            .accept(spam, t0 + Duration::from_millis(100))
            .expect_err("over-rate");
        assert!(matches!(err, Refusal::Busy(_)));
        assert_eq!(
            s.aggregate(t0 + Duration::from_millis(100))[0].loss_pct_max,
            Some(0.0),
            "a refused report must not reach the aggregate"
        );
    }

    /// The selector refusal is what bounds the key space: a consumer cannot
    /// mint tier names, so the map cannot grow in that dimension.
    #[test]
    fn an_unresolvable_selector_is_refused() {
        let s = store();
        let t0 = Instant::now();
        assert!(matches!(
            s.accept(report("c1", Some("ultra")), t0),
            Err(Refusal::InvalidArgs(_))
        ));
        let mut bad_codec = report("c1", None);
        bad_codec.codec = Some("av1".into());
        assert!(matches!(
            s.accept(bad_codec, t0),
            Err(Refusal::InvalidArgs(_))
        ));
        assert!(s.aggregate(t0).is_empty());
    }

    #[test]
    fn an_empty_or_oversized_consumer_id_is_refused() {
        let s = store();
        let t0 = Instant::now();
        assert!(matches!(
            s.accept(report("", Some("high")), t0),
            Err(Refusal::InvalidArgs(_))
        ));
        let long = "x".repeat(MAX_CONSUMER_ID + 1);
        assert!(matches!(
            s.accept(report(&long, Some("high")), t0),
            Err(Refusal::InvalidArgs(_))
        ));
    }

    #[test]
    fn a_zero_interval_is_refused() {
        let s = store();
        let mut r = report("c1", Some("high"));
        r.interval_ms = 0;
        assert!(matches!(
            s.accept(r, Instant::now()),
            Err(Refusal::InvalidArgs(_))
        ));
    }

    /// `None` tier resolves to the default, so the same viewer reporting both
    /// spellings is one entry rather than two.
    #[test]
    fn the_default_tier_and_its_name_are_one_key() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("c1", None), t0).unwrap();
        let err = s.accept(report("c1", Some("high")), t0);
        assert!(
            matches!(err, Err(Refusal::Busy(_))),
            "None and \"high\" are the same tier, so the second is a re-report"
        );
        let agg = s.aggregate(t0);
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].tier, "high");
    }

    /// Absent is not zero, all the way through the fold (RFC 07 §1.3).
    #[test]
    fn frame_age_is_omitted_from_the_aggregate_when_nobody_measured_it() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("c1", Some("high")), t0).unwrap();
        let agg = s.aggregate(t0);
        assert_eq!(agg[0].consumers, 1);
        assert_eq!(agg[0].loss_pct_max, Some(0.0), "loss IS measurable");
        assert_eq!(agg[0].frame_age_ms_max, None, "not zero — not measured");
        assert_eq!(agg[0].frame_age_ms_p50, None);
        assert_eq!(agg[0].decode_queue_max, None);
    }

    #[test]
    fn a_negative_frame_age_reaches_the_aggregate_unclamped() {
        let s = store();
        let t0 = Instant::now();
        let mut r = report("c1", Some("high"));
        r.frame_age_ms = Some(-12.5);
        r.frame_age_max_ms = Some(-3.0);
        s.accept(r, t0).unwrap();
        let agg = s.aggregate(t0);
        assert_eq!(agg[0].frame_age_ms_p50, Some(-12.5));
        assert_eq!(agg[0].frame_age_ms_max, Some(-3.0));
    }

    /// A max and a median, each meaning what it says: one bad consumer must
    /// move the max and not the median.
    #[test]
    fn one_bad_consumer_moves_the_max_and_not_the_median() {
        let s = store();
        let t0 = Instant::now();
        for (i, lost) in [0u64, 0, 50].into_iter().enumerate() {
            let mut r = report(&format!("c{i}"), Some("high"));
            r.lost_frames = lost;
            s.accept(r, t0).unwrap();
        }
        let agg = s.aggregate(t0);
        assert_eq!(agg[0].consumers, 3);
        assert!(agg[0].loss_pct_max.unwrap() > 30.0, "the worst case shows");
        assert_eq!(agg[0].loss_pct_p50, Some(0.0), "the typical case does not");
    }

    /// A `consumer_id` is payload-only and must never reach a key
    /// (RFC 07 §1.1) — the whole reason the feedback surface costs no keyspace.
    #[test]
    fn a_consumer_id_never_reaches_a_key() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("secret-viewer-id", Some("high")), t0)
            .unwrap();
        for agg in s.aggregate(t0) {
            let key = format!("{}/rx/{}", agg.stream, agg.tier);
            assert!(!key.contains("secret-viewer-id"), "{key}");
        }
    }

    #[test]
    fn two_tiers_of_one_stream_aggregate_separately() {
        let s = store();
        let t0 = Instant::now();
        s.accept(report("c1", Some("high")), t0).unwrap();
        let mut low = report("c2", Some("low"));
        low.lost_frames = 10;
        s.accept(low, t0).unwrap();

        let agg = s.aggregate(t0);
        assert_eq!(agg.len(), 2, "per tier, not per stream");
        let low = agg.iter().find(|a| a.tier == "low").unwrap();
        let high = agg.iter().find(|a| a.tier == "high").unwrap();
        assert!(low.loss_pct_max.unwrap() > 0.0);
        assert_eq!(high.loss_pct_max, Some(0.0));
    }

    #[test]
    fn the_preview_reports_under_its_own_tier_label() {
        let s = store();
        let t0 = Instant::now();
        let mut preview = report("c1", None);
        preview.codec = Some("jpeg".into());
        s.accept(preview, t0).unwrap();
        assert_eq!(s.aggregate(t0)[0].tier, "preview");
    }
}
