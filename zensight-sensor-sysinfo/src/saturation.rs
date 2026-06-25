//! Derived host **saturation score** + coarse **health state** (report proposal P6).
//!
//! The sensor already collects the full USE (Utilization / Saturation / Errors)
//! matrix — PSI, run-queue depth, swap traffic, disk %util, FD occupancy — but
//! emits no *single* at-a-glance signal, so the dashboard / topology tint /
//! alerting each have to re-derive "how loaded is this host?" from raw metrics.
//!
//! This module distills those saturation signals into one number:
//! - [`saturation_score`] — a documented **weighted blend** of the USE saturation
//!   inputs, each normalized to `0..1`, weighted, summed and scaled to `0..100`.
//! - [`health_state`] — a coarse `ok` / `warn` / `crit` band over that score, with
//!   configurable thresholds.
//!
//! Both are **pure** functions of a typed [`SaturationInputs`] + [`SaturationConfig`]
//! (no I/O, no session), so they are fully unit-testable. The collector gathers the
//! inputs from the same `/proc`/`/sys` + `sysinfo` sources the telemetry collectors
//! already read, then emits `system/saturation_score` (Gauge) and
//! `system/health_state` (Text) each poll tick.
//!
//! ## Scoring model
//!
//! Each input is normalized to a saturation fraction in `0..1` (1.0 == fully
//! saturated):
//!
//! | Input                       | Normalization                                  |
//! |-----------------------------|------------------------------------------------|
//! | PSI cpu `some/avg10`        | `pct / 100`                                    |
//! | PSI memory `some/avg10`     | `pct / 100`                                    |
//! | PSI io `some/avg10`         | `pct / 100`                                    |
//! | run-queue (`procs_running`) | `procs_running / nCPU` (1.0 == one runnable    |
//! |                             | task per CPU = full saturation)                |
//! | swap-in rate                | `pages_per_sec / swap_in_ref_pages_per_sec`    |
//! | disk %util (busiest device) | `pct / 100`                                    |
//! | FD table occupancy          | `pct / 100`                                    |
//!
//! The score is `100 * Σ(weight_i · norm_i) / Σ(weight_i)`. Dividing by the **total**
//! weight (not just the present-input weight) means a **missing input is treated as
//! `0` (not saturated)**: a host with reduced collection reports a conservatively
//! *lower* score rather than a misleadingly inflated one. This is intentional — see
//! [`SaturationInputs`].
//!
//! Default weights (sum to 1.0) emphasize the memory/CPU/IO pressure trio, which
//! best predicts user-visible stalls:
//!
//! | Input        | Default weight |
//! |--------------|----------------|
//! | PSI memory   | 0.24           |
//! | PSI cpu      | 0.19           |
//! | PSI io       | 0.16           |
//! | disk %util   | 0.16           |
//! | swap-in      | 0.12           |
//! | run-queue    | 0.08           |
//! | FD occupancy | 0.05           |
//!
//! With these weights the canonical "host in trouble" combination (high PSI on all
//! three resources + a full disk + active swap-in) clears `~84` → `crit` even at a
//! realistic ~95% PSI, while an idle host scores `~0` → `ok`.

use serde::{Deserialize, Serialize};

// ===========================================================================
// Configuration
// ===========================================================================

/// Saturation-score configuration (JSON5 `sysinfo.saturation`). The score itself
/// is gated by `collect.saturation_score` (default on); this block tunes the blend
/// and the health-state bands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaturationConfig {
    /// Per-input blend weights (need not sum to 1.0 — the score renormalizes by
    /// their total).
    #[serde(default)]
    pub weights: SaturationWeights,

    /// Swap-in rate (pages/s) that normalizes to a saturation fraction of `1.0`.
    /// A page is typically 4 KiB, so the 1000 pages/s default is ~4 MiB/s of
    /// sustained swap-in.
    #[serde(default = "default_swap_in_ref")]
    pub swap_in_ref_pages_per_sec: f64,

    /// Score at/above which `health_state` is `warn` (default 50).
    #[serde(default = "default_warn")]
    pub warn: f64,

    /// Score at/above which `health_state` is `crit` (default 80).
    #[serde(default = "default_crit")]
    pub crit: f64,
}

fn default_swap_in_ref() -> f64 {
    1000.0
}
fn default_warn() -> f64 {
    50.0
}
fn default_crit() -> f64 {
    80.0
}

impl Default for SaturationConfig {
    fn default() -> Self {
        Self {
            weights: SaturationWeights::default(),
            swap_in_ref_pages_per_sec: default_swap_in_ref(),
            warn: default_warn(),
            crit: default_crit(),
        }
    }
}

/// Per-input blend weights. Defaults sum to 1.0 (see module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaturationWeights {
    #[serde(default = "w_psi_cpu")]
    pub psi_cpu: f64,
    #[serde(default = "w_psi_memory")]
    pub psi_memory: f64,
    #[serde(default = "w_psi_io")]
    pub psi_io: f64,
    #[serde(default = "w_run_queue")]
    pub run_queue: f64,
    #[serde(default = "w_swap_in")]
    pub swap_in: f64,
    #[serde(default = "w_disk_util")]
    pub disk_util: f64,
    #[serde(default = "w_fd")]
    pub fd: f64,
}

fn w_psi_cpu() -> f64 {
    0.19
}
fn w_psi_memory() -> f64 {
    0.24
}
fn w_psi_io() -> f64 {
    0.16
}
fn w_run_queue() -> f64 {
    0.08
}
fn w_swap_in() -> f64 {
    0.12
}
fn w_disk_util() -> f64 {
    0.16
}
fn w_fd() -> f64 {
    0.05
}

impl Default for SaturationWeights {
    fn default() -> Self {
        Self {
            psi_cpu: w_psi_cpu(),
            psi_memory: w_psi_memory(),
            psi_io: w_psi_io(),
            run_queue: w_run_queue(),
            swap_in: w_swap_in(),
            disk_util: w_disk_util(),
            fd: w_fd(),
        }
    }
}

// ===========================================================================
// Inputs
// ===========================================================================

/// The saturation signals for one poll tick, as gathered by the collector. Every
/// field is `Option` because each collector is independently config-gated and may
/// be absent (non-Linux, missing `/proc` file, no previous sample for a rate). A
/// `None` field is **treated as `0` (not saturated)** by [`saturation_score`], so
/// the score degrades gracefully toward a lower value as inputs drop out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SaturationInputs {
    /// PSI cpu `some/avg10`, percent `0..100`.
    pub psi_cpu_avg10: Option<f64>,
    /// PSI memory `some/avg10`, percent `0..100`.
    pub psi_memory_avg10: Option<f64>,
    /// PSI io `some/avg10`, percent `0..100`.
    pub psi_io_avg10: Option<f64>,
    /// Run-queue depth relative to CPU count (`procs_running / nCPU`). `1.0` means
    /// one runnable task per CPU (full saturation); higher means over-subscribed.
    pub run_queue_ratio: Option<f64>,
    /// Combined swap-in rate in pages/s (`pswpin` delta over the interval).
    pub swap_in_pages_per_sec: Option<f64>,
    /// Busiest block device's `%util` (`0..100`).
    pub disk_util_percent: Option<f64>,
    /// File-descriptor table occupancy (`0..100`).
    pub fd_used_percent: Option<f64>,
}

// ===========================================================================
// Pure scoring
// ===========================================================================

/// Clamp to the closed unit interval `[0, 1]`.
fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// Normalize a `0..100` percent (or `None`) to a `0..1` saturation fraction.
/// Missing → `0.0`.
fn norm_pct(pct: Option<f64>) -> f64 {
    pct.map(|v| clamp01(v / 100.0)).unwrap_or(0.0)
}

/// Compute the `0..100` host saturation score from the typed inputs + config.
///
/// Pure and total: never panics, always returns a finite value in `0..=100`.
/// Missing inputs contribute `0`; the result is monotonic non-decreasing in every
/// input (raising any single signal can only raise the score).
pub fn saturation_score(inputs: &SaturationInputs, cfg: &SaturationConfig) -> f64 {
    let w = &cfg.weights;
    let swap_ref = if cfg.swap_in_ref_pages_per_sec > 0.0 {
        cfg.swap_in_ref_pages_per_sec
    } else {
        default_swap_in_ref()
    };

    // (weight, normalized 0..1 contribution). A missing/`None` input normalizes to
    // 0 but its weight still counts toward the denominator (= "treated as 0").
    let terms = [
        (w.psi_cpu, norm_pct(inputs.psi_cpu_avg10)),
        (w.psi_memory, norm_pct(inputs.psi_memory_avg10)),
        (w.psi_io, norm_pct(inputs.psi_io_avg10)),
        (
            w.run_queue,
            inputs.run_queue_ratio.map(clamp01).unwrap_or(0.0),
        ),
        (
            w.swap_in,
            inputs
                .swap_in_pages_per_sec
                .map(|r| clamp01(r / swap_ref))
                .unwrap_or(0.0),
        ),
        (w.disk_util, norm_pct(inputs.disk_util_percent)),
        (w.fd, norm_pct(inputs.fd_used_percent)),
    ];

    // Ignore non-positive weights (a user can zero out a signal to drop it).
    let weight_sum: f64 = terms.iter().map(|(wt, _)| wt.max(0.0)).sum();
    if weight_sum <= 0.0 {
        return 0.0;
    }
    let acc: f64 = terms.iter().map(|(wt, n)| wt.max(0.0) * n).sum();
    (100.0 * acc / weight_sum).clamp(0.0, 100.0)
}

/// Map a `0..100` score to a coarse health band using the configured thresholds.
/// `crit` takes priority over `warn`.
pub fn health_state(score: f64, cfg: &SaturationConfig) -> &'static str {
    if score >= cfg.crit {
        "crit"
    } else if score >= cfg.warn {
        "warn"
    } else {
        "ok"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected ~{b}, got {a}");
    }

    #[test]
    fn idle_host_scores_zero_and_ok() {
        let cfg = SaturationConfig::default();
        let idle = SaturationInputs {
            psi_cpu_avg10: Some(0.0),
            psi_memory_avg10: Some(0.0),
            psi_io_avg10: Some(0.0),
            run_queue_ratio: Some(0.0),
            swap_in_pages_per_sec: Some(0.0),
            disk_util_percent: Some(0.0),
            fd_used_percent: Some(0.0),
        };
        let s = saturation_score(&idle, &cfg);
        approx(s, 0.0);
        assert_eq!(health_state(s, &cfg), "ok");
    }

    #[test]
    fn fully_saturated_host_scores_100_and_crit() {
        let cfg = SaturationConfig::default();
        let pegged = SaturationInputs {
            psi_cpu_avg10: Some(100.0),
            psi_memory_avg10: Some(100.0),
            psi_io_avg10: Some(100.0),
            run_queue_ratio: Some(4.0),            // > 1, clamps to 1
            swap_in_pages_per_sec: Some(10_000.0), // >> ref, clamps to 1
            disk_util_percent: Some(100.0),
            fd_used_percent: Some(100.0),
        };
        let s = saturation_score(&pegged, &cfg);
        approx(s, 100.0);
        assert_eq!(health_state(s, &cfg), "crit");
    }

    #[test]
    fn acceptance_high_psi_full_disk_swap_in_is_crit() {
        // The issue's canonical "bad host": high PSI (all three) + full disk +
        // active swap-in. With default weights this clears ~84 -> crit even at a
        // realistic ~95% PSI (run-queue + FD absent, treated as 0).
        let cfg = SaturationConfig::default();
        let bad = SaturationInputs {
            psi_cpu_avg10: Some(95.0),
            psi_memory_avg10: Some(95.0),
            psi_io_avg10: Some(95.0),
            swap_in_pages_per_sec: Some(2_000.0), // >= ref
            disk_util_percent: Some(98.0),
            ..Default::default() // run_queue + fd missing (treated as 0)
        };
        let s = saturation_score(&bad, &cfg);
        assert!(s >= cfg.crit, "expected crit-level score, got {s}");
        assert_eq!(health_state(s, &cfg), "crit");
    }

    #[test]
    fn each_input_contributes_monotonically() {
        let cfg = SaturationConfig::default();
        let base = SaturationInputs::default();
        let base_score = saturation_score(&base, &cfg);

        // Raising any single input must not lower the score.
        let mutators: [fn(&mut SaturationInputs); 7] = [
            |i| i.psi_cpu_avg10 = Some(60.0),
            |i| i.psi_memory_avg10 = Some(60.0),
            |i| i.psi_io_avg10 = Some(60.0),
            |i| i.run_queue_ratio = Some(0.8),
            |i| i.swap_in_pages_per_sec = Some(800.0),
            |i| i.disk_util_percent = Some(60.0),
            |i| i.fd_used_percent = Some(60.0),
        ];
        for m in mutators {
            let mut i = base.clone();
            m(&mut i);
            let s = saturation_score(&i, &cfg);
            assert!(
                s > base_score,
                "raising one input should raise the score (got {s} vs {base_score})"
            );
        }
    }

    #[test]
    fn score_increases_with_input_magnitude() {
        let cfg = SaturationConfig::default();
        let lo = SaturationInputs {
            psi_cpu_avg10: Some(20.0),
            ..Default::default()
        };
        let hi = SaturationInputs {
            psi_cpu_avg10: Some(80.0),
            ..Default::default()
        };
        assert!(saturation_score(&hi, &cfg) > saturation_score(&lo, &cfg));
    }

    #[test]
    fn missing_inputs_treated_as_zero_and_lower_the_score() {
        let cfg = SaturationConfig::default();
        // Only PSI present and maxed; everything else missing (== 0). The score
        // therefore stays well below 100 — a conservatively low reading.
        let partial = SaturationInputs {
            psi_cpu_avg10: Some(100.0),
            psi_memory_avg10: Some(100.0),
            psi_io_avg10: Some(100.0),
            ..Default::default()
        };
        let s = saturation_score(&partial, &cfg);
        // Contribution = (0.19 + 0.24 + 0.16) / 1.0 = 0.59 -> 59.
        approx(s, 59.0);
        assert!(s < 100.0);
        // Adding the remaining signals only raises it.
        let mut full = partial.clone();
        full.disk_util_percent = Some(100.0);
        full.fd_used_percent = Some(100.0);
        full.run_queue_ratio = Some(1.0);
        full.swap_in_pages_per_sec = Some(2_000.0);
        assert!(saturation_score(&full, &cfg) > s);
    }

    #[test]
    fn empty_inputs_score_zero() {
        let cfg = SaturationConfig::default();
        approx(saturation_score(&SaturationInputs::default(), &cfg), 0.0);
    }

    #[test]
    fn health_state_bands() {
        let cfg = SaturationConfig::default(); // warn 50, crit 80
        assert_eq!(health_state(0.0, &cfg), "ok");
        assert_eq!(health_state(49.9, &cfg), "ok");
        assert_eq!(health_state(50.0, &cfg), "warn");
        assert_eq!(health_state(79.9, &cfg), "warn");
        assert_eq!(health_state(80.0, &cfg), "crit");
        assert_eq!(health_state(100.0, &cfg), "crit");
    }

    #[test]
    fn custom_thresholds_are_respected() {
        let cfg = SaturationConfig {
            warn: 30.0,
            crit: 60.0,
            ..Default::default()
        };
        assert_eq!(health_state(25.0, &cfg), "ok");
        assert_eq!(health_state(40.0, &cfg), "warn");
        assert_eq!(health_state(70.0, &cfg), "crit");
    }

    #[test]
    fn zero_weights_score_zero_not_nan() {
        let cfg = SaturationConfig {
            weights: SaturationWeights {
                psi_cpu: 0.0,
                psi_memory: 0.0,
                psi_io: 0.0,
                run_queue: 0.0,
                swap_in: 0.0,
                disk_util: 0.0,
                fd: 0.0,
            },
            ..Default::default()
        };
        let pegged = SaturationInputs {
            psi_cpu_avg10: Some(100.0),
            ..Default::default()
        };
        let s = saturation_score(&pegged, &cfg);
        assert!(s.is_finite());
        approx(s, 0.0);
    }

    #[test]
    fn run_queue_ratio_normalizes_against_cpu_count() {
        let cfg = SaturationConfig::default();
        // Only the run-queue signal present. Half-saturated (ratio 0.5) should be
        // half the contribution of fully-saturated (ratio >= 1).
        let half = SaturationInputs {
            run_queue_ratio: Some(0.5),
            ..Default::default()
        };
        let full = SaturationInputs {
            run_queue_ratio: Some(1.0),
            ..Default::default()
        };
        approx(
            saturation_score(&full, &cfg),
            2.0 * saturation_score(&half, &cfg),
        );
    }

    #[test]
    fn swap_in_normalizes_against_configurable_ref() {
        let cfg = SaturationConfig {
            swap_in_ref_pages_per_sec: 500.0,
            ..Default::default()
        };
        // At the reference rate the swap-in term is fully saturated; doubling it
        // can't push the score past that (clamped at 1.0).
        let at_ref = SaturationInputs {
            swap_in_pages_per_sec: Some(500.0),
            ..Default::default()
        };
        let over_ref = SaturationInputs {
            swap_in_pages_per_sec: Some(5_000.0),
            ..Default::default()
        };
        approx(
            saturation_score(&at_ref, &cfg),
            saturation_score(&over_ref, &cfg),
        );
    }
}
