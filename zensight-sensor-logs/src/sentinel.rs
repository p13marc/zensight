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
    /// Include the built-in **kernel pattern** rules (#824): EXT4-fs error,
    /// md/RAID disk failure, block-device I/O error — the handful of lines
    /// that mean a machine is dying. **Off by default** (the quiet-alerts
    /// stance: silence unless asked), and pattern-based, so unlike
    /// `include_builtins` they work on any source, not just journald.
    #[serde(default)]
    pub include_kernel_builtins: bool,
    /// Operator-declared rules.
    #[serde(default)]
    pub rules: Vec<LogRule>,
}

impl Default for LogRulesConfig {
    fn default() -> Self {
        Self {
            eval_interval_secs: default_eval_interval(),
            include_builtins: true,
            include_kernel_builtins: false,
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
    ///
    /// **This is already the recovery window** (#932), which is why this
    /// sensor gained no `recover_after_secs` while netlink, hostspec and
    /// systemd did. A log rule has no "currently violated" state to debounce —
    /// a line either matched or it did not — so `for_secs` here means "must
    /// stay quiet this long", implemented in this module's own `active` map
    /// with an expiry sweep, and `observe` is called with `Some(Duration::ZERO)`
    /// precisely because the reporter's debounce is meaningless for it.
    ///
    /// A second hold stacked on top would be two timers meaning the same
    /// thing, with the alert clearing after the sum of them.
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    /// Cap on *fires* per window (#824): at most `max_fires` alert
    /// publications within `per_secs`, further fires suppressed (and counted)
    /// until the window frees. Distinct from `threshold`, which delays the
    /// first fire; this bounds how often a flapping rule can page. `None` =
    /// no cap.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

/// A `max_fires per per_secs` cap on alert publications for one rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_fires: u64,
    pub per_secs: u64,
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
    /// Fires eaten by the rule's `rate_limit` (#824) — surfaced in
    /// [`RulesStatus`] so a capped rule is visibly capped, never silently so.
    suppressed: AtomicU64,
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
        .chain(
            cfg.include_kernel_builtins
                .then(kernel_builtin_rules)
                .unwrap_or_default(),
        )
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
            suppressed: AtomicU64::new(0),
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
        let (hits, suppressed) = {
            let compiled = self.compiled.read().unwrap();
            let hits = compiled
                .rules
                .iter()
                .map(|r| (r.rule.id.clone(), r.hits.load(Ordering::Relaxed)))
                .collect();
            let suppressed = compiled
                .rules
                .iter()
                .filter(|r| r.suppressed.load(Ordering::Relaxed) > 0)
                .map(|r| (r.rule.id.clone(), r.suppressed.load(Ordering::Relaxed)))
                .collect();
            (hits, suppressed)
        };
        RulesStatus {
            config: cfg,
            hits,
            suppressed,
        }
    }
}

/// Read-RPC reply: the active config plus per-rule lifetime hit counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesStatus {
    pub config: LogRulesConfig,
    pub hits: HashMap<String, u64>,
    /// Fires eaten by each rule's `rate_limit` (#824); absent/empty when no
    /// rule has suppressed anything. Serde-defaulted: mixed-version fleets
    /// decode old payloads to empty, never to an error.
    #[serde(default)]
    pub suppressed: HashMap<String, u64>,
}

// ---- the sentinel --------------------------------------------------------

/// Per-rule mutable state, touched only by the intake task + reconcile tick.
#[derive(Default)]
struct State {
    /// rule id → recent match instants (for threshold windows).
    windows: HashMap<String, VecDeque<Instant>>,
    /// rule id → recent *fire* instants (for `rate_limit` windows, #824).
    fires: HashMap<String, VecDeque<Instant>>,
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

            // Rate limit (#824): cap *fires*, not matches — checked only on
            // the leading edge, before the active entry is written, so a
            // suppressed fire leaves no state behind and the next match after
            // the window frees fires normally. Suppressions are counted.
            if is_new && let Some(rl) = &cr.rule.rate_limit {
                let win = state.fires.entry(cr.rule.id.clone()).or_default();
                let horizon = now
                    .checked_sub(Duration::from_secs(rl.per_secs.max(1)))
                    .unwrap_or(now);
                while win.front().is_some_and(|&t| t < horizon) {
                    win.pop_front();
                }
                if win.len() as u64 >= rl.max_fires.max(1) {
                    cr.suppressed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                win.push_back(now);
            }

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
        // Each rule's rate-limit window length, for pruning quiet rules'
        // fire history (a rule that stops matching would otherwise keep its
        // window until the next match).
        let limits: HashMap<String, u64> = {
            let compiled = self.handle.compiled.read().unwrap();
            compiled
                .rules
                .iter()
                .filter_map(|r| {
                    r.rule
                        .rate_limit
                        .as_ref()
                        .map(|rl| (r.rule.id.clone(), rl.per_secs.max(1)))
                })
                .collect()
        };

        // Group the still-active keys by rule id.
        let by_rule: HashMap<String, Vec<String>> = {
            let mut state = self.state.lock().unwrap();
            state.active.retain(|_, (_, exp)| *exp > now);
            state.fires.retain(|rule, win| {
                // A rule without a limit any more (hot-swap) drops its window.
                let Some(&per_secs) = limits.get(rule) else {
                    return false;
                };
                win.retain(|&t| now.duration_since(t) < Duration::from_secs(per_secs));
                !win.is_empty()
            });
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

/// Redact secret-looking `key=value` assignments in a log line before it is
/// quoted into an alert (#824). Same denylist as the debug bundle
/// ([`zensight_sensor_core::is_secret_key`]) so what counts as a secret cannot
/// drift; same line-oriented shape as systemd's `redact_unit_file`. Returns
/// the text and whether anything was replaced — a redacted quote is flagged,
/// never passed off as verbatim.
fn redact_line(text: &str) -> (String, bool) {
    let mut redacted = false;
    let out = text
        .split(' ')
        .map(|tok| match tok.split_once('=') {
            Some((key, _))
                if !key.is_empty()
                    && zensight_sensor_core::is_secret_key(key.trim_matches('"'), &[]) =>
            {
                redacted = true;
                format!("{key}={}", zensight_sensor_core::REDACTED_MARKER)
            }
            _ => tok.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    (out, redacted)
}

/// Build the alert for a matched rule, substituting the summary template and
/// lifting the requested labels.
fn build_alert(host: &str, rule: &LogRule, msg: &SyslogMessage, count: u64) -> Alert {
    let unit = journald_field(msg, "unit");
    let app = msg.app_name.clone();

    // The line is redacted before any of it can reach a summary — matching
    // ran on the raw text above, but what leaves the host is scrubbed, and
    // capture groups substitute from the scrubbed copy (#824).
    let (safe_message, was_redacted) = redact_line(&msg.message);

    // The count and sample line live in the *summary*, not in labels: the
    // reporter derives an alert's identity from its labels (`alert_key`), and a
    // per-line-varying count/sample would make every line look like a distinct
    // alert, defeating dedup + auto-resolve. Identity is (rule, unit, app,
    // message_id); the count/sample are the mutable payload. (`redacted`
    // stays out of the labels for the same reason: a rule that sometimes
    // matches a secret must not split its alert key.)
    let mut summary = match &rule.summary {
        Some(tmpl) => render_summary(
            tmpl,
            rule,
            msg,
            &safe_message,
            count,
            unit.as_deref(),
            app.as_deref(),
        ),
        None if count > 1 => format!(
            "{} (repeated {count}×): {}",
            rule.id,
            truncate(&safe_message, default_summary_max())
        ),
        None => format!(
            "{}: {}",
            rule.id,
            truncate(&safe_message, default_summary_max())
        ),
    };
    if was_redacted {
        summary.push_str(" (redacted)");
    }

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

/// Substitute `{...}` placeholders in a summary template. `safe_message` is
/// the redacted copy of the line: both `{message}` and the regex capture
/// groups substitute from it, so a secret can reach a summary through
/// neither (#824).
fn render_summary(
    tmpl: &str,
    rule: &LogRule,
    msg: &SyslogMessage,
    safe_message: &str,
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
            re.captures(safe_message).map(|c| {
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
            "message" => out.push_str(&truncate(safe_message, default_summary_max())),
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

    let ctx = zensight_sensor_core::v1::for_producer(&producer);
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
        rate_limit: None,
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

/// The built-in **kernel pattern** rules (#824): the unambiguous lines that
/// mean storage is dying. Gated by `include_kernel_builtins` (off by default —
/// the quiet-alerts stance) and pattern-based rather than `MESSAGE_ID`-based,
/// so they catch the lines from any source: journald, a network syslog
/// stream, a tailed file. Overridable by a same-id user rule like every
/// built-in; a modest rate limit ships on each, because a dying disk can
/// print its last words thousands of times.
pub fn kernel_builtin_rules() -> Vec<LogRule> {
    let ev = |id: &str, pattern: &str, summary: &str| LogRule {
        id: id.to_string(),
        description: Some(format!("built-in kernel pattern: {id}")),
        matcher: LogMatch {
            pattern: Some(pattern.to_string()),
            ..Default::default()
        },
        threshold: None,
        severity: AlertSeverity::Critical,
        summary: Some(summary.to_string()),
        labels_from: vec![],
        for_secs: 600,
        rate_limit: Some(RateLimit {
            max_fires: 6,
            per_secs: 3600,
        }),
    };
    vec![
        ev(
            "ext4-fs-error",
            r"EXT4-fs error \(device ([^)]+)\)",
            "ext4-fs-error on {1}: {message}",
        ),
        ev(
            "xfs-corruption",
            r"XFS \(([^)]+)\): (Corruption|Metadata corruption|Internal error)",
            "xfs-corruption on {1}: {message}",
        ),
        ev(
            "md-raid-failure",
            r"md/raid[^:]*:[^:]*: Disk failure on (\S+)",
            "md-raid-failure: {message}",
        ),
        ev(
            "block-io-error",
            r"(?:blk_update_request: )?I/O error, dev (\S+)",
            "block-io-error on {1}: {message}",
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
            rate_limit: None,
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

    // ---- #824: rate limit, redaction, kernel built-ins ----

    #[test]
    fn rate_limit_caps_fires_and_frees_after_the_window() {
        let mut r = rule(
            "flappy",
            LogMatch {
                pattern: Some("boom".into()),
                ..Default::default()
            },
            None,
        );
        r.for_secs = 1; // expire fast so each evaluate is a fresh fire
        r.rate_limit = Some(RateLimit {
            max_fires: 2,
            per_secs: 60,
        });
        let s = LogSentinel::new(
            "h",
            LogRulesConfig {
                include_builtins: false,
                rules: vec![r],
                ..Default::default()
            },
        );
        let line = msg("<27>Oct 11 00:00:00 h app: boom");
        let t0 = Instant::now();
        assert_eq!(s.evaluate(&line, t0).len(), 1, "fire 1");
        assert_eq!(
            s.evaluate(&line, t0 + Duration::from_secs(2)).len(),
            1,
            "fire 2"
        );
        assert_eq!(
            s.evaluate(&line, t0 + Duration::from_secs(4)).len(),
            0,
            "fire 3 suppressed by the cap"
        );
        // The suppression is visible, not silent.
        assert_eq!(s.handle().snapshot().suppressed.get("flappy"), Some(&1));
        // Past the window, the rule fires again.
        assert_eq!(
            s.evaluate(&line, t0 + Duration::from_secs(70)).len(),
            1,
            "window freed"
        );
    }

    #[test]
    fn secrets_are_redacted_from_summaries_and_captures() {
        let (safe, redacted) = redact_line("connect failed password=hunter2 host=db1");
        assert!(redacted);
        assert!(!safe.contains("hunter2"), "{safe}");
        assert!(safe.contains("host=db1"), "non-secrets survive: {safe}");

        let mut r = rule(
            "db",
            LogMatch {
                pattern: Some(r"password=(\S+)".into()),
                ..Default::default()
            },
            None,
        );
        // A template that quotes both the line and the capture group.
        r.summary = Some("db: {message} [{1}]".into());
        let s = LogSentinel::new(
            "h",
            LogRulesConfig {
                include_builtins: false,
                rules: vec![r],
                ..Default::default()
            },
        );
        let fired = s.evaluate(
            &msg("<27>Oct 11 00:00:00 h app: connect failed password=hunter2 host=db1"),
            Instant::now(),
        );
        assert_eq!(fired.len(), 1, "matching ran on the raw line");
        assert!(
            !fired[0].summary.contains("hunter2"),
            "neither {{message}} nor a capture may leak the secret: {}",
            fired[0].summary
        );
        assert!(
            fired[0].summary.ends_with("(redacted)"),
            "a scrubbed quote is flagged, never passed off as verbatim: {}",
            fired[0].summary
        );
    }

    #[test]
    fn kernel_builtins_are_opt_in_and_match_the_motivating_lines() {
        let lines = [
            "<2>Oct 11 00:00:00 h kernel: EXT4-fs error (device sda1): ext4_find_entry:1455: inode #2: comm cron: reading directory lblock 0",
            "<2>Oct 11 00:00:00 h kernel: md/raid1:md0: Disk failure on nvme1n1p2, disabling device.",
            "<2>Oct 11 00:00:00 h kernel: blk_update_request: I/O error, dev nvme0n1, sector 123456",
            "<2>Oct 11 00:00:00 h kernel: XFS (dm-3): Metadata corruption detected at xfs_inode_buf_verify",
        ];

        // Off by default: the quiet-alerts stance.
        let quiet = LogSentinel::new(
            "h",
            LogRulesConfig {
                include_builtins: false,
                ..Default::default()
            },
        );
        for line in &lines {
            assert!(quiet.evaluate(&msg(line), Instant::now()).is_empty());
        }

        // Opted in: each motivating line is a Critical, its device named.
        let s = LogSentinel::new(
            "h",
            LogRulesConfig {
                include_builtins: false,
                include_kernel_builtins: true,
                ..Default::default()
            },
        );
        let mut rules_fired = Vec::new();
        for line in &lines {
            let fired = s.evaluate(&msg(line), Instant::now());
            assert_eq!(fired.len(), 1, "{line}");
            assert_eq!(fired[0].severity, AlertSeverity::Critical);
            rules_fired.push(fired[0].rule.clone());
        }
        assert_eq!(
            rules_fired,
            [
                "ext4-fs-error",
                "md-raid-failure",
                "block-io-error",
                "xfs-corruption"
            ]
        );
        // A user rule with the same id still overrides a kernel built-in.
        let overridden = LogSentinel::new(
            "h",
            LogRulesConfig {
                include_builtins: false,
                include_kernel_builtins: true,
                rules: vec![rule(
                    "ext4-fs-error",
                    LogMatch {
                        pattern: Some("never-matches".into()),
                        ..Default::default()
                    },
                    None,
                )],
                ..Default::default()
            },
        );
        assert!(
            overridden
                .evaluate(&msg(lines[0]), Instant::now())
                .is_empty()
        );
    }
}
