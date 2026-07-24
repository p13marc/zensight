//! Log sentinel (#543): declarative, hot-swappable pattern→alert rules.
//!
//! Operators declare "when the logs say X, alert" rules — in config and at
//! runtime over `@rpc/logs/rules/set` — instead of needing a code change per
//! condition. Each log line flowing through the intake loop is matched against
//! the active ruleset; a match fires a structured alert through the shared
//! [`AlertReporter`] (the same firing/resolve/late-join machinery the other
//! alert families use). Optional `count >= N within window` thresholds suppress
//! single-line noise; one-shot rules auto-resolve after a quiet period.
//!
//! The four hardcoded journald known-events (coredump, unit-failed, OOM) ship as
//! **built-in rules** ([`builtin_rules`]) folded into this one mechanism, so a
//! custom `message_id` rule now needs no code — the old hardcoded-only limit is
//! gone.
//!
//! Design note: unlike the sibling netlink/systemd sentinels (which *poll* kernel
//! state on an interval), the log sentinel is **push-at-intake** — it evaluates
//! each record as it arrives, with only a periodic tick for windowed-threshold
//! bookkeeping and alert reconciliation.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::telemetry::Protocol;
use zensight_sensor_core::AlertReporter;

use crate::parser::SyslogMessage;

fn default_eval_interval() -> u64 {
    10
}
fn default_for_secs() -> u64 {
    300
}
fn default_summary_max() -> usize {
    160
}

/// The full sentinel ruleset — seeded from config, hot-swapped at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRulesConfig {
    /// How often (seconds) expired alerts are reconciled / windows pruned.
    #[serde(default = "default_eval_interval")]
    pub eval_interval_secs: u64,
    /// Include the shipped built-in known-event rules (coredump/OOM/unit-failed).
    /// On by default so upgrading keeps the known-events working unchanged.
    #[serde(default = "crate::config::default_true")]
    pub include_builtins: bool,
    /// Operator-declared rules.
    #[serde(default)]
    pub rules: Vec<LogRule>,
}

impl Default for LogRulesConfig {
    fn default() -> Self {
        Self {
            eval_interval_secs: default_eval_interval(),
            include_builtins: true,
            rules: Vec::new(),
        }
    }
}

/// One declarative rule: match criteria → an alert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRule {
    /// Stable id — the alert `rule` namespace and the hit-counter key. Must be
    /// unique; later duplicates are dropped at compile with a warning.
    pub id: String,
    /// Human description (optional; surfaced in the read RPC).
    #[serde(default)]
    pub description: Option<String>,
    /// Match criteria (all present fields must hold — AND).
    #[serde(default, rename = "match")]
    pub matcher: LogMatch,
    /// Optional `count >= N within window` threshold to avoid single-line noise.
    #[serde(default)]
    pub threshold: Option<Threshold>,
    /// Alert severity when the rule fires.
    #[serde(default)]
    pub severity: AlertSeverity,
    /// Summary template. `{message}`, `{unit}`, `{app}`, `{host}`, `{count}`,
    /// `{severity}` and regex capture groups `{1}`..`{9}` / `{name}` are
    /// substituted. Defaults to `"<id>: <truncated message>"`.
    #[serde(default)]
    pub summary: Option<String>,
    /// Journald / structured-data fields to lift into the alert labels (e.g.
    /// `coredump_exe`), on top of the always-included `unit`/`app`.
    #[serde(default)]
    pub labels_from: Vec<String>,
    /// Auto-resolve TTL: the alert clears this long after its last match
    /// (the "quiet period"). Defaults to 300s.
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
}

/// Match criteria for a [`LogRule`]. An empty matcher matches everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogMatch {
    /// Regex tested against the message text (unanchored).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Match lines at least this severe: syslog severity number `<=` this
    /// (0=emerg … 7=debug, lower is worse). `Some(4)` = warning-and-worse.
    #[serde(default)]
    pub min_severity: Option<u8>,
    /// Exact facility slug (e.g. `auth`).
    #[serde(default)]
    pub facility: Option<String>,
    /// Exact `_SYSTEMD_UNIT` (journald `unit` structured field).
    #[serde(default)]
    pub unit: Option<String>,
    /// Exact app / program name (syslog tag).
    #[serde(default)]
    pub app: Option<String>,
    /// Exact mined `template_id` (requires templating on).
    #[serde(default)]
    pub template_id: Option<String>,
    /// Exact journald `MESSAGE_ID` (32-char hex, case-insensitive).
    #[serde(default)]
    pub message_id: Option<String>,
}

/// A `count >= N within window` threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Threshold {
    pub count: u64,
    pub within_secs: u64,
}

// ---- compiled form -------------------------------------------------------

/// A rule with its regex compiled once and a lifetime hit counter.
struct CompiledRule {
    rule: LogRule,
    regex: Option<Regex>,
    hits: AtomicU64,
}

impl CompiledRule {
    /// True when every present criterion holds for `msg`.
    fn matches(&self, msg: &SyslogMessage) -> bool {
        let m = &self.rule.matcher;
        if let Some(min) = m.min_severity
            && (msg.severity as u8) > min
        {
            return false;
        }
        if let Some(fac) = &m.facility
            && msg.facility.as_str() != fac
        {
            return false;
        }
        if let Some(unit) = &m.unit
            && journald_field(msg, "unit").as_deref() != Some(unit.as_str())
        {
            return false;
        }
        if let Some(app) = &m.app
            && msg.app_name.as_deref() != Some(app.as_str())
        {
            return false;
        }
        if let Some(tid) = &m.template_id
            && msg
                .structured_data
                .get("zensight")
                .and_then(|s| s.get("template_id"))
                != Some(tid)
        {
            return false;
        }
        if let Some(mid) = &m.message_id {
            match msg.msg_id.as_deref() {
                Some(got) if got.trim().eq_ignore_ascii_case(mid.trim()) => {}
                _ => return false,
            }
        }
        if let Some(re) = &self.regex
            && !re.is_match(&msg.message)
        {
            return false;
        }
        true
    }
}

fn journald_field(msg: &SyslogMessage, field: &str) -> Option<String> {
    msg.structured_data
        .get("journald")
        .and_then(|m| m.get(field))
        .cloned()
}

/// The compiled, active ruleset behind the hot-swap lock.
struct Compiled {
    eval_interval: Duration,
    rules: Vec<CompiledRule>,
}

/// Compile a config into the active ruleset: user rules override built-ins by
/// id (a same-id user rule wins), drop invalid regexes and duplicate user ids
/// with a warning.
fn compile(cfg: &LogRulesConfig) -> Compiled {
    let mut seen = std::collections::HashSet::new();
    let mut rules = Vec::new();
    // User rules take precedence: a built-in whose id a user rule reuses is the
    // documented override path, so drop the built-in in favor of the user's.
    let user_ids: std::collections::HashSet<&str> =
        cfg.rules.iter().map(|r| r.id.as_str()).collect();
    let builtins = cfg
        .include_builtins
        .then(builtin_rules)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !user_ids.contains(r.id.as_str()));
    let all = builtins.chain(cfg.rules.iter().cloned());
    for rule in all {
        if !seen.insert(rule.id.clone()) {
            tracing::warn!(rule = %rule.id, "sentinel: duplicate rule id ignored");
            continue;
        }
        let regex = match &rule.matcher.pattern {
            Some(p) => match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!(rule = %rule.id, error = %e, "sentinel: bad regex, rule skipped");
                    continue;
                }
            },
            None => None,
        };
        rules.push(CompiledRule {
            rule,
            regex,
            hits: AtomicU64::new(0),
        });
    }
    Compiled {
        eval_interval: Duration::from_secs(cfg.eval_interval_secs.max(1)),
        rules,
    }
}

// ---- handle (hot-swap) ---------------------------------------------------

/// Cloneable handle to the live ruleset — the RPC write path calls
/// [`replace`](Self::replace); the read path calls [`snapshot`](Self::snapshot).
#[derive(Clone)]
pub struct SentinelHandle {
    compiled: Arc<RwLock<Compiled>>,
    /// Raw config kept verbatim for the read RPC (compiled form is lossy).
    config: Arc<RwLock<LogRulesConfig>>,
}

impl SentinelHandle {
    pub fn replace(&self, cfg: LogRulesConfig) {
        *self.compiled.write().unwrap() = compile(&cfg);
        *self.config.write().unwrap() = cfg;
        tracing::info!("sentinel: ruleset replaced");
    }

    /// Current ruleset + per-rule hit counters, for the read RPC.
    pub fn snapshot(&self) -> RulesStatus {
        let cfg = self.config.read().unwrap().clone();
        let hits = {
            let compiled = self.compiled.read().unwrap();
            compiled
                .rules
                .iter()
                .map(|r| (r.rule.id.clone(), r.hits.load(Ordering::Relaxed)))
                .collect()
        };
        RulesStatus { config: cfg, hits }
    }
}

/// Read-RPC reply: the active config plus per-rule lifetime hit counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesStatus {
    pub config: LogRulesConfig,
    pub hits: HashMap<String, u64>,
}

// ---- the sentinel --------------------------------------------------------

/// Per-rule mutable state, touched only by the intake task + reconcile tick.
#[derive(Default)]
struct State {
    /// rule id → recent match instants (for threshold windows).
    windows: HashMap<String, VecDeque<Instant>>,
    /// alert_key → (rule id, expiry) for dedup + reconcile.
    active: HashMap<String, (String, Instant)>,
}

/// Evaluates each intake line against the ruleset. The [`AlertReporter`] is
/// passed to the async methods rather than owned, so the pure matching engine
/// ([`evaluate`](Self::evaluate)) is unit-testable without a live session.
pub struct LogSentinel {
    host: String,
    handle: SentinelHandle,
    state: Mutex<State>,
}

impl LogSentinel {
    pub fn new(host: impl Into<String>, cfg: LogRulesConfig) -> Self {
        let handle = SentinelHandle {
            compiled: Arc::new(RwLock::new(compile(&cfg))),
            config: Arc::new(RwLock::new(cfg)),
        };
        Self {
            host: host.into(),
            handle,
            state: Mutex::new(State::default()),
        }
    }

    pub fn handle(&self) -> SentinelHandle {
        self.handle.clone()
    }

    /// Evaluate one line; fire any newly-triggered alerts. Called per record in
    /// the intake loop. Locks are held only to build the fire list (sync); the
    /// reporter is awaited afterward.
    pub async fn observe(&self, reporter: &AlertReporter, msg: &SyslogMessage, now: Instant) {
        let fire = self.evaluate(msg, now);
        for alert in fire {
            let key = alert.alert_key();
            if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                tracing::warn!(error = %e, alert = %key, "sentinel: failed to publish alert");
            }
        }
    }

    /// Pure(ish) match + threshold + dedup step: returns alerts to fire and
    /// updates window/active state. Split from I/O so it is unit-testable.
    fn evaluate(&self, msg: &SyslogMessage, now: Instant) -> Vec<Alert> {
        let compiled = self.handle.compiled.read().unwrap();
        let mut state = self.state.lock().unwrap();
        let mut out = Vec::new();

        for cr in &compiled.rules {
            if !cr.matches(msg) {
                continue;
            }
            cr.hits.fetch_add(1, Ordering::Relaxed);

            // Threshold gate: only fire once the window count crosses N.
            if let Some(th) = &cr.rule.threshold {
                let win = state.windows.entry(cr.rule.id.clone()).or_default();
                let horizon = now
                    .checked_sub(Duration::from_secs(th.within_secs.max(1)))
                    .unwrap_or(now);
                win.push_back(now);
                while win.front().is_some_and(|&t| t < horizon) {
                    win.pop_front();
                }
                if (win.len() as u64) < th.count.max(1) {
                    continue;
                }
            }

            let count = cr
                .rule
                .threshold
                .as_ref()
                .map(|_| {
                    state
                        .windows
                        .get(&cr.rule.id)
                        .map(|w| w.len() as u64)
                        .unwrap_or(1)
                })
                .unwrap_or(1);

            let alert = build_alert(&self.host, &cr.rule, msg, count);
            let key = alert.alert_key();
            let expiry = now + Duration::from_secs(cr.rule.for_secs.max(1));

            // Dedup: refresh the expiry, only emit on the leading edge so a hot
            // rule doesn't re-fire per line. Re-emitting is harmless (the
            // reporter debounces) but wasteful.
            let is_new = !matches!(state.active.get(&key), Some((_, exp)) if *exp > now);
            state
                .active
                .insert(key.clone(), (cr.rule.id.clone(), expiry));
            if is_new {
                out.push(alert);
            }
        }
        out
    }

    /// Reconcile expired alerts (resolve anything past its quiet period) and
    /// prune stale window entries. Runs on the eval-interval tick.
    pub async fn reconcile(&self, reporter: &AlertReporter, now: Instant) {
        // Group the still-active keys by rule id.
        let by_rule: HashMap<String, Vec<String>> = {
            let mut state = self.state.lock().unwrap();
            state.active.retain(|_, (_, exp)| *exp > now);
            let mut m: HashMap<String, Vec<String>> = HashMap::new();
            for (key, (rule, _)) in state.active.iter() {
                m.entry(rule.clone()).or_default().push(key.clone());
            }
            m
        };

        // Reconcile every rule that has ever been compiled, so a rule that just
        // went quiet (no active keys) gets its alerts resolved.
        let rule_ids: Vec<String> = {
            let compiled = self.handle.compiled.read().unwrap();
            compiled.rules.iter().map(|r| r.rule.id.clone()).collect()
        };
        for rule in rule_ids {
            let still = by_rule.get(&rule).cloned().unwrap_or_default();
            if let Err(e) = reporter.reconcile(&rule, &still).await {
                tracing::warn!(error = %e, rule = %rule, "sentinel: reconcile failed");
            }
        }
    }

    /// Run the reconcile tick until cancelled.
    pub async fn run_reconcile_loop(self: Arc<Self>, reporter: Arc<AlertReporter>) {
        let interval = self.handle.compiled.read().unwrap().eval_interval;
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            self.reconcile(&reporter, Instant::now()).await;
        }
    }
}

/// Truncate `s` to `max` chars with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Build the alert for a matched rule, substituting the summary template and
/// lifting the requested labels.
fn build_alert(host: &str, rule: &LogRule, msg: &SyslogMessage, count: u64) -> Alert {
    let unit = journald_field(msg, "unit");
    let app = msg.app_name.clone();

    // The count and sample line live in the *summary*, not in labels: the
    // reporter derives an alert's identity from its labels (`alert_key`), and a
    // per-line-varying count/sample would make every line look like a distinct
    // alert, defeating dedup + auto-resolve. Identity is (rule, unit, app,
    // message_id); the count/sample are the mutable payload.
    let summary = match &rule.summary {
        Some(tmpl) => render_summary(tmpl, rule, msg, count, unit.as_deref(), app.as_deref()),
        None if count > 1 => format!(
            "{} (repeated {count}×): {}",
            rule.id,
            truncate(&msg.message, default_summary_max())
        ),
        None => format!(
            "{}: {}",
            rule.id,
            truncate(&msg.message, default_summary_max())
        ),
    };

    let mut alert = Alert::new(
        host.to_string(),
        Protocol::Logs,
        AlertKind::Anomaly,
        rule.id.clone(),
        rule.severity,
        summary,
    )
    .with_label("rule", rule.id.clone());

    if let Some(u) = &unit {
        alert = alert.with_label("unit", u.clone());
    }
    if let Some(a) = &app {
        alert = alert.with_label("app", a.clone());
    }
    if let Some(mid) = &msg.msg_id {
        alert = alert.with_label("message_id", mid.trim().to_ascii_lowercase());
    }
    for field in &rule.labels_from {
        if let Some(v) = journald_field(msg, field) {
            alert = alert.with_label(field.clone(), v);
        }
    }
    alert
}

/// Substitute `{...}` placeholders in a summary template.
fn render_summary(
    tmpl: &str,
    rule: &LogRule,
    msg: &SyslogMessage,
    count: u64,
    unit: Option<&str>,
    app: Option<&str>,
) -> String {
    let caps = rule
        .matcher
        .pattern
        .as_ref()
        .and_then(|p| Regex::new(p).ok())
        .and_then(|re| {
            re.captures(&msg.message).map(|c| {
                (0..c.len())
                    .map(|i| c.get(i).map(|m| m.as_str().to_string()).unwrap_or_default())
                    .collect::<Vec<_>>()
            })
        });

    let mut out = String::with_capacity(tmpl.len());
    let mut chars = tmpl.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        for ch in chars.by_ref() {
            if ch == '}' {
                break;
            }
            name.push(ch);
        }
        match name.as_str() {
            "message" => out.push_str(&truncate(&msg.message, default_summary_max())),
            "count" => out.push_str(&count.to_string()),
            "unit" => out.push_str(unit.unwrap_or("")),
            "app" => out.push_str(app.unwrap_or("")),
            "host" => out.push_str(&msg.hostname.clone().unwrap_or_default()),
            "severity" => out.push_str(msg.severity.as_str()),
            n => {
                // Numeric → regex capture group.
                if let Ok(idx) = n.parse::<usize>() {
                    if let Some(g) = caps.as_ref().and_then(|c| c.get(idx)) {
                        out.push_str(g);
                    }
                } else {
                    // Unknown name — leave the placeholder literally.
                    out.push('{');
                    out.push_str(n);
                    out.push('}');
                }
            }
        }
    }
    out
}

/// Topic for the sentinel rules control surface (`@rpc/logs/rules` +
/// `rules/set`).
pub const RULES_TOPIC: &str = "rules";

/// Serve the sentinel rules control surface as `@rpc` procedures until the
/// session closes: `rules` (read snapshot + hit counters) + `rules/set`
/// (replace the ruleset; fleet-fanout allowed). Mirrors the sibling sentinels'
/// `expectations`/`expectations/set` (#543).
pub async fn serve_rules(session: Arc<zenoh::Session>, producer: String, handle: SentinelHandle) {
    use zensight_sensor_core::rpc::{self, RpcError};
    use zensight_sensor_core::v1::V1Context;

    let ctx = V1Context::for_producer(&zensight_common::PROFILE, &producer);
    let apply = handle.clone();
    let tasks = rpc::serve_topic::<LogRulesConfig, _, _, _, _>(
        session,
        &ctx,
        RULES_TOPIC,
        move |cfg| {
            let h = apply.clone();
            async move {
                h.replace(cfg);
                Ok(())
            }
        },
        move || {
            let h = handle.clone();
            async move {
                serde_json::to_vec(&h.snapshot())
                    .map_err(|e| RpcError::new("error/logs/serialize", e.to_string()))
            }
        },
    )
    .await;
    match tasks {
        Ok(tasks) => {
            for t in tasks {
                let _ = t.await;
            }
        }
        Err(e) => tracing::error!(error = %e, "sentinel: failed to serve rules @rpc"),
    }
}

/// The shipped built-in rules: the four journald known-events (#61) folded into
/// the sentinel so they share one mechanism and can be overridden by config.
pub fn builtin_rules() -> Vec<LogRule> {
    let ev = |id: &str, mid: &str, sev: AlertSeverity, labels_from: &[&str]| LogRule {
        id: id.to_string(),
        description: Some(format!("built-in journald known-event: {id}")),
        matcher: LogMatch {
            message_id: Some(mid.to_string()),
            ..Default::default()
        },
        threshold: None,
        severity: sev,
        summary: Some(format!("{id}: {{message}}")),
        labels_from: labels_from.iter().map(|s| s.to_string()).collect(),
        // Point events: brief incident, coalesce a burst, auto-resolve quickly.
        for_secs: 30,
    };
    vec![
        ev(
            "coredump",
            "fc2e22bc6ee647b6b90729ab34a250b1",
            AlertSeverity::Critical,
            &["coredump_exe", "coredump_signal", "coredump_pid"],
        ),
        ev(
            "unit-failed",
            "d9b373ed55a64feb8242e02dbe79a49c",
            AlertSeverity::Warning,
            &[],
        ),
        ev(
            "oomd-kill",
            "d989611b15e44c9dbf31e3c81256e4ed",
            AlertSeverity::Critical,
            &[],
        ),
        ev(
            "kernel-oom",
            "fe6faa94e7774663a0da52717891d8ef",
            AlertSeverity::Critical,
            &[],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn msg(line: &str) -> SyslogMessage {
        parse(line).unwrap()
    }

    fn rule(id: &str, matcher: LogMatch, threshold: Option<Threshold>) -> LogRule {
        LogRule {
            id: id.to_string(),
            description: None,
            matcher,
            threshold,
            severity: AlertSeverity::Warning,
            summary: None,
            labels_from: vec![],
            for_secs: default_for_secs(),
        }
    }

    #[test]
    fn regex_and_severity_match() {
        let cfg = LogRulesConfig {
            include_builtins: false,
            rules: vec![rule(
                "auth",
                LogMatch {
                    pattern: Some("Failed password".into()),
                    min_severity: Some(4),
                    ..Default::default()
                },
                None,
            )],
            ..Default::default()
        };
        let s = LogSentinel::new("h", cfg);
        let now = Instant::now();
        // <36> = facility 4 (auth), severity 4 (warning) → matches sev + regex.
        assert_eq!(
            s.evaluate(
                &msg("<36>Oct 11 00:00:00 h sshd: Failed password for root"),
                now
            )
            .len(),
            1
        );
        // Debug severity (7) fails min_severity=4.
        assert!(
            s.evaluate(
                &msg("<39>Oct 11 00:00:00 h sshd: Failed password for root"),
                now
            )
            .is_empty()
        );
        // Non-matching text.
        assert!(
            s.evaluate(&msg("<36>Oct 11 00:00:00 h sshd: Accepted password"), now)
                .is_empty()
        );
    }

    #[test]
    fn threshold_fires_only_after_count() {
        let cfg = LogRulesConfig {
            include_builtins: false,
            rules: vec![rule(
                "burst",
                LogMatch {
                    pattern: Some("Failed password".into()),
                    ..Default::default()
                },
                Some(Threshold {
                    count: 5,
                    within_secs: 60,
                }),
            )],
            ..Default::default()
        };
        let s = LogSentinel::new("h", cfg);
        let now = Instant::now();
        let line = msg("<36>Oct 11 00:00:00 h sshd: Failed password for root");
        // First four are below threshold.
        for _ in 0..4 {
            assert!(s.evaluate(&line, now).is_empty());
        }
        // The fifth crosses it → one alert whose summary carries the count.
        let fired = s.evaluate(&line, now);
        assert_eq!(fired.len(), 1);
        assert!(
            fired[0].summary.contains("repeated 5×"),
            "summary carries the count, got {:?}",
            fired[0].summary
        );
        // The sixth is deduped (same identity, already firing within for_secs).
        assert!(s.evaluate(&line, now).is_empty());
    }

    #[test]
    fn window_ages_out() {
        let cfg = LogRulesConfig {
            include_builtins: false,
            rules: vec![rule(
                "burst",
                LogMatch {
                    pattern: Some("x".into()),
                    ..Default::default()
                },
                Some(Threshold {
                    count: 3,
                    within_secs: 10,
                }),
            )],
            ..Default::default()
        };
        let s = LogSentinel::new("h", cfg);
        let t0 = Instant::now();
        let line = msg("<36>Oct 11 00:00:00 h app: x");
        // Two now, one 20s later: the first two have aged out of the 10s window,
        // so the count never reaches 3.
        assert!(s.evaluate(&line, t0).is_empty());
        assert!(s.evaluate(&line, t0).is_empty());
        assert!(
            s.evaluate(&line, t0 + Duration::from_secs(20)).is_empty(),
            "stale matches must not count toward the threshold"
        );
    }

    #[test]
    fn builtin_coredump_fires_with_labels() {
        let cfg = LogRulesConfig::default(); // builtins on
        let s = LogSentinel::new("h", cfg);
        let mut m = msg("<27>Oct 11 00:00:00 h systemd-coredump: Process dumped core");
        m.msg_id = Some("fc2e22bc6ee647b6b90729ab34a250b1".into());
        let mut jd = HashMap::new();
        jd.insert("coredump_exe".to_string(), "/usr/sbin/nginx".to_string());
        m.structured_data.insert("journald".to_string(), jd);

        let fired = s.evaluate(&m, Instant::now());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].severity, AlertSeverity::Critical);
        assert_eq!(fired[0].rule, "coredump");
        assert_eq!(
            fired[0].labels.get("coredump_exe").map(String::as_str),
            Some("/usr/sbin/nginx")
        );
    }

    #[test]
    fn summary_template_substitutes() {
        let mut r = rule(
            "auth",
            LogMatch {
                pattern: Some(r"Failed password for (\w+)".into()),
                ..Default::default()
            },
            None,
        );
        r.summary = Some("auth failure for {1} on {host}".into());
        let cfg = LogRulesConfig {
            include_builtins: false,
            rules: vec![r],
            ..Default::default()
        };
        let s = LogSentinel::new("h", cfg);
        let fired = s.evaluate(
            &msg("<36>Oct 11 00:00:00 web01 sshd: Failed password for admin"),
            Instant::now(),
        );
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].summary, "auth failure for admin on web01");
    }

    #[test]
    fn a_same_id_user_rule_overrides_a_builtin() {
        // A user rule reusing the "coredump" id must win over the built-in.
        let mut r = rule(
            "coredump",
            LogMatch {
                message_id: Some("fc2e22bc6ee647b6b90729ab34a250b1".into()),
                ..Default::default()
            },
            None,
        );
        r.severity = AlertSeverity::Info; // built-in is Critical
        let cfg = LogRulesConfig {
            include_builtins: true,
            rules: vec![r],
            ..Default::default()
        };
        let s = LogSentinel::new("h", cfg);
        let mut m = msg("<27>Oct 11 00:00:00 h systemd-coredump: dumped core");
        m.msg_id = Some("fc2e22bc6ee647b6b90729ab34a250b1".into());
        let fired = s.evaluate(&m, Instant::now());
        assert_eq!(fired.len(), 1, "exactly one rule fires (no duplicate)");
        assert_eq!(
            fired[0].severity,
            AlertSeverity::Info,
            "the user rule's severity wins over the built-in"
        );
    }

    #[test]
    fn hot_swap_via_handle_changes_matching() {
        // Start with builtins only (no user rule for "boom").
        let s = LogSentinel::new("h", LogRulesConfig::default());
        let line = msg("<36>Oct 11 00:00:00 h app: boom happened");
        assert!(
            s.evaluate(&line, Instant::now()).is_empty(),
            "no rule matches yet"
        );

        // Push a new ruleset live via the handle (the `rules/set` path).
        s.handle().replace(LogRulesConfig {
            include_builtins: false,
            rules: vec![rule(
                "boom",
                LogMatch {
                    pattern: Some("boom".into()),
                    ..Default::default()
                },
                None,
            )],
            ..Default::default()
        });

        let fired = s.evaluate(&line, Instant::now());
        assert_eq!(fired.len(), 1, "the pushed rule now matches");
        assert_eq!(fired[0].rule, "boom");

        // The read path reflects the swap + a hit counter.
        let status = s.handle().snapshot();
        assert_eq!(status.config.rules.len(), 1);
        assert_eq!(status.hits.get("boom").copied(), Some(1));
    }

    #[test]
    fn a_custom_message_id_rule_needs_no_code() {
        let cfg = LogRulesConfig {
            include_builtins: false,
            rules: vec![rule(
                "my-event",
                LogMatch {
                    message_id: Some("ABCDEF0123456789ABCDEF0123456789".into()),
                    ..Default::default()
                },
                None,
            )],
            ..Default::default()
        };
        let s = LogSentinel::new("h", cfg);
        let mut m = msg("<27>Oct 11 00:00:00 h app: something");
        // Case-insensitive MESSAGE_ID match.
        m.msg_id = Some("abcdef0123456789abcdef0123456789".into());
        assert_eq!(s.evaluate(&m, Instant::now()).len(), 1);
    }
}
