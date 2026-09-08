//! Embedded unit sentinel (#277): declarative service-health expectations →
//! alerts, hot-swappable at runtime. Mirrors the netlink sentinel.
//!
//! Expectations are evaluated on the D-Bus event stream (instant, via a `Notify`
//! nudge) and on a slow poll. Each deviation raises an [`AlertKind::Expectation`]
//! alert via the [`AlertReporter`] (firing → resolved → tombstone); each rule is
//! reconciled every sweep so a recovered expectation auto-resolves. The rule set
//! lives behind an `Arc<RwLock<…>>` so [`SentinelHandle`] can swap it live
//! (`@rpc/systemd/expectations/set`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, RwLock};
use tracing::warn;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

use crate::dbus::{ManagerProxy, TimerProxy, UnitProxy};
use crate::restart_window::RestartWindow;

pub const SERVICE_ACTIVE_RULE: &str = "expect-service-active";
pub const TARGET_ACTIVE_RULE: &str = "expect-target-active";
pub const TIMER_RULE: &str = "expect-timer";
pub const TIMER_SUCCEEDED_RULE: &str = "expect-timer-succeeded";
pub const RESTART_RATE_RULE: &str = "expect-restart-rate";
pub const FORBID_FAILED_RULE: &str = "forbid-failed";

// The expectation vocabulary itself lives in `zensight-common::systemd`
// (#849), for the same reason hostspec's does (#816): it is a WIRE CONTRACT
// with three consumers — this sensor, the GUI that authors it, and the
// `@desired` fleet author — and a state-class payload needs a real
// schemars-generated schema, which a sensor-crate type can never provide.
// Re-exported here so every existing `sentinel::ExpectationsConfig` path
// keeps working; the checking below is what stayed.
pub use zensight_common::systemd::{
    ExpectationsConfig, RestartRateExpectation, ServiceActiveExpectation, TargetActiveExpectation,
    TimerExpectation,
};

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

/// A timer satisfies "succeeded within `within_secs`" iff it *fired* within
/// the window **and** its triggered service's last completed run succeeded
/// (`Service.Result == "success"`). An unreadable result (`None` — no such
/// service, D-Bus refusal) is **not** satisfied: "could not check" must never
/// read as "passed" (#824).
pub fn timer_succeeded_ok(
    last_trigger_usec: u64,
    now_usec: u64,
    within_secs: u64,
    service_result: Option<&str>,
) -> bool {
    timer_ok(last_trigger_usec, now_usec, within_secs) && service_result == Some("success")
}

// ─── Hot-swap handle ─────────────────────────────────────────────────────────

/// Runtime handle to the sentinel's expectation set (`@rpc/systemd/expectations/set`).
#[derive(Clone)]
pub struct SentinelHandle {
    expectations: Arc<RwLock<ExpectationsConfig>>,
}

impl SentinelHandle {
    /// Replace the entire expectation set.
    pub async fn replace(&self, cfg: ExpectationsConfig) {
        *self.expectations.write().await = cfg;
    }
    /// Snapshot the current set (for the `@rpc/systemd/expectations` read).
    pub async fn snapshot(&self) -> ExpectationsConfig {
        self.expectations.read().await.clone()
    }
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
        let mut interval_secs = self.eval_interval_secs().await;
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
            // The interval is part of the hot-swappable set (`@rpc/…/set`,
            // `@desired`). It used to be read once at startup, so a swapped
            // set was stamped "applied" on the marker while the sensor kept
            // sweeping at the old cadence — the marker asserting something
            // false about the one thing it exists to be honest about.
            let wanted = self.eval_interval_secs().await;
            if wanted != interval_secs {
                tracing::info!(
                    from = interval_secs,
                    to = wanted,
                    "systemd sentinel: eval interval changed by a hot-swapped set"
                );
                interval_secs = wanted;
                let period = Duration::from_secs(interval_secs);
                tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            }
        }
    }

    async fn eval_interval_secs(&self) -> u64 {
        self.expectations.read().await.eval_interval_secs.max(1)
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
        // The set's recovery hold (#932): how long an expectation must be
        // continuously satisfied again before its alert resolves. `Some(0)`
        // resolves on the first passing sweep, which is what this did before
        // the field existed.
        let recover_after = Some(Duration::from_secs(exp.recover_after_secs));

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
        self.reconcile(SERVICE_ACTIVE_RULE, &svc_keys, recover_after)
            .await;

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
        self.reconcile(TARGET_ACTIVE_RULE, &tgt_keys, recover_after)
            .await;

        // expect timer triggered / succeeded within.
        let now = Self::now_usec();
        let mut timer_keys = Vec::new();
        let mut timer_ok_keys = Vec::new();
        for e in &exp.timers {
            if e.within_secs.is_none() && e.succeeded_within_secs.is_none() {
                warn!(
                    timer = %e.timer,
                    "sentinel: timer expectation with neither within_secs nor \
                     succeeded_within_secs checks nothing"
                );
                continue;
            }
            let last = self
                .timer_last_trigger(&manager, &e.timer)
                .await
                .unwrap_or(0);
            if let Some(within) = e.within_secs
                && !timer_ok(last, now, within)
            {
                let a = self.alert(
                    TIMER_RULE,
                    AlertSeverity::Warning,
                    &e.timer,
                    format!("expected timer {} triggered within {within}s", e.timer),
                );
                timer_keys.push(a.alert_key());
                self.observe(a, for_duration).await;
            }
            // The stronger form (#824): the timer fired AND its triggered
            // service's last run succeeded. `LastTriggerUSec` advancing on
            // schedule says nothing about the run's outcome.
            if let Some(within) = e.succeeded_within_secs {
                let (triggered_unit, result) = self.timer_service_result(&manager, &e.timer).await;
                if !timer_succeeded_ok(last, now, within, result.as_deref()) {
                    let outcome = match &result {
                        Some(r) => format!("last run: {r}"),
                        None => "run outcome unreadable".to_string(),
                    };
                    let a = self.alert(
                        TIMER_SUCCEEDED_RULE,
                        AlertSeverity::Warning,
                        &e.timer,
                        format!(
                            "expected timer {} to have a successful {} run within {within}s ({outcome})",
                            e.timer, triggered_unit
                        ),
                    );
                    timer_ok_keys.push(a.alert_key());
                    self.observe(a, for_duration).await;
                }
            }
        }
        self.reconcile(TIMER_RULE, &timer_keys, recover_after).await;
        self.reconcile(TIMER_SUCCEEDED_RULE, &timer_ok_keys, recover_after)
            .await;

        // expect restart rate below a ceiling.
        let mut rate_keys = Vec::new();
        for e in &exp.restart_rates {
            let restarts = self.n_restarts(&manager, &e.unit).await.unwrap_or(0);
            // The expectation is "restarts < max per window" (the type's own
            // doc, and configuration.md), so `max` itself is the first
            // violation — `>`, which this read for a while, let exactly `max`
            // restarts through in silence.
            if self.restart_delta(&e.unit, restarts, e.window_secs) >= e.max {
                let a = self.alert(
                    RESTART_RATE_RULE,
                    AlertSeverity::Warning,
                    &e.unit,
                    format!(
                        "service {} restart rate reached {}/{}s",
                        e.unit, e.max, e.window_secs
                    ),
                );
                rate_keys.push(a.alert_key());
                self.observe(a, for_duration).await;
            }
        }
        self.reconcile(RESTART_RATE_RULE, &rate_keys, recover_after)
            .await;

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
            self.reconcile(FORBID_FAILED_RULE, &failed_keys, recover_after)
                .await;
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
    /// `recover_after` is the set's own recovery hold (#932), passed
    /// explicitly for the same reason `observe` passes the debounce
    /// explicitly: this reporter is SHARED with the #276 threshold alerts and
    /// the #931 operator rules, whose windows are their own.
    async fn reconcile(&self, rule: &str, firing: &[String], recover_after: Option<Duration>) {
        let opts = zensight_sensor_core::ReconcileOpts { recover_after };
        if let Err(e) = self.reporter.reconcile_opts(rule, firing, opts).await {
            warn!(error = %e, rule, "sentinel: reconcile failed");
        }
    }

    /// Restart delta over the sliding window (rebasing when the window elapses or
    /// the counter resets).
    fn restart_delta(&self, unit: &str, restarts: u32, window_secs: u64) -> u32 {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs.max(1));
        let mut w = self.restart_windows.lock().expect("restart windows");
        w.entry(unit.to_string())
            .or_default()
            .observe(restarts, now, window)
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
    /// The unit a timer triggers (`Timer.Unit`, falling back to the
    /// `<name>.service` convention when unreadable) and that unit's
    /// `Service.Result` — `None` when the outcome cannot be read, which the
    /// caller must treat as *not satisfied*, never as success (#824).
    async fn timer_service_result(
        &self,
        manager: &ManagerProxy<'_>,
        timer: &str,
    ) -> (String, Option<String>) {
        let triggered = async {
            let path = manager.load_unit(timer).await.ok()?;
            let p = TimerProxy::builder(&self.conn)
                .path(path)
                .ok()?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await
                .ok()?;
            p.unit().await.ok().filter(|u| !u.is_empty())
        }
        .await
        .unwrap_or_else(|| format!("{}.service", timer.trim_end_matches(".timer")));

        let result = async {
            let path = manager.load_unit(&triggered).await.ok()?;
            let p = crate::dbus::ServiceProxy::builder(&self.conn)
                .path(path)
                .ok()?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await
                .ok()?;
            p.result().await.ok()
        }
        .await;
        (triggered, result)
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

/// The gate both writers run — the RPC `expectations/set` and the `@desired`
/// reconciler — before a set reaches the handle. An invalid set is refused
/// with a reason a caller (or the `applied/expectations` marker's
/// `last_rejected`) can show; the previous good set keeps running. Mirrors
/// hostspec's `validate` (#816). Before it existed the systemd apply closure
/// was `Ok(())` unconditionally, so `eval_interval_secs: 0`, a timer with no
/// window and a restart rate over a zero window were all accepted and
/// stamped `source: desired`.
pub fn validate(cfg: &ExpectationsConfig) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    if cfg.eval_interval_secs == 0 {
        errs.push("eval_interval_secs must be >= 1".into());
    }
    let mut seen: std::collections::HashSet<(&'static str, String)> = Default::default();
    let mut name = |kind: &'static str, n: &str, errs: &mut Vec<String>| {
        if n.trim().is_empty() {
            errs.push(format!("{kind}: an expectation has an empty unit name"));
        } else if !seen.insert((kind, n.to_string())) {
            errs.push(format!(
                "{kind}:{n}: duplicate — two expectations on one unit would cross-resolve"
            ));
        }
    };
    for e in &cfg.services_active {
        name("services_active", &e.unit, &mut errs);
    }
    for e in &cfg.targets_active {
        name("targets_active", &e.target, &mut errs);
    }
    for t in &cfg.timers {
        name("timers", &t.timer, &mut errs);
        if t.within_secs.is_none() && t.succeeded_within_secs.is_none() {
            errs.push(format!(
                "timers:{}: neither within_secs nor succeeded_within_secs — the expectation \
                 would be inert",
                t.timer
            ));
        }
        if t.within_secs == Some(0) || t.succeeded_within_secs == Some(0) {
            errs.push(format!(
                "timers:{}: a window of 0 seconds can never be met",
                t.timer
            ));
        }
    }
    for r in &cfg.restart_rates {
        name("restart_rates", &r.unit, &mut errs);
        if r.window_secs == 0 {
            errs.push(format!(
                "restart_rates:{}: window_secs must be >= 1",
                r.unit
            ));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate both writers share. Each refusal names the expectation, so an
    /// operator reading `last_rejected` on the marker knows which line to fix.
    #[test]
    fn validate_refuses_an_inert_or_impossible_set() {
        let mut cfg = ExpectationsConfig::default();
        assert!(
            validate(&cfg).is_ok(),
            "the empty set is valid — and the stock install"
        );

        cfg.eval_interval_secs = 0;
        cfg.timers.push(TimerExpectation {
            timer: "backup.timer".into(),
            within_secs: None,
            succeeded_within_secs: None,
        });
        cfg.restart_rates.push(RestartRateExpectation {
            unit: "nginx.service".into(),
            max: 5,
            window_secs: 0,
        });
        cfg.services_active
            .push(ServiceActiveExpectation { unit: " ".into() });
        cfg.services_active.push(ServiceActiveExpectation {
            unit: "sshd.service".into(),
        });
        cfg.services_active.push(ServiceActiveExpectation {
            unit: "sshd.service".into(),
        });
        let err = validate(&cfg).expect_err("every one of these is a refusal");
        for needle in [
            "eval_interval_secs",
            "backup.timer",
            "inert",
            "nginx.service",
            "window_secs",
            "empty unit name",
            "duplicate",
        ] {
            assert!(err.contains(needle), "missing {needle:?} in: {err}");
        }

        cfg.eval_interval_secs = 10;
        cfg.timers[0].within_secs = Some(3600);
        cfg.restart_rates[0].window_secs = 600;
        cfg.services_active.remove(0);
        cfg.services_active.pop();
        assert!(validate(&cfg).is_ok(), "{:?}", validate(&cfg));
    }

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

    /// #824: the timer fired on schedule — and that alone must not satisfy
    /// the *succeeded* form. LastTriggerUSec advanced hourly for eight days
    /// while every run failed; this is the check that would have caught it
    /// on day one.
    #[test]
    fn timer_succeeded_needs_both_the_fire_and_the_success() {
        let now = 1_000_000_000u64;
        let fired_recently = now - 30_000_000;
        // Fired + succeeded → satisfied.
        assert!(timer_succeeded_ok(fired_recently, now, 60, Some("success")));
        // Fired on schedule, service failing → NOT satisfied (the cosign case).
        assert!(!timer_succeeded_ok(
            fired_recently,
            now,
            60,
            Some("exit-code")
        ));
        assert!(!timer_succeeded_ok(
            fired_recently,
            now,
            60,
            Some("timeout")
        ));
        // Unreadable outcome is "could not check", never "passed".
        assert!(!timer_succeeded_ok(fired_recently, now, 60, None));
        // Didn't fire in the window at all → not satisfied, success or not.
        assert!(!timer_succeeded_ok(
            now - 120_000_000,
            now,
            60,
            Some("success")
        ));
        assert!(!timer_succeeded_ok(0, now, 60, Some("success")));
    }

    #[test]
    fn timer_expectation_accepts_either_form() {
        // The issue's "one-word difference": the declarative form parses with
        // either window, or both.
        let json = r#"{ "timers": [
            { "timer": "logrotate.timer", "within_secs": 90000 },
            { "timer": "cosign.timer", "succeeded_within_secs": 3900 },
            { "timer": "both.timer", "within_secs": 60, "succeeded_within_secs": 120 }
        ] }"#;
        let cfg: ExpectationsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.timers[0].within_secs, Some(90_000));
        assert_eq!(cfg.timers[0].succeeded_within_secs, None);
        assert_eq!(cfg.timers[1].succeeded_within_secs, Some(3_900));
        assert_eq!(cfg.timers[2].within_secs, Some(60));
        assert_eq!(cfg.timers[2].succeeded_within_secs, Some(120));
        // Round-trips through its own serialization.
        let back: ExpectationsConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, back);
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
                within_secs: Some(90_000),
                succeeded_within_secs: None,
            }],
            forbid_failed: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ExpectationsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }
}
