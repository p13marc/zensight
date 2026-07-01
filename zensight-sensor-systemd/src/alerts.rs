//! Built-in threshold alerts (#276).
//!
//! Turns systemd state into actionable alerts via the sensor-core
//! [`AlertReporter`] (firing → resolved → tombstone). Five rules:
//! - `systemd-unit-failed`     — a watched unit is in `ActiveState=failed`
//! - `systemd-system-degraded` — `SystemState=degraded` or `NFailedUnits>0`
//! - `systemd-restart-storm`   — a watched unit's `NRestarts` climbed past a
//!   threshold within a sliding window (restart-loop signal)
//! - `systemd-timer-overdue`   — a watched timer's next elapse is past due
//! - `systemd-unit-mem`        — a watched unit's `MemoryCurrent` over a ceiling
//!
//! The evaluate step is pure (state → alerts) so every rule is unit-testable; the
//! stateful bits (restart windows) live in [`AlertEvaluator`]. Every rule is
//! emitted as a `RuleAlerts` entry even when it has no violations, so the driver
//! can `reconcile` recovered conditions away.
//!
//! **Dedup with the logs sensor:** the logs sensor also raises a `unit-failed`
//! alert, but *event-based* (matched on the journald `MESSAGE_ID` of a unit
//! failure). This rule is *state-based* (polls `ActiveState`), so it also covers
//! units that were already failed before the sensor started and auto-resolves on
//! recovery. Set `alerts.unit_failed = false` here to defer to the logs sensor.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::warn;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

use crate::unit::UnitSample;

pub const UNIT_FAILED_RULE: &str = "systemd-unit-failed";
pub const DEGRADED_RULE: &str = "systemd-system-degraded";
pub const RESTART_STORM_RULE: &str = "systemd-restart-storm";
pub const TIMER_OVERDUE_RULE: &str = "systemd-timer-overdue";
pub const UNIT_MEM_RULE: &str = "systemd-unit-mem";

/// Threshold-alert configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsConfig {
    /// Master switch for all built-in threshold alerts.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Debounce before a firing alert publishes (seconds).
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    /// Emit the state-based `systemd-unit-failed` rule (see dedup note).
    #[serde(default = "default_true")]
    pub unit_failed: bool,
    /// Emit `systemd-system-degraded`.
    #[serde(default = "default_true")]
    pub system_degraded: bool,
    /// `NRestarts` increase within `restart_storm_window_secs` that fires
    /// `systemd-restart-storm` (0 disables).
    #[serde(default = "default_restart_storm_threshold")]
    pub restart_storm_threshold: u32,
    #[serde(default = "default_restart_storm_window_secs")]
    pub restart_storm_window_secs: u64,
    /// `MemoryCurrent` ceiling (bytes) that fires `systemd-unit-mem` (0 disables).
    #[serde(default)]
    pub unit_mem_ceiling_bytes: u64,
    /// Grace period (seconds) past a timer's next elapse before it's `overdue`.
    #[serde(default = "default_timer_overdue_grace_secs")]
    pub timer_overdue_grace_secs: u64,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: default_for_secs(),
            unit_failed: true,
            system_degraded: true,
            restart_storm_threshold: default_restart_storm_threshold(),
            restart_storm_window_secs: default_restart_storm_window_secs(),
            unit_mem_ceiling_bytes: 0,
            timer_overdue_grace_secs: default_timer_overdue_grace_secs(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_for_secs() -> u64 {
    15
}
fn default_restart_storm_threshold() -> u32 {
    3
}
fn default_restart_storm_window_secs() -> u64 {
    300
}
fn default_timer_overdue_grace_secs() -> u64 {
    300
}

/// A watched timer's schedule, for the overdue rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerSample {
    pub name: String,
    /// Wall-clock µs of the next scheduled elapse (0 / `u64::MAX` = none).
    pub next_elapse_usec_realtime: u64,
}

/// Fully-derived inputs for one evaluation tick (storm units pre-computed).
#[derive(Debug, Clone, Default)]
pub struct AlertInputs {
    pub system_state: String,
    pub n_failed_units: u32,
    pub units: Vec<UnitSample>,
    pub timers: Vec<TimerSample>,
    /// Units flagged by the restart-storm window: `(name, restarts_in_window)`.
    pub storm_units: Vec<(String, u32)>,
    /// Current wall-clock µs (for the timer-overdue comparison).
    pub now_usec: u64,
}

/// The firing alerts for one rule this tick (empty = nothing firing, but the rule
/// is still reconciled so recovered conditions resolve).
pub struct RuleAlerts {
    pub rule: String,
    pub alerts: Vec<Alert>,
}

fn alert(host: &str, rule: &str, severity: AlertSeverity, summary: String) -> Alert {
    Alert::new(
        host,
        Protocol::Systemd,
        AlertKind::SensorHealth,
        rule,
        severity,
        summary,
    )
}

/// Pure evaluation: given the derived inputs, produce the firing alerts per
/// enabled rule. One `RuleAlerts` per enabled rule (possibly empty).
pub fn evaluate(host: &str, cfg: &AlertsConfig, inputs: &AlertInputs) -> Vec<RuleAlerts> {
    let mut out = Vec::new();

    if cfg.unit_failed {
        let alerts = inputs
            .units
            .iter()
            .filter(|u| u.is_failed())
            .map(|u| {
                alert(
                    host,
                    UNIT_FAILED_RULE,
                    AlertSeverity::Critical,
                    format!("unit {} failed (exit {})", u.name, u.exec_main_status),
                )
                .with_label("unit", u.name.clone())
                .with_label("exit_code", u.exec_main_status.to_string())
            })
            .collect();
        out.push(RuleAlerts {
            rule: UNIT_FAILED_RULE.to_string(),
            alerts,
        });
    }

    if cfg.system_degraded {
        let mut alerts = Vec::new();
        if inputs.system_state == "degraded" || inputs.n_failed_units > 0 {
            alerts.push(
                alert(
                    host,
                    DEGRADED_RULE,
                    AlertSeverity::Warning,
                    format!(
                        "system {} ({} failed unit(s))",
                        inputs.system_state, inputs.n_failed_units
                    ),
                )
                .with_label("system_state", inputs.system_state.clone())
                .with_label("n_failed_units", inputs.n_failed_units.to_string()),
            );
        }
        out.push(RuleAlerts {
            rule: DEGRADED_RULE.to_string(),
            alerts,
        });
    }

    if cfg.restart_storm_threshold > 0 {
        let alerts = inputs
            .storm_units
            .iter()
            .map(|(name, n)| {
                alert(
                    host,
                    RESTART_STORM_RULE,
                    AlertSeverity::Warning,
                    format!(
                        "unit {name} restarted {n} times within {}s",
                        cfg.restart_storm_window_secs
                    ),
                )
                .with_label("unit", name.clone())
                .with_label("restarts", n.to_string())
            })
            .collect();
        out.push(RuleAlerts {
            rule: RESTART_STORM_RULE.to_string(),
            alerts,
        });
    }

    if cfg.timer_overdue_grace_secs > 0 {
        let grace_usec = cfg.timer_overdue_grace_secs.saturating_mul(1_000_000);
        let alerts = inputs
            .timers
            .iter()
            .filter(|t| {
                let n = t.next_elapse_usec_realtime;
                n > 0 && n != u64::MAX && inputs.now_usec > n.saturating_add(grace_usec)
            })
            .map(|t| {
                let overdue_secs =
                    (inputs.now_usec.saturating_sub(t.next_elapse_usec_realtime)) / 1_000_000;
                alert(
                    host,
                    TIMER_OVERDUE_RULE,
                    AlertSeverity::Warning,
                    format!("timer {} overdue by {overdue_secs}s", t.name),
                )
                .with_label("unit", t.name.clone())
                .with_label("overdue_secs", overdue_secs.to_string())
            })
            .collect();
        out.push(RuleAlerts {
            rule: TIMER_OVERDUE_RULE.to_string(),
            alerts,
        });
    }

    if cfg.unit_mem_ceiling_bytes > 0 {
        let ceiling = cfg.unit_mem_ceiling_bytes;
        let alerts = inputs
            .units
            .iter()
            .filter_map(|u| u.mem_bytes.map(|m| (u, m)))
            .filter(|(_, m)| *m > ceiling)
            .map(|(u, m)| {
                alert(
                    host,
                    UNIT_MEM_RULE,
                    AlertSeverity::Warning,
                    format!("unit {} memory {m} bytes over ceiling {ceiling}", u.name),
                )
                .with_label("unit", u.name.clone())
                .with_label("mem_bytes", m.to_string())
            })
            .collect();
        out.push(RuleAlerts {
            rule: UNIT_MEM_RULE.to_string(),
            alerts,
        });
    }

    out
}

/// Per-unit sliding restart window: the base `NRestarts` at the window start.
struct RestartWindow {
    start: Instant,
    base: u32,
}

/// Compute the restart-storm units from the current `NRestarts` against the
/// per-unit sliding window (resetting the base when the window elapses or the
/// counter goes backwards). Pure over the passed-in window map — unit-testable
/// without a reporter.
fn restart_storm(
    windows: &mut HashMap<String, RestartWindow>,
    units: &[UnitSample],
    now: Instant,
    threshold: u32,
    window: Duration,
) -> Vec<(String, u32)> {
    if threshold == 0 {
        return Vec::new();
    }
    let mut storm = Vec::new();
    for u in units {
        let w = windows.entry(u.name.clone()).or_insert(RestartWindow {
            start: now,
            base: u.n_restarts,
        });
        if now.duration_since(w.start) >= window || u.n_restarts < w.base {
            w.start = now;
            w.base = u.n_restarts;
        }
        let delta = u.n_restarts.saturating_sub(w.base);
        if delta >= threshold {
            storm.push((u.name.clone(), delta));
        }
    }
    storm
}

/// Drives [`evaluate`] each tick, holding the restart-window state and the
/// [`AlertReporter`].
pub struct AlertEvaluator {
    host: String,
    cfg: AlertsConfig,
    reporter: Arc<AlertReporter>,
    restart_windows: HashMap<String, RestartWindow>,
}

impl AlertEvaluator {
    pub fn new(host: String, cfg: AlertsConfig, reporter: Arc<AlertReporter>) -> Self {
        Self {
            host,
            cfg,
            reporter,
            restart_windows: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Compute the restart-storm units from the current `NRestarts` and the
    /// per-unit sliding window (resetting the base when the window elapses).
    fn storm_units(&mut self, units: &[UnitSample], now: Instant) -> Vec<(String, u32)> {
        let window = Duration::from_secs(self.cfg.restart_storm_window_secs.max(1));
        restart_storm(
            &mut self.restart_windows,
            units,
            now,
            self.cfg.restart_storm_threshold,
            window,
        )
    }

    /// Evaluate + reconcile for this tick. `system_state`/`n_failed_units`/`units`/
    /// `timers` are the freshly-read state; `now_usec` is the current wall clock.
    pub async fn tick(
        &mut self,
        system_state: String,
        n_failed_units: u32,
        units: Vec<UnitSample>,
        timers: Vec<TimerSample>,
        now_usec: u64,
        now: Instant,
    ) {
        if !self.cfg.enabled {
            return;
        }
        let storm_units = self.storm_units(&units, now);
        let inputs = AlertInputs {
            system_state,
            n_failed_units,
            units,
            timers,
            storm_units,
            now_usec,
        };
        let for_duration = (self.cfg.for_secs > 0).then(|| Duration::from_secs(self.cfg.for_secs));
        for ra in evaluate(&self.host, &self.cfg, &inputs) {
            let mut firing_keys = Vec::with_capacity(ra.alerts.len());
            for a in ra.alerts {
                firing_keys.push(a.alert_key());
                if let Err(e) = self.reporter.observe(a, for_duration).await {
                    warn!(error = %e, rule = %ra.rule, "systemd: failed to publish alert");
                }
            }
            if let Err(e) = self.reporter.reconcile(&ra.rule, &firing_keys).await {
                warn!(error = %e, rule = %ra.rule, "systemd: failed to reconcile alerts");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "host01";

    fn sample(name: &str, active: &str) -> UnitSample {
        UnitSample {
            name: name.into(),
            active_state: active.into(),
            ..Default::default()
        }
    }

    fn rule<'a>(out: &'a [RuleAlerts], rule: &str) -> &'a RuleAlerts {
        out.iter().find(|r| r.rule == rule).expect("rule present")
    }

    #[test]
    fn unit_failed_fires_per_failed_watched_unit() {
        let cfg = AlertsConfig::default();
        let inputs = AlertInputs {
            units: vec![
                sample("ok.service", "active"),
                sample("bad.service", "failed"),
            ],
            ..Default::default()
        };
        let out = evaluate(HOST, &cfg, &inputs);
        let ra = rule(&out, UNIT_FAILED_RULE);
        assert_eq!(ra.alerts.len(), 1);
        assert_eq!(ra.alerts[0].severity, AlertSeverity::Critical);
        assert_eq!(
            ra.alerts[0].labels.get("unit").map(String::as_str),
            Some("bad.service")
        );
    }

    #[test]
    fn degraded_fires_on_system_state_or_failed_count() {
        let cfg = AlertsConfig::default();
        // Clean system → no alert, rule still present for reconcile.
        let clean = AlertInputs {
            system_state: "running".into(),
            n_failed_units: 0,
            ..Default::default()
        };
        assert!(
            rule(&evaluate(HOST, &cfg, &clean), DEGRADED_RULE)
                .alerts
                .is_empty()
        );
        // Degraded state fires.
        let deg = AlertInputs {
            system_state: "degraded".into(),
            ..Default::default()
        };
        assert_eq!(
            rule(&evaluate(HOST, &cfg, &deg), DEGRADED_RULE)
                .alerts
                .len(),
            1
        );
        // A non-zero failed count fires even if state string is "running".
        let failed = AlertInputs {
            system_state: "running".into(),
            n_failed_units: 2,
            ..Default::default()
        };
        assert_eq!(
            rule(&evaluate(HOST, &cfg, &failed), DEGRADED_RULE)
                .alerts
                .len(),
            1
        );
    }

    #[test]
    fn timer_overdue_respects_grace() {
        let cfg = AlertsConfig {
            timer_overdue_grace_secs: 60,
            ..Default::default()
        };
        let now = 1_000_000_000u64; // µs
        // Next elapse 30s ago → within grace, no alert.
        let within = AlertInputs {
            timers: vec![TimerSample {
                name: "a.timer".into(),
                next_elapse_usec_realtime: now - 30_000_000,
            }],
            now_usec: now,
            ..Default::default()
        };
        assert!(
            rule(&evaluate(HOST, &cfg, &within), TIMER_OVERDUE_RULE)
                .alerts
                .is_empty()
        );
        // Next elapse 120s ago → past the 60s grace, fires.
        let overdue = AlertInputs {
            timers: vec![TimerSample {
                name: "a.timer".into(),
                next_elapse_usec_realtime: now - 120_000_000,
            }],
            now_usec: now,
            ..Default::default()
        };
        assert_eq!(
            rule(&evaluate(HOST, &cfg, &overdue), TIMER_OVERDUE_RULE)
                .alerts
                .len(),
            1
        );
        // A never-scheduled timer (0) never fires.
        let never = AlertInputs {
            timers: vec![TimerSample {
                name: "b.timer".into(),
                next_elapse_usec_realtime: 0,
            }],
            now_usec: now,
            ..Default::default()
        };
        assert!(
            rule(&evaluate(HOST, &cfg, &never), TIMER_OVERDUE_RULE)
                .alerts
                .is_empty()
        );
    }

    #[test]
    fn unit_mem_fires_over_ceiling_only() {
        let cfg = AlertsConfig {
            unit_mem_ceiling_bytes: 1000,
            ..Default::default()
        };
        let mut over = sample("big.service", "active");
        over.mem_bytes = Some(2000);
        let mut under = sample("small.service", "active");
        under.mem_bytes = Some(500);
        let mut noacct = sample("noacct.service", "active");
        noacct.mem_bytes = None;
        let inputs = AlertInputs {
            units: vec![over, under, noacct],
            ..Default::default()
        };
        let ra = rule(&evaluate(HOST, &cfg, &inputs), UNIT_MEM_RULE);
        assert_eq!(ra.alerts.len(), 1);
        assert_eq!(
            ra.alerts[0].labels.get("unit").map(String::as_str),
            Some("big.service")
        );
        // Ceiling 0 disables the rule entirely (not even present).
        let disabled = AlertsConfig {
            unit_mem_ceiling_bytes: 0,
            ..Default::default()
        };
        assert!(
            !evaluate(HOST, &disabled, &inputs)
                .iter()
                .any(|r| r.rule == UNIT_MEM_RULE)
        );
    }

    #[test]
    fn restart_storm_windowing() {
        let mut windows = HashMap::new();
        let window = Duration::from_secs(300);
        let t0 = Instant::now();
        let mut u = sample("flap.service", "active");
        u.n_restarts = 5;
        // First sight sets the base at 5 → delta 0, no storm.
        assert!(restart_storm(&mut windows, std::slice::from_ref(&u), t0, 3, window).is_empty());
        // Climbs to 8 within the window → delta 3 ≥ threshold → storm.
        u.n_restarts = 8;
        assert_eq!(
            restart_storm(&mut windows, std::slice::from_ref(&u), t0, 3, window),
            vec![("flap.service".to_string(), 3)]
        );
        // Counter reset (unit reloaded) rebases → no storm.
        u.n_restarts = 1;
        assert!(restart_storm(&mut windows, std::slice::from_ref(&u), t0, 3, window).is_empty());
    }

    #[test]
    fn restart_storm_disabled_at_zero_threshold() {
        let mut windows = HashMap::new();
        let mut u = sample("x.service", "active");
        u.n_restarts = 99;
        assert!(
            restart_storm(
                &mut windows,
                &[u],
                Instant::now(),
                0,
                Duration::from_secs(1)
            )
            .is_empty()
        );
    }
}
