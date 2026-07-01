//! Embedded unit sentinel (#277): declarative service-health expectations →
//! alerts, hot-swappable at runtime. Mirrors the netlink sentinel.
//!
//! Expectations are evaluated on the D-Bus event stream (instant, via a `Notify`
//! nudge) and on a slow poll. Each deviation raises an [`AlertKind::Expectation`]
//! alert via the [`AlertReporter`] (firing → resolved → tombstone); each rule is
//! reconciled every sweep so a recovered expectation auto-resolves. The rule set
//! lives behind an `Arc<RwLock<…>>` so [`SentinelHandle`] can swap it live
//! (`@/commands/expectations`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};
use tracing::warn;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

use crate::dbus::{ManagerProxy, TimerProxy, UnitProxy};

pub const SERVICE_ACTIVE_RULE: &str = "expect-service-active";
pub const TARGET_ACTIVE_RULE: &str = "expect-target-active";
pub const TIMER_RULE: &str = "expect-timer";
pub const RESTART_RATE_RULE: &str = "expect-restart-rate";
pub const FORBID_FAILED_RULE: &str = "forbid-failed";

/// "expect service `<unit>` active".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceActiveExpectation {
    pub unit: String,
}

/// "expect target `<target>` active".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetActiveExpectation {
    pub target: String,
}

/// "expect timer `<timer>` triggered within `<within_secs>`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerExpectation {
    pub timer: String,
    pub within_secs: u64,
}

/// "expect service `<unit>` restarts_rate < `<max>` per `<window_secs>`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartRateExpectation {
    pub unit: String,
    pub max: u32,
    pub window_secs: u64,
}

/// The full declarative expectation set (seeded from config, hot-swappable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectationsConfig {
    #[serde(default = "default_eval_interval_secs")]
    pub eval_interval_secs: u64,
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    #[serde(default)]
    pub services_active: Vec<ServiceActiveExpectation>,
    #[serde(default)]
    pub targets_active: Vec<TargetActiveExpectation>,
    #[serde(default)]
    pub timers: Vec<TimerExpectation>,
    #[serde(default)]
    pub restart_rates: Vec<RestartRateExpectation>,
    /// "forbid any unit in state failed".
    #[serde(default)]
    pub forbid_failed: bool,
}

fn default_eval_interval_secs() -> u64 {
    10
}
fn default_for_secs() -> u64 {
    15
}

// ─── Pure checks (unit-testable) ─────────────────────────────────────────────

/// A service/target satisfies an "active" expectation iff its `ActiveState` is
/// `active`.
pub fn active_ok(state: Option<&str>) -> bool {
    state == Some("active")
}

/// A timer satisfies "triggered within `within_secs`" iff it fired within the
/// window (`last_trigger_usec` non-zero and recent enough).
pub fn timer_ok(last_trigger_usec: u64, now_usec: u64, within_secs: u64) -> bool {
    if last_trigger_usec == 0 || last_trigger_usec == u64::MAX {
        return false; // never triggered → not satisfied
    }
    let age_usec = now_usec.saturating_sub(last_trigger_usec);
    age_usec <= within_secs.saturating_mul(1_000_000)
}

// ─── Hot-swap handle ─────────────────────────────────────────────────────────

/// Runtime handle to the sentinel's expectation set (`@/commands/expectations`).
#[derive(Clone)]
pub struct SentinelHandle {
    expectations: Arc<RwLock<ExpectationsConfig>>,
}

impl SentinelHandle {
    /// Replace the entire expectation set.
    pub async fn replace(&self, cfg: ExpectationsConfig) {
        *self.expectations.write().await = cfg;
    }
    /// Snapshot the current set (for `@/status/expectations`).
    pub async fn snapshot(&self) -> ExpectationsConfig {
        self.expectations.read().await.clone()
    }
}

/// Per-unit sliding restart window base.
struct RestartWindow {
    start: Instant,
    base: u32,
}

/// The sentinel evaluator: reads unit state from D-Bus and reconciles expectation
/// alerts.
pub struct Evaluator {
    host: String,
    expectations: Arc<RwLock<ExpectationsConfig>>,
    reporter: Arc<AlertReporter>,
    conn: zbus::Connection,
    restart_windows: Mutex<HashMap<String, RestartWindow>>,
    wake: Option<Arc<Notify>>,
}

impl Evaluator {
    pub fn new(
        host: String,
        config: ExpectationsConfig,
        reporter: Arc<AlertReporter>,
        conn: zbus::Connection,
    ) -> Self {
        Self {
            host,
            expectations: Arc::new(RwLock::new(config)),
            reporter,
            conn,
            restart_windows: Mutex::new(HashMap::new()),
            wake: None,
        }
    }

    /// Attach a `Notify` so the event stream can trigger an instant re-eval.
    pub fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Extract the hot-swap handle before spawning `run`.
    pub fn handle(&self) -> SentinelHandle {
        SentinelHandle {
            expectations: self.expectations.clone(),
        }
    }

    /// Run the sentinel until the session closes: sweep on a slow poll and on any
    /// event-stream nudge.
    pub async fn run(self) {
        let interval_secs = {
            let e = self.expectations.read().await;
            e.eval_interval_secs.max(1)
        };
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        let wake = self.wake.clone();
        tracing::info!("systemd sentinel ready (interval {interval_secs}s)");
        loop {
            match &wake {
                Some(w) => {
                    tokio::select! {
                        _ = tick.tick() => {}
                        _ = w.notified() => {}
                    }
                }
                None => {
                    tick.tick().await;
                }
            }
            self.sweep().await;
        }
    }

    /// Current wall-clock µs.
    fn now_usec() -> u64 {
        chrono::Utc::now().timestamp_micros().max(0) as u64
    }

    /// One evaluation sweep: check every expectation, observe violations, and
    /// reconcile each rule so recovered expectations resolve.
    async fn sweep(&self) {
        let exp = self.expectations.read().await.clone();
        let manager = match ManagerProxy::new(&self.conn).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "sentinel: Manager proxy failed");
                return;
            }
        };
        // Always pass an explicit debounce so the sentinel's `for_secs` governs
        // (the shared reporter's base debounce is the threshold-alerts one, which
        // may differ); `0` → publish immediately.
        let for_duration = Some(Duration::from_secs(exp.for_secs));

        // expect service/target active.
        let mut svc_keys = Vec::new();
        for e in &exp.services_active {
            if !active_ok(self.active_state(&manager, &e.unit).await.as_deref()) {
                let a = self.alert(
                    SERVICE_ACTIVE_RULE,
                    AlertSeverity::Critical,
                    &e.unit,
                    format!("expected service {} active", e.unit),
                );
                svc_keys.push(a.alert_key());
                self.observe(a, for_duration).await;
            }
        }
        self.reconcile(SERVICE_ACTIVE_RULE, &svc_keys).await;

        let mut tgt_keys = Vec::new();
        for e in &exp.targets_active {
            if !active_ok(self.active_state(&manager, &e.target).await.as_deref()) {
                let a = self.alert(
                    TARGET_ACTIVE_RULE,
                    AlertSeverity::Warning,
                    &e.target,
                    format!("expected target {} active", e.target),
                );
                tgt_keys.push(a.alert_key());
                self.observe(a, for_duration).await;
            }
        }
        self.reconcile(TARGET_ACTIVE_RULE, &tgt_keys).await;

        // expect timer triggered within.
        let now = Self::now_usec();
        let mut timer_keys = Vec::new();
        for e in &exp.timers {
            let last = self
                .timer_last_trigger(&manager, &e.timer)
                .await
                .unwrap_or(0);
            if !timer_ok(last, now, e.within_secs) {
                let a = self.alert(
                    TIMER_RULE,
                    AlertSeverity::Warning,
                    &e.timer,
                    format!(
                        "expected timer {} triggered within {}s",
                        e.timer, e.within_secs
                    ),
                );
                timer_keys.push(a.alert_key());
                self.observe(a, for_duration).await;
            }
        }
        self.reconcile(TIMER_RULE, &timer_keys).await;

        // expect restart rate below a ceiling.
        let mut rate_keys = Vec::new();
        for e in &exp.restart_rates {
            let restarts = self.n_restarts(&manager, &e.unit).await.unwrap_or(0);
            if self.restart_delta(&e.unit, restarts, e.window_secs) > e.max {
                let a = self.alert(
                    RESTART_RATE_RULE,
                    AlertSeverity::Warning,
                    &e.unit,
                    format!(
                        "service {} restart rate exceeded {}/{}s",
                        e.unit, e.max, e.window_secs
                    ),
                );
                rate_keys.push(a.alert_key());
                self.observe(a, for_duration).await;
            }
        }
        self.reconcile(RESTART_RATE_RULE, &rate_keys).await;

        // forbid any failed unit.
        if exp.forbid_failed {
            let mut failed_keys = Vec::new();
            if let Ok(listed) = manager.list_units().await {
                for u in listed.iter().filter(|u| u.3 == "failed") {
                    let a = self.alert(
                        FORBID_FAILED_RULE,
                        AlertSeverity::Critical,
                        &u.0,
                        format!("unit {} is failed (forbidden)", u.0),
                    );
                    failed_keys.push(a.alert_key());
                    self.observe(a, for_duration).await;
                }
            }
            self.reconcile(FORBID_FAILED_RULE, &failed_keys).await;
        }
    }

    fn alert(&self, rule: &str, severity: AlertSeverity, unit: &str, summary: String) -> Alert {
        Alert::new(
            &self.host,
            Protocol::Systemd,
            AlertKind::Expectation,
            rule,
            severity,
            summary,
        )
        .with_label("unit", unit.to_string())
    }

    async fn observe(&self, a: Alert, for_duration: Option<Duration>) {
        if let Err(e) = self.reporter.observe(a, for_duration).await {
            warn!(error = %e, "sentinel: publish failed");
        }
    }
    async fn reconcile(&self, rule: &str, firing: &[String]) {
        if let Err(e) = self.reporter.reconcile(rule, firing).await {
            warn!(error = %e, rule, "sentinel: reconcile failed");
        }
    }

    /// Restart delta over the sliding window (rebasing when the window elapses or
    /// the counter resets).
    fn restart_delta(&self, unit: &str, restarts: u32, window_secs: u64) -> u32 {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs.max(1));
        let mut w = self.restart_windows.lock().expect("restart windows");
        let e = w.entry(unit.to_string()).or_insert(RestartWindow {
            start: now,
            base: restarts,
        });
        if now.duration_since(e.start) >= window || restarts < e.base {
            e.start = now;
            e.base = restarts;
        }
        restarts.saturating_sub(e.base)
    }

    // ── D-Bus reads (best-effort, uncached: one-shot per sweep, and the eager
    // GetAll populate would warn on interface mismatch) ──
    async fn active_state(&self, manager: &ManagerProxy<'_>, unit: &str) -> Option<String> {
        let path = manager.load_unit(unit).await.ok()?;
        let p = UnitProxy::builder(&self.conn)
            .path(path)
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        p.active_state().await.ok()
    }
    async fn timer_last_trigger(&self, manager: &ManagerProxy<'_>, timer: &str) -> Option<u64> {
        let path = manager.load_unit(timer).await.ok()?;
        let p = TimerProxy::builder(&self.conn)
            .path(path)
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        p.last_trigger_usec().await.ok()
    }
    async fn n_restarts(&self, manager: &ManagerProxy<'_>, unit: &str) -> Option<u32> {
        let path = manager.load_unit(unit).await.ok()?;
        let p = crate::dbus::ServiceProxy::builder(&self.conn)
            .path(path)
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        p.n_restarts().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_ok_only_for_active() {
        assert!(active_ok(Some("active")));
        assert!(!active_ok(Some("failed")));
        assert!(!active_ok(Some("inactive")));
        assert!(!active_ok(None));
    }

    #[test]
    fn timer_ok_within_window() {
        let now = 1_000_000_000u64; // µs
        // Fired 30s ago, window 60s → ok.
        assert!(timer_ok(now - 30_000_000, now, 60));
        // Fired 120s ago, window 60s → not ok.
        assert!(!timer_ok(now - 120_000_000, now, 60));
        // Never fired → not ok.
        assert!(!timer_ok(0, now, 60));
        assert!(!timer_ok(u64::MAX, now, 60));
    }

    #[test]
    fn expectations_config_json_roundtrip() {
        let cfg = ExpectationsConfig {
            eval_interval_secs: 5,
            for_secs: 0,
            services_active: vec![ServiceActiveExpectation {
                unit: "sshd.service".into(),
            }],
            timers: vec![TimerExpectation {
                timer: "logrotate.timer".into(),
                within_secs: 90_000,
            }],
            forbid_failed: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ExpectationsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }
}
