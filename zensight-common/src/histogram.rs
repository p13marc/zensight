//! The histogram value (#1151): a fixed-bucket distribution on the bus.
//!
//! RFC 08 §2 v1.36 (zenkey 0.9) ratified `kind = "histogram"` with declared
//! `buckets`, and left the payload shape to the application profile (RFC 11
//! §4). This is that shape, and the reason it exists: a latency distribution
//! had nowhere to go but N quantile gauges (`rtt_p95_ms`, `rtt_min_ms`, …),
//! which cannot be aggregated across hosts — averaging quantiles is the
//! classic wrong answer — and whose boundaries were declared nowhere.
//!
//! # The shape
//!
//! ```json
//! {"type": "histogram",
//!  "value": {"buckets": [0.005, 0.01, 0.025],
//!            "counts":  [3, 10, 2, 1],
//!            "count": 16, "sum": 0.19}}
//! ```
//!
//! - `buckets` — the upper bounds, strictly ascending and finite, `+Inf`
//!   implicit. **Equal to the subject's declared `buckets`**, bit for bit:
//!   that is what makes two producers of one subject comparable, what
//!   `registry::kind_matches` checks at the publish site, and what zenkey's
//!   `kind-mismatch` judge checks on the wire.
//! - `counts` — observations per bucket, **not** cumulative, one more than
//!   `buckets`: `counts[i]` falls in `(buckets[i-1], buckets[i]]`, the last
//!   is the `+Inf` overflow. The OTLP explicit-bucket layout; Prometheus's
//!   cumulative `le` series are derived from it ([`Self::cumulative`]).
//! - `count` = Σ `counts`, `sum` = the sum of every observed value.
//!
//! **Cumulative since the producer started**, like a counter: every count
//! only grows, and a producer restart resets the whole value — visible, as
//! for a counter, as the origin's `alive` token cycling (RFC 08 §2). A
//! consumer that wants a window diffs two values ([`Self::delta_since`]).

use serde::{Deserialize, Serialize};

/// A fixed-bucket distribution, cumulative since the producer started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistogramValue {
    /// Upper bounds, strictly ascending and finite; `+Inf` implicit. Equal to
    /// the subject's declared `buckets`.
    pub buckets: Vec<f64>,
    /// Observations per bucket, not cumulative; `buckets.len() + 1` entries,
    /// the last the `+Inf` overflow.
    pub counts: Vec<u64>,
    /// Total observations — the sum of `counts`.
    pub count: u64,
    /// The sum of every observed value.
    pub sum: f64,
}

impl HistogramValue {
    /// An empty histogram over `buckets` (the subject's declared bounds).
    pub fn new(buckets: &[f64]) -> Self {
        HistogramValue {
            buckets: buckets.to_vec(),
            counts: vec![0; buckets.len() + 1],
            count: 0,
            sum: 0.0,
        }
    }

    /// Record one observation. A non-finite value is refused (and returns
    /// `false`): it has no bucket, and it would poison `sum` for every
    /// consumer downstream.
    pub fn observe(&mut self, v: f64) -> bool {
        if !v.is_finite() {
            return false;
        }
        // `(buckets[i-1], buckets[i]]`: the first bound that is >= v.
        let i = self.buckets.partition_point(|b| *b < v);
        self.counts[i] += 1;
        self.count += 1;
        self.sum += v;
        true
    }

    /// Whether the value is well-formed: bounds strictly ascending and
    /// finite, one more count than bounds, `count` their sum, `sum` finite.
    /// A consumer refuses an inconsistent value rather than rendering it.
    pub fn is_consistent(&self) -> bool {
        self.buckets.iter().all(|b| b.is_finite())
            && self.buckets.windows(2).all(|w| w[0] < w[1])
            && self.counts.len() == self.buckets.len() + 1
            && self.counts.iter().try_fold(0u64, |a, c| a.checked_add(*c)) == Some(self.count)
            && self.sum.is_finite()
    }

    /// Whether `other` is over the same bounds, bit for bit.
    pub fn same_bounds(&self, bounds: &[f64]) -> bool {
        self.buckets.len() == bounds.len()
            && self
                .buckets
                .iter()
                .zip(bounds)
                .all(|(a, b)| a.to_bits() == b.to_bits())
    }

    /// Prometheus's `le` view: the cumulative count at each bound, then the
    /// `+Inf` total — `buckets.len() + 1` entries, the last equal to `count`.
    pub fn cumulative(&self) -> Vec<u64> {
        let mut acc = 0u64;
        self.counts
            .iter()
            .map(|c| {
                acc = acc.saturating_add(*c);
                acc
            })
            .collect()
    }

    /// What was observed between `prev` and `self`, when `self` continues
    /// `prev`: same bounds and no count went down. `None` when it does not —
    /// a producer restart (every count reset) or a different declaration —
    /// in which case `self` *is* the window since the reset, and the caller
    /// uses it whole. The same rule a counter's rate applies.
    pub fn delta_since(&self, prev: &HistogramValue) -> Option<HistogramValue> {
        if !self.same_bounds(&prev.buckets) || self.counts.len() != prev.counts.len() {
            return None;
        }
        let counts: Option<Vec<u64>> = self
            .counts
            .iter()
            .zip(&prev.counts)
            .map(|(now, before)| now.checked_sub(*before))
            .collect();
        let counts = counts?;
        Some(HistogramValue {
            buckets: self.buckets.clone(),
            count: counts.iter().sum(),
            counts,
            sum: self.sum - prev.sum,
        })
    }

    /// The mean observed value, when anything was observed.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// An **estimate** of the `q`-quantile (0 ≤ q ≤ 1), by linear
    /// interpolation inside the bucket that holds it — Prometheus's
    /// `histogram_quantile` rule. The first bucket's lower edge is `0` when
    /// its bound is positive (the usual case: latencies, sizes); a quantile
    /// that lands in the `+Inf` bucket is reported as the last finite bound,
    /// which is a floor, not a value. `None` when nothing was observed.
    ///
    /// An estimate is only as fine as the buckets: it is the reason a
    /// renderer labels it `≈`, and never the reason to publish quantile
    /// gauges beside the histogram.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 || !(0.0..=1.0).contains(&q) {
            return None;
        }
        let rank = q * self.count as f64;
        let cumulative = self.cumulative();
        let i = cumulative.iter().position(|c| *c as f64 >= rank)?;
        if i == self.buckets.len() {
            // In the overflow bucket: the last finite bound is all we know.
            return self.buckets.last().copied();
        }
        let upper = self.buckets[i];
        let lower = if i == 0 {
            if upper > 0.0 { 0.0 } else { upper }
        } else {
            self.buckets[i - 1]
        };
        let below = if i == 0 { 0 } else { cumulative[i - 1] };
        let in_bucket = self.counts[i];
        if in_bucket == 0 {
            return Some(upper);
        }
        let fraction = (rank - below as f64) / in_bucket as f64;
        Some(lower + (upper - lower) * fraction.clamp(0.0, 1.0))
    }

    /// One line for a table cell or a log: the count, the mean, and the
    /// estimated median and p95 — each estimate marked `≈`.
    pub fn summary(&self, unit: Option<&str>) -> String {
        if self.count == 0 {
            return "no observations".to_string();
        }
        let u = unit.map(|u| format!(" {u}")).unwrap_or_default();
        let fmt = |v: f64| {
            if v.abs() >= 100.0 || v == v.trunc() {
                format!("{v:.0}")
            } else if v.abs() >= 1.0 {
                format!("{v:.2}")
            } else {
                format!("{v:.4}")
            }
        };
        let mean = self.mean().map(fmt).unwrap_or_default();
        let p50 = self.quantile(0.5).map(fmt).unwrap_or_default();
        let p95 = self.quantile(0.95).map(fmt).unwrap_or_default();
        format!(
            "n={} · mean {mean}{u} · p50 ≈{p50}{u} · p95 ≈{p95}{u}",
            self.count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: [f64; 3] = [0.01, 0.1, 1.0];

    #[test]
    fn observe_places_each_value_in_its_upper_inclusive_bucket() {
        let mut h = HistogramValue::new(&B);
        for v in [0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 7.0] {
            assert!(h.observe(v));
        }
        // (−∞,0.01] (0.01,0.1] (0.1,1] (1,+Inf)
        assert_eq!(h.counts, vec![2, 2, 2, 1]);
        assert_eq!(h.count, 7);
        assert!((h.sum - 8.665).abs() < 1e-9);
        assert!(h.is_consistent());
        assert_eq!(h.cumulative(), vec![2, 4, 6, 7]);
        assert!(!h.observe(f64::NAN), "a non-finite value has no bucket");
        assert_eq!(h.count, 7);
    }

    #[test]
    fn consistency_catches_every_way_a_value_can_be_malformed() {
        let ok = HistogramValue {
            buckets: B.to_vec(),
            counts: vec![1, 0, 0, 0],
            count: 1,
            sum: 0.005,
        };
        assert!(ok.is_consistent());
        let mut bad = ok.clone();
        bad.count = 2;
        assert!(!bad.is_consistent(), "count must be the sum of counts");
        let mut bad = ok.clone();
        bad.counts.pop();
        assert!(
            !bad.is_consistent(),
            "one count per bucket plus the overflow"
        );
        let mut bad = ok.clone();
        bad.buckets = vec![0.1, 0.01, 1.0];
        assert!(!bad.is_consistent(), "bounds ascend");
        let mut bad = ok;
        bad.sum = f64::INFINITY;
        assert!(!bad.is_consistent());
    }

    #[test]
    fn delta_since_is_the_window_or_none_across_a_reset() {
        let mut a = HistogramValue::new(&B);
        a.observe(0.005);
        let mut b = a.clone();
        b.observe(0.5);
        b.observe(0.05);
        let d = b.delta_since(&a).expect("continues");
        assert_eq!(d.counts, vec![0, 1, 1, 0]);
        assert_eq!(d.count, 2);
        assert!((d.sum - 0.55).abs() < 1e-9);
        // A restart: counts went down, so the new value is its own window.
        let fresh = {
            let mut f = HistogramValue::new(&B);
            f.observe(0.5);
            f
        };
        assert!(fresh.delta_since(&b).is_none());
        // Different bounds are a different declaration, never a delta.
        assert!(HistogramValue::new(&[1.0]).delta_since(&a).is_none());
    }

    #[test]
    fn quantile_interpolates_inside_the_bucket_like_histogram_quantile() {
        let h = HistogramValue {
            buckets: vec![1.0, 2.0, 4.0],
            counts: vec![10, 10, 20, 0],
            count: 40,
            sum: 90.0,
        };
        // p25 = rank 10 → the top of the first bucket, [0, 1].
        assert_eq!(h.quantile(0.25), Some(1.0));
        // p50 = rank 20 → the top of (1, 2].
        assert_eq!(h.quantile(0.5), Some(2.0));
        // p75 = rank 30 → halfway through (2, 4].
        assert_eq!(h.quantile(0.75), Some(3.0));
        assert_eq!(h.mean(), Some(2.25));
        // In the overflow bucket, the last bound is a floor.
        let over = HistogramValue {
            buckets: vec![1.0],
            counts: vec![0, 5],
            count: 5,
            sum: 50.0,
        };
        assert_eq!(over.quantile(0.5), Some(1.0));
        assert_eq!(HistogramValue::new(&B).quantile(0.5), None);
        assert_eq!(h.quantile(1.5), None);
    }

    #[test]
    fn the_summary_marks_its_estimates() {
        let mut h = HistogramValue::new(&[0.01, 0.1, 1.0]);
        for v in [0.02, 0.03, 0.04, 0.5] {
            h.observe(v);
        }
        let s = h.summary(Some("s"));
        assert!(s.starts_with("n=4 · mean 0.1475 s · p50 ≈"), "{s}");
        assert!(s.contains("p95 ≈"), "{s}");
        assert_eq!(HistogramValue::new(&B).summary(None), "no observations");
    }

    /// The wire shape: tagged `histogram`, `buckets` spelled as zenkey's
    /// `kind-mismatch` judge reads them, and a CBOR round trip bit for bit.
    #[test]
    fn it_rides_telemetry_value_with_the_tag_the_registry_names() {
        let mut h = HistogramValue::new(&B);
        h.observe(0.05);
        let v = crate::TelemetryValue::Histogram(h.clone());
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["type"], "histogram");
        assert_eq!(
            json["value"]["buckets"],
            serde_json::json!([0.01, 0.1, 1.0])
        );
        assert_eq!(json["value"]["counts"], serde_json::json!([0, 1, 0, 0]));
        let bytes = crate::serialization::encode(&v, crate::serialization::Format::Cbor).unwrap();
        let back: crate::TelemetryValue =
            crate::serialization::decode(&bytes, crate::serialization::Format::Cbor).unwrap();
        assert_eq!(back, v);
    }
}
