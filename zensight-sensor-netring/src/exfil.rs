//! Data-exfiltration heuristic (#123).
//!
//! Learns a per-source baseline of outbound flow size and flags a flow whose
//! outbound bytes exceed that baseline by a configurable number of standard
//! deviations. The baseline is an exponentially-weighted moving mean **and**
//! variance per source IP (EWMA / EWMVar), so it adapts to a host's normal
//! behaviour and needs no fixed threshold per environment.
//!
//! Pure + stateful (no capture types, no clock), so the statistics and the
//! firing decision are unit-testable; the monitor feeds it `(src, bytes_out)` in
//! the TCP flow-end handler and ships any finding on the typed alerts channel.

use std::collections::HashMap;
use std::net::IpAddr;

/// Exponentially-weighted mean + variance of a single series (West's incremental
/// EWMA variance). `alpha` is the smoothing factor in `(0, 1]`.
#[derive(Debug, Clone)]
struct EwmaVar {
    alpha: f64,
    mean: f64,
    /// EWMA of squared deviation from the running mean.
    var: f64,
    count: u64,
}

impl EwmaVar {
    fn new(alpha: f64) -> Self {
        Self {
            alpha,
            mean: 0.0,
            var: 0.0,
            count: 0,
        }
    }

    /// Fold in a sample, returning the standard deviation **before** the update
    /// (the baseline this sample is judged against).
    fn update(&mut self, sample: f64) -> f64 {
        let stddev_before = self.var.max(0.0).sqrt();
        if self.count == 0 {
            self.mean = sample;
            self.var = 0.0;
        } else {
            // West (1979) incremental EWMA mean/variance.
            let diff = sample - self.mean;
            let incr = self.alpha * diff;
            self.mean += incr;
            self.var = (1.0 - self.alpha) * (self.var + diff * incr);
        }
        self.count += 1;
        stddev_before
    }
}

/// A flagged exfiltration candidate: the offending source, its observed outbound
/// volume, and how many sigma above baseline it landed.
#[derive(Debug, Clone, PartialEq)]
pub struct ExfilFinding {
    pub src: IpAddr,
    pub bytes_out: u64,
    pub zscore: f64,
}

/// Per-source outbound-volume baseline + exceedance detector (#123). `sigma` and
/// `min_bytes` are passed to [`observe`](Self::observe) per call (read from the
/// live [`AnomalyConfig`]) so runtime tuning takes effect without a rebuild — the
/// detector itself only holds the learned per-source statistics.
#[derive(Debug)]
pub struct ExfilDetector {
    alpha: f64,
    /// Samples a source must have before its baseline is trusted (warm-up).
    warmup: u64,
    /// Cap on tracked sources (bounds memory; evict-none, ignore-new past cap).
    max_sources: usize,
    sources: HashMap<IpAddr, EwmaVar>,
}

impl ExfilDetector {
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha: alpha.clamp(f64::MIN_POSITIVE, 1.0),
            warmup: 8,
            max_sources: 65_536,
            sources: HashMap::new(),
        }
    }

    /// Observe one flow's outbound bytes from `src`. Updates the baseline and
    /// returns a finding when the flow is anomalously large for that source
    /// (past warm-up, above the `min_bytes` floor, and beyond `sigma` stddevs).
    pub fn observe(
        &mut self,
        src: IpAddr,
        bytes_out: u64,
        sigma: f64,
        min_bytes: u64,
    ) -> Option<ExfilFinding> {
        // Ignore brand-new sources once the table is full (bounded memory).
        if !self.sources.contains_key(&src) && self.sources.len() >= self.max_sources {
            return None;
        }
        let alpha = self.alpha;
        let entry = self
            .sources
            .entry(src)
            .or_insert_with(|| EwmaVar::new(alpha));
        let trusted = entry.count >= self.warmup;
        let mean = entry.mean;
        let stddev = entry.update(bytes_out as f64);

        if !trusted || bytes_out < min_bytes {
            return None;
        }
        // A flat baseline (stddev≈0) still flags a clear order-of-magnitude jump.
        let threshold = mean + sigma * stddev.max(1.0);
        if (bytes_out as f64) <= threshold {
            return None;
        }
        let zscore = (bytes_out as f64 - mean) / stddev.max(1.0);
        Some(ExfilFinding {
            src,
            bytes_out,
            zscore,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    const SIGMA: f64 = 3.0;

    #[test]
    fn warmup_suppresses_early_flags() {
        let mut d = ExfilDetector::new(0.3);
        // First few large flows during warm-up never fire.
        for _ in 0..8 {
            assert_eq!(d.observe(ip(1), 10_000, SIGMA, 1_000), None);
        }
    }

    #[test]
    fn steady_source_then_spike_fires() {
        let mut d = ExfilDetector::new(0.3);
        // Establish a ~10KB baseline with realistic jitter (8-12KB) past warm-up.
        let jitter = [9_000u64, 11_000, 8_500, 12_000, 10_500, 9_500];
        for i in 0..30 {
            let _ = d.observe(ip(1), jitter[i % jitter.len()], SIGMA, 1_000);
        }
        // A flow within the normal band does not fire.
        assert!(d.observe(ip(1), 11_500, SIGMA, 1_000).is_none());
        // A 50x burst fires with a large z-score.
        let f = d
            .observe(ip(1), 500_000, SIGMA, 1_000)
            .expect("exfil flagged");
        assert_eq!(f.src, ip(1));
        assert_eq!(f.bytes_out, 500_000);
        assert!(f.zscore > 3.0);
    }

    #[test]
    fn below_floor_never_fires() {
        let mut d = ExfilDetector::new(0.3);
        for _ in 0..20 {
            let _ = d.observe(ip(1), 100, SIGMA, 1_000_000);
        }
        // 10x the baseline but under the absolute byte floor → no fire.
        assert!(d.observe(ip(1), 1_000, SIGMA, 1_000_000).is_none());
    }

    #[test]
    fn sources_are_independent() {
        let mut d = ExfilDetector::new(0.3);
        for _ in 0..20 {
            let _ = d.observe(ip(1), 10_000, SIGMA, 1_000);
            let _ = d.observe(ip(2), 2_000_000, SIGMA, 1_000);
        }
        // A 1MB flow is a spike for the quiet host but normal for the busy one.
        assert!(d.observe(ip(1), 1_000_000, SIGMA, 1_000).is_some());
        assert!(d.observe(ip(2), 1_000_000, SIGMA, 1_000).is_none());
    }

    #[test]
    fn source_table_is_bounded() {
        let mut d = ExfilDetector::new(0.3);
        d.max_sources = 2;
        let _ = d.observe(ip(1), 100, SIGMA, 1_000);
        let _ = d.observe(ip(2), 100, SIGMA, 1_000);
        // Third distinct source past the cap is ignored (not tracked).
        assert!(d.observe(ip(3), 10_000_000, SIGMA, 1_000).is_none());
        assert_eq!(d.sources.len(), 2);
        // Existing sources still update.
        let _ = d.observe(ip(1), 200, SIGMA, 1_000);
        assert_eq!(d.sources.len(), 2);
    }
}
