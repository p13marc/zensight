//! Novelty / "what's new" detection → alerts (#103, report proposal C3).
//!
//! The only log-driven alerts before this were the four hardcoded systemd
//! `MESSAGE_ID`s in [`crate::events`] — they catch *known* failures. This module
//! catches the unknown-unknowns by sitting on top of the #102 template miner
//! ([`crate::template`]): every line is already reduced to a stable
//! `template_id`, so we maintain a bounded **seen-templates set** and raise an
//! [`AlertKind::Anomaly`] when either
//!
//! - a template appears for the **first time** after a startup *warm-up* window
//!   (rule [`NOVELTY_RULE`]) — a never-before-seen log-line *shape*; or
//! - a *known* template's recent rate **spikes** N× over its EWMA baseline
//!   (rule [`SPIKE_RULE`]) — a familiar line that suddenly floods.
//!
//! The decision core is **pure**: [`is_novel`] and [`is_spike`] are total
//! functions over their inputs, and [`NoveltyTracker`] threads the clock in
//! explicitly (`now: Instant`) so the whole thing is unit-testable without a
//! Zenoh session. Alert *wiring* (the shared [`AlertReporter`], reconcile loops)
//! lives in `main.rs`, mirroring the per-unit error-budget path in
//! [`crate::derived`]: novelty fires immediately per-line (like
//! [`crate::events`]) and auto-resolves after a dedup window; rate-spikes fire on
//! the derived tick and resolve when the rate falls back to baseline.
//!
//! Everything is **bounded** (the seen-set is capped, beyond which new shapes are
//! conservatively ignored) and **conservative** (warm-up so a cold start isn't
//! all "novel"; dedup by `template_id` so one novel template is exactly one
//! alert).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::telemetry::Protocol;

/// Rule slug for first-seen-template alerts (point events, auto-resolving).
pub const NOVELTY_RULE: &str = "log-novelty";
/// Rule slug for rate-spike alerts (ongoing condition, resolves on recovery).
pub const SPIKE_RULE: &str = "log-rate-spike";

/// Max length of the masked template placed in an alert summary.
const SUMMARY_MAX: usize = 160;

/// Resolved, validated novelty/rate-spike tuning (#103).
///
/// There is no `enabled` field: the tracker is only constructed when novelty is
/// enabled (and templating is on), so by the time these params exist the feature
/// is live — mirroring how `main.rs` gates the other alert paths.
#[derive(Debug, Clone, Copy)]
pub struct NoveltyParams {
    /// Startup warm-up: templates first seen before this has elapsed are folded
    /// into the baseline (recorded, never flagged), so a cold start isn't all
    /// "novel". Rate-spikes are likewise suppressed until warm-up passes.
    pub warm_up: Duration,
    /// How long a fired novelty point-event stays "firing" before it
    /// auto-resolves (it never re-fires — dedup is by `template_id`).
    pub dedup: Duration,
    /// Rate-spike multiplier: a known template fires when its window rate exceeds
    /// `multiplier`× its EWMA baseline. `<= 1.0` disables spike detection.
    pub rate_spike_multiplier: f64,
    /// Absolute floor on a window's count before a spike can fire (so a jump from
    /// 1→5 lines doesn't alert).
    pub min_spike_count: f64,
    /// EWMA smoothing factor in `0.0..=1.0` for the per-template baseline rate.
    pub ewma_alpha: f64,
    /// Hard cap on the seen-set size (bounds memory). Beyond the cap new shapes
    /// are conservatively treated as known (no alert, not tracked).
    pub max_templates: usize,
}

impl Default for NoveltyParams {
    fn default() -> Self {
        Self {
            warm_up: Duration::from_secs(300),
            dedup: Duration::from_secs(300),
            rate_spike_multiplier: 5.0,
            min_spike_count: 10.0,
            ewma_alpha: 0.3,
            max_templates: 2000,
        }
    }
}

/// Pure novelty decision: a template is novel iff it has **not** been seen before
/// *and* the warm-up window has elapsed. Total function — directly unit-tested.
fn is_novel(seen_before: bool, elapsed_since_start: Duration, warm_up: Duration) -> bool {
    !seen_before && elapsed_since_start >= warm_up
}

/// Pure rate-spike decision: a *known* template spikes when its current window
/// count jumps `multiplier`× over its EWMA baseline, with an absolute volume
/// floor (`min_count`) and an established (non-zero) baseline so a cold or
/// low-traffic template can't trip it. Total function — directly unit-tested.
fn is_spike(current: f64, ewma: f64, multiplier: f64, min_count: f64) -> bool {
    multiplier > 1.0 && ewma > 0.0 && current >= min_count && current > ewma * multiplier
}

/// Per-template rolling state for rate-spike detection.
#[derive(Debug)]
struct Stat {
    /// Masked template string (kept for the alert summary).
    template: String,
    /// Lines mined for this template since the last tick (the current window).
    window_count: u64,
    /// EWMA of the per-window count; `None` until the first tick seeds it.
    ewma: Option<f64>,
}

#[derive(Debug, Default)]
struct Inner {
    /// `template_id` → rolling rate state. Membership *is* the seen-set, so the
    /// first insert of an id is its "first seen".
    seen: HashMap<String, Stat>,
    /// `template_id` → (novelty `alert_key`, expiry). A fired novelty point-event
    /// lives here until `now >= expiry`, at which point the tick drops it so the
    /// reconcile sweep resolves it.
    active_novelty: HashMap<String, (String, Instant)>,
}

/// Alert decisions produced by one [`NoveltyTracker::tick`] call.
pub struct NoveltyTick {
    /// Rate-spike alerts to (re-)`observe` on the shared reporter.
    pub firing: Vec<Alert>,
    /// `alert_key`s of all novelty point-events still within their dedup window —
    /// reconcile [`NOVELTY_RULE`] against this so expired ones auto-resolve.
    pub novelty_keys: Vec<String>,
    /// `alert_key`s of all currently-spiking templates — reconcile [`SPIKE_RULE`]
    /// against this so templates back at baseline auto-resolve.
    pub spike_keys: Vec<String>,
}

/// Bounded, pure-core novelty + rate-spike detector. Shared (`Arc`) between the
/// publish loop (per-line [`observe`](Self::observe)) and the derived tick
/// ([`tick`](Self::tick)).
pub struct NoveltyTracker {
    params: NoveltyParams,
    /// Sensor-wide source label (matches the templating/derived rollup source) so
    /// novelty/spike alerts don't scatter per network-syslog hostname.
    source: String,
    started_at: Instant,
    inner: Mutex<Inner>,
}

impl NoveltyTracker {
    /// Build a tracker. `started_at` anchors the warm-up window (pass
    /// `Instant::now()` in production; a fixed base in tests).
    pub fn new(params: NoveltyParams, source: impl Into<String>, started_at: Instant) -> Self {
        Self {
            params: NoveltyParams {
                ewma_alpha: params.ewma_alpha.clamp(0.0, 1.0),
                max_templates: params.max_templates.max(1),
                ..params
            },
            source: source.into(),
            started_at,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Fold one mined line into the seen-set. Returns a firing novelty [`Alert`]
    /// the first time a template is seen *after* warm-up (exactly once per
    /// `template_id`); `None` otherwise. Cheap: a lock + a couple of map ops.
    pub fn observe(&self, id: &str, template: &str, now: Instant) -> Option<Alert> {
        let elapsed = now.saturating_duration_since(self.started_at);
        let mut inner = self.inner.lock().ok()?;

        // Known template: just advance its window counter for spike tracking.
        if let Some(stat) = inner.seen.get_mut(id) {
            stat.window_count = stat.window_count.saturating_add(1);
            return None;
        }

        // Unknown template, but the seen-set is full → stay bounded and
        // conservative: don't track it and don't alert.
        if inner.seen.len() >= self.params.max_templates {
            return None;
        }

        let novel = is_novel(false, elapsed, self.params.warm_up);
        inner.seen.insert(
            id.to_string(),
            Stat {
                template: template.to_string(),
                window_count: 1,
                ewma: None,
            },
        );

        if !novel {
            // First-seen during warm-up: baseline, not an alert.
            return None;
        }

        let alert = novelty_alert(&self.source, id, template);
        inner
            .active_novelty
            .insert(id.to_string(), (alert.alert_key(), now + self.params.dedup));
        Some(alert)
    }

    /// Advance the rate window for every tracked template and return the alert
    /// decisions. Call once per derived tick. Updates each template's EWMA
    /// baseline, fires/clears rate-spikes, and ages out novelty point-events.
    pub fn tick(&self, now: Instant) -> NoveltyTick {
        let elapsed = now.saturating_duration_since(self.started_at);
        let warm = elapsed >= self.params.warm_up;
        let p = self.params;
        let mut out = NoveltyTick {
            firing: Vec::new(),
            novelty_keys: Vec::new(),
            spike_keys: Vec::new(),
        };

        let Ok(mut inner) = self.inner.lock() else {
            return out;
        };

        // Novelty point-events: keep the ones still inside their dedup window,
        // drop the expired (the reconcile sweep then resolves them).
        inner.active_novelty.retain(|_, (_, expiry)| *expiry > now);
        out.novelty_keys = inner
            .active_novelty
            .values()
            .map(|(key, _)| key.clone())
            .collect();

        // Rate-spikes: per template, compare this window's count to the EWMA
        // baseline, then roll the EWMA forward and reset the window.
        for (id, stat) in inner.seen.iter_mut() {
            let current = stat.window_count as f64;
            stat.window_count = 0;

            let spike = match stat.ewma {
                // Baseline established and warm: a sufficiently large jump fires.
                Some(ewma) if warm => {
                    is_spike(current, ewma, p.rate_spike_multiplier, p.min_spike_count)
                }
                _ => false,
            };

            // Roll the EWMA forward (seed it on the first tick).
            stat.ewma = Some(match stat.ewma {
                Some(ewma) => p.ewma_alpha * current + (1.0 - p.ewma_alpha) * ewma,
                None => current,
            });

            if spike {
                let baseline = stat.ewma.unwrap_or(current);
                let alert = spike_alert(&self.source, id, &stat.template, current, baseline);
                out.spike_keys.push(alert.alert_key());
                out.firing.push(alert);
            }
        }

        out
    }
}

/// Truncate `s` to `max` chars (with an ellipsis) for an alert summary.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Build the firing alert for a never-before-seen template (pure / testable).
fn novelty_alert(source: &str, id: &str, template: &str) -> Alert {
    Alert::new(
        source.to_string(),
        Protocol::Syslog,
        AlertKind::Anomaly,
        NOVELTY_RULE,
        AlertSeverity::Info,
        format!("new log pattern: {}", truncate(template, SUMMARY_MAX)),
    )
    .with_label("template_id", id.to_string())
    .with_label("template", truncate(template, SUMMARY_MAX))
}

/// Build the firing alert for a known template whose rate has spiked.
fn spike_alert(source: &str, id: &str, template: &str, current: f64, baseline: f64) -> Alert {
    Alert::new(
        source.to_string(),
        Protocol::Syslog,
        AlertKind::Anomaly,
        SPIKE_RULE,
        AlertSeverity::Warning,
        format!(
            "log rate spike: {} — {:.0} this window vs ~{:.1} baseline",
            truncate(template, SUMMARY_MAX),
            current,
            baseline
        ),
    )
    .with_label("template_id", id.to_string())
    .with_label("template", truncate(template, SUMMARY_MAX))
    .with_label("window_count", format!("{current:.0}"))
    .with_label("baseline", format!("{baseline:.2}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WARM: Duration = Duration::from_secs(60);

    fn params() -> NoveltyParams {
        NoveltyParams {
            warm_up: WARM,
            dedup: Duration::from_secs(120),
            rate_spike_multiplier: 5.0,
            min_spike_count: 10.0,
            ewma_alpha: 0.3,
            max_templates: 2000,
        }
    }

    // ---- pure decision functions ----------------------------------------

    #[test]
    fn is_novel_truth_table() {
        // Never seen + past warm-up → novel.
        assert!(is_novel(false, Duration::from_secs(61), WARM));
        // Never seen but still warming up → not novel (baseline).
        assert!(!is_novel(false, Duration::from_secs(10), WARM));
        // Boundary: exactly at warm-up counts as past it.
        assert!(is_novel(false, WARM, WARM));
        // Already seen → never novel, even long after warm-up.
        assert!(!is_novel(true, Duration::from_secs(10_000), WARM));
    }

    #[test]
    fn is_spike_truth_table() {
        // 100 vs baseline 10, 5× → 100 > 50 → spike.
        assert!(is_spike(100.0, 10.0, 5.0, 10.0));
        // Just over baseline (15 vs 10×5=50) → no spike.
        assert!(!is_spike(15.0, 10.0, 5.0, 10.0));
        // Big ratio but below the absolute floor → no spike.
        assert!(!is_spike(8.0, 1.0, 5.0, 10.0));
        // No established baseline → no spike.
        assert!(!is_spike(100.0, 0.0, 5.0, 10.0));
        // Multiplier <= 1 disables spike detection entirely.
        assert!(!is_spike(100.0, 1.0, 1.0, 10.0));
    }

    // ---- tracker: novelty ------------------------------------------------

    fn tracker() -> (NoveltyTracker, Instant) {
        let t0 = Instant::now();
        (NoveltyTracker::new(params(), "host01", t0), t0)
    }

    #[test]
    fn warm_up_suppresses_early_novelty() {
        let (tr, t0) = tracker();
        // Seen during warm-up → recorded as baseline, no alert.
        assert!(tr.observe("aaaa", "user <*> logged in", t0).is_none());
        // The same template later (after warm-up) is already known → still none.
        assert!(
            tr.observe("aaaa", "user <*> logged in", t0 + Duration::from_secs(61))
                .is_none()
        );
    }

    #[test]
    fn genuinely_new_template_after_warmup_fires_once() {
        let (tr, t0) = tracker();
        let after = t0 + Duration::from_secs(61);
        // First sighting after warm-up → one novelty alert.
        let a = tr.observe("bbbb", "disk <*> failed", after).unwrap();
        assert_eq!(a.rule, NOVELTY_RULE);
        assert_eq!(a.kind, AlertKind::Anomaly);
        assert_eq!(
            a.labels.get("template_id").map(String::as_str),
            Some("bbbb")
        );
        // Repeat sightings do NOT re-fire (dedup by template_id).
        assert!(
            tr.observe("bbbb", "disk <*> failed", after + Duration::from_secs(1))
                .is_none()
        );
    }

    #[test]
    fn template_seen_in_warmup_is_not_flagged_after_warmup() {
        let (tr, t0) = tracker();
        // Baseline during warm-up.
        assert!(tr.observe("cccc", "ntp sync <*>", t0).is_none());
        // A *different* new template after warm-up fires; the warm-up one does not.
        assert!(
            tr.observe("cccc", "ntp sync <*>", t0 + Duration::from_secs(61))
                .is_none()
        );
        assert!(
            tr.observe("dddd", "kernel panic <*>", t0 + Duration::from_secs(61))
                .is_some()
        );
    }

    #[test]
    fn novelty_alert_appears_in_tick_keys_then_auto_resolves() {
        let (tr, t0) = tracker();
        let fired_at = t0 + Duration::from_secs(61);
        let a = tr.observe("eeee", "new shape <*>", fired_at).unwrap();
        let key = a.alert_key();
        // Within the dedup window the key is still active (reconcile keeps it).
        let t = tr.tick(fired_at + Duration::from_secs(1));
        assert!(t.novelty_keys.contains(&key));
        // After the dedup window it drops out → reconcile resolves it.
        let t = tr.tick(fired_at + Duration::from_secs(121));
        assert!(!t.novelty_keys.contains(&key));
    }

    // ---- tracker: rate spikes -------------------------------------------

    /// Observe `n` lines of template `id` at `now`.
    fn observe_n(tr: &NoveltyTracker, id: &str, template: &str, n: usize, now: Instant) {
        for _ in 0..n {
            let _ = tr.observe(id, template, now);
        }
    }

    #[test]
    fn rate_spike_fires_over_baseline_and_resolves_on_recovery() {
        let (tr, t0) = tracker();
        let base = t0 + Duration::from_secs(61); // past warm-up

        // Window 1: establish a baseline of 10/window. No spike (no prior EWMA).
        observe_n(&tr, "ffff", "ping <*>", 10, base);
        let t1 = tr.tick(base + Duration::from_secs(1));
        assert!(t1.firing.is_empty());
        assert!(t1.spike_keys.is_empty());

        // Window 2: 100 lines (10× baseline, well over 5×) → spike fires.
        observe_n(&tr, "ffff", "ping <*>", 100, base + Duration::from_secs(2));
        let t2 = tr.tick(base + Duration::from_secs(3));
        assert_eq!(t2.firing.len(), 1);
        assert_eq!(t2.firing[0].rule, SPIKE_RULE);
        assert_eq!(t2.spike_keys.len(), 1);
        let spike_key = t2.spike_keys[0].clone();

        // Window 3: back to baseline-ish (a few lines) → no spike; key gone, so
        // the reconcile sweep resolves the prior spike alert.
        observe_n(&tr, "ffff", "ping <*>", 5, base + Duration::from_secs(4));
        let t3 = tr.tick(base + Duration::from_secs(5));
        assert!(t3.firing.is_empty());
        assert!(!t3.spike_keys.contains(&spike_key));
    }

    #[test]
    fn no_spike_during_warmup_even_with_a_jump() {
        // warm_up not yet elapsed: a huge jump must not fire.
        let t0 = Instant::now();
        let tr = NoveltyTracker::new(params(), "h", t0);
        observe_n(&tr, "gggg", "x <*>", 5, t0);
        tr.tick(t0 + Duration::from_secs(1)); // seed EWMA = 5 (still warming)
        observe_n(&tr, "gggg", "x <*>", 500, t0 + Duration::from_secs(2));
        let t = tr.tick(t0 + Duration::from_secs(3)); // still < 60s warm-up
        assert!(t.firing.is_empty());
        assert!(t.spike_keys.is_empty());
    }

    #[test]
    fn seen_set_is_bounded() {
        let mut p = params();
        p.warm_up = Duration::from_secs(0); // every new template would be novel
        p.max_templates = 2;
        let t0 = Instant::now();
        let tr = NoveltyTracker::new(p, "h", t0);
        let now = t0 + Duration::from_secs(1);
        // First two distinct templates fire novelty and are tracked.
        assert!(tr.observe("id1", "a <*>", now).is_some());
        assert!(tr.observe("id2", "b <*>", now).is_some());
        // Third is over the cap → conservatively ignored (no alert, not tracked).
        assert!(tr.observe("id3", "c <*>", now).is_none());
    }
}
