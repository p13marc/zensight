//! Where a chart's history comes from: the fleet, or this viewer's cache
//! (#909).
//!
//! Both answer the same question and neither is a substitute for the other.
//!
//! The **local cache** ([`zensight_store`]) holds what *this* viewer saw while
//! it was running. On the reference fleet the GUI is open for minutes a week,
//! so what it holds is mostly gaps — which is the whole reason the historian
//! exists (#898).
//!
//! The **fleet history** is a headless service that has been subscribing the
//! whole time. It is the better answer whenever it is there, and it is not
//! always there: a deployment may not run one, and the one it runs may be
//! down. Falling back is not a degradation to hide — it is the honest state,
//! and the banner says so rather than letting an operator read a five-minute
//! chart as five minutes of history.
//!
//! Series names are the same on both sides, which is what makes the fallback
//! possible at all: since #904 the store keys a series by
//! `(origin, producer, subject)` — the wire key minus the class chunk — and
//! the historian writes the same tiers through the same crate.

use std::collections::HashMap;

use zensight_common::history::{RangeReply, RangeSeries};
use zensight_store::Sample;

/// Which side answered, for the banner and for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistorySource {
    /// A historian answered: the fleet's history, kept whether this GUI was
    /// running or not.
    Fleet,
    /// No historian is alive, so this is what this viewer happened to see.
    #[default]
    Local,
}

impl HistorySource {
    /// The caveat to show, or `None` when there is nothing to caveat.
    ///
    /// Phrased as what the reader is looking at rather than what is broken:
    /// a deployment with no historian is not in a fault state, and telling an
    /// operator their fleet is "unavailable" when they never deployed one
    /// would be crying wolf. What matters either way is that the window on
    /// screen is one viewer's, and might be short.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            HistorySource::Fleet => None,
            HistorySource::Local => {
                Some("Fleet history unavailable — showing this viewer's local cache only")
            }
        }
    }
}

/// The tier-appropriate `step`, in seconds, for a window and a chart width.
///
/// Two bounds, and the coarser wins. The **pixel** bound is that asking for
/// more points than the chart can draw is work nobody sees. The **tier** bound
/// is that the historian clamps `step` to a tier anyway, so requesting one
/// second across a month gets hour buckets whatever this says — and a caller
/// that did not expect that reads a coarse chart as a fine one.
///
/// The floor of 1 s is the per-second ring; the 2-day boundary is where the
/// minute tier's retention ends and **the hour tier is finally read**, which
/// nothing in this GUI had ever done before #909.
pub fn step_for(from_ms: i64, to_ms: i64, chart_px: u32) -> i64 {
    let span_s = ((to_ms - from_ms).max(0) / 1_000).max(1);
    let px = chart_px.max(1) as i64;
    let by_pixels = (span_s / px).max(1);
    // Two days is the minute tier's retention; past it only the hour tier has
    // anything, and asking for minutes would return a sparse left-hand edge
    // that looks like an outage.
    let by_tier = if span_s > 2 * 86_400 {
        3_600
    } else if span_s > 3_600 {
        60
    } else {
        1
    };
    by_pixels.max(by_tier)
}

/// Merge the replies of several historians into one set of series.
///
/// Several may answer a fleet selector — one per site is the expected
/// deployment (RFC 05 §2.1) — and where two hold the same series they should
/// agree, because they ingested the same samples. Where they do not, **the
/// first reply wins and the disagreement is logged**: silently interleaving
/// two versions of one series would draw a chart that is neither, and picking
/// the "better" one means inventing a rule about which historian is more
/// trustworthy, which nothing on the wire supports.
///
/// Keyed by `(origin, producer, subject)` — the series identity — so two
/// historians holding the same series collide, and two holding *different*
/// series do not.
pub fn merge_replies(replies: Vec<RangeReply>) -> Vec<RangeSeries> {
    let mut seen: HashMap<(String, String, String), String> = HashMap::new();
    let mut out: Vec<RangeSeries> = Vec::new();
    for reply in replies {
        for series in reply.series {
            let id = (
                series.origin.clone(),
                series.producer.clone(),
                series.subject.clone(),
            );
            match seen.get(&id) {
                Some(first) if *first != reply.historian => {
                    tracing::warn!(
                        origin = %series.origin, producer = %series.producer,
                        subject = %series.subject, first = %first,
                        also = %reply.historian,
                        "two historians hold this series; keeping the first reply's \
                         points. They ingested the same bus, so a disagreement is \
                         worth looking at — a shorter retention, a later start, or a \
                         narrowed key_expr on one of them"
                    );
                    continue;
                }
                Some(_) => continue,
                None => {
                    seen.insert(id, reply.historian.clone());
                    out.push(series);
                }
            }
        }
    }
    out
}

/// Project merged series onto the `(metric_name, samples)` pairs the device
/// chart seeds from, keeping only the ones belonging to `source`.
///
/// The metric name comes from the reply rather than from the subject: for a
/// proxy producer the subject leads with a device chunk, and reconstructing
/// the name from it means knowing which producers are proxies and how their
/// devices are slugged. The store recorded that at ingest; re-deriving it here
/// is how two consumers come to disagree.
pub fn series_for_device(series: Vec<RangeSeries>, source: &str) -> Vec<(String, Vec<Sample>)> {
    series
        .into_iter()
        .filter(|s| s.source.as_deref() == Some(source))
        .filter_map(|s| {
            let name = s.metric.clone().unwrap_or_else(|| s.subject.clone());
            let samples: Vec<Sample> = s
                .points
                .iter()
                .map(|(ts, value)| Sample {
                    ts: *ts,
                    value: *value,
                })
                .collect();
            (!samples.is_empty()).then_some((name, samples))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::history::{Aggregate, SeriesKind};

    fn series(origin: &str, subject: &str, source: &str, pts: &[(i64, f64)]) -> RangeSeries {
        RangeSeries {
            origin: origin.into(),
            producer: "sysinfo".into(),
            subject: subject.into(),
            kind: SeriesKind::Gauge,
            unit: None,
            agg: Aggregate::Avg,
            source: Some(source.into()),
            metric: Some(subject.into()),
            points: pts.to_vec(),
        }
    }

    fn reply(historian: &str, series: Vec<RangeSeries>) -> RangeReply {
        RangeReply {
            historian: historian.into(),
            from: 0,
            to: 1_000,
            step_s: 60,
            truncated: false,
            next_cursor: None,
            series,
        }
    }

    /// Two historians holding different series contribute both; holding the
    /// same one, the first wins. Interleaving two versions of one series would
    /// draw a chart that is neither.
    #[test]
    fn overlapping_historians_do_not_interleave_a_series() {
        let a = reply(
            "h-aaaaaaaaaaaa",
            vec![
                series("h-111111111111", "cpu", "host1", &[(0, 1.0)]),
                series("h-111111111111", "mem", "host1", &[(0, 2.0)]),
            ],
        );
        let b = reply(
            "h-bbbbbbbbbbbb",
            vec![
                // The same series, different points — a disagreement.
                series("h-111111111111", "cpu", "host1", &[(0, 99.0)]),
                // …and one only this historian holds.
                series("h-222222222222", "cpu", "host2", &[(0, 3.0)]),
            ],
        );
        let merged = merge_replies(vec![a, b]);
        assert_eq!(
            merged.len(),
            3,
            "two from the first, the unique one from the second"
        );
        let cpu1 = merged
            .iter()
            .find(|s| s.origin == "h-111111111111" && s.subject == "cpu")
            .unwrap();
        assert_eq!(cpu1.points, vec![(0, 1.0)], "the first reply's points win");
        assert!(merged.iter().any(|s| s.origin == "h-222222222222"));
    }

    /// One historian answering twice is not a conflict — a fan-in can deliver
    /// a reply per matching key, and dropping the second silently is right.
    #[test]
    fn one_historian_answering_twice_is_not_a_conflict() {
        let s = series("h-111111111111", "cpu", "host1", &[(0, 1.0)]);
        let merged = merge_replies(vec![
            reply("h-aaaaaaaaaaaa", vec![s.clone()]),
            reply("h-aaaaaaaaaaaa", vec![s]),
        ]);
        assert_eq!(merged.len(), 1);
    }

    /// The device filter is by `source`, not by origin: one host's sysinfo and
    /// one switch polled by that host's snmp share an origin and are two
    /// devices (#474).
    #[test]
    fn only_the_asked_for_device_seeds_the_chart() {
        let merged = vec![
            series("h-111111111111", "cpu", "host1", &[(0, 1.0)]),
            series("h-111111111111", "cpu", "sw1", &[(0, 9.0)]),
            // An empty series contributes nothing rather than an empty line.
            series("h-111111111111", "mem", "host1", &[]),
        ];
        let out = series_for_device(merged, "host1");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "cpu");
        assert_eq!(out[0].1.len(), 1);
        assert_eq!(out[0].1[0].value, 1.0);
    }

    /// The step is the coarser of the pixel bound and the tier bound. The tier
    /// bound is the one that matters: the historian clamps `step` to a tier
    /// anyway, so a caller that asked for seconds across a month would read
    /// hour buckets as if they were seconds.
    #[test]
    fn the_step_never_asks_for_finer_than_the_tier_holds() {
        let hour = 3_600_000i64;
        let day = 24 * hour;

        // Ten minutes on a wide chart: the per-second ring.
        assert_eq!(step_for(0, 10 * 60_000, 1_000), 1);
        // A day: the minute tier, and the pixel bound is coarser here.
        assert!(step_for(0, day, 800) >= 60);
        // Three days: past the minute tier's retention, so hours.
        assert!(step_for(0, 3 * day, 10_000) >= 3_600);
        // A narrow chart raises the step above the tier floor rather than
        // asking for points nobody can see.
        assert!(step_for(0, day, 10) > 60);
    }

    #[test]
    fn only_the_local_source_carries_a_caveat() {
        assert!(HistorySource::Fleet.caveat().is_none());
        assert!(HistorySource::Local.caveat().is_some());
        assert_eq!(HistorySource::default(), HistorySource::Local);
    }
}
