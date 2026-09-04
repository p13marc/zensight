//! Expectation engine (Pillar B): declare what the machine *should* look like,
//! evaluate it against observed kernel state, and emit alerts on deviation.
//!
//! Embedded in the netlink sensor (it needs the same netlink access). The check
//! logic is pure and unit-tested; the [`Evaluator`] wires it to live nlink
//! connections + an [`AlertReporter`].

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, RwLock};
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

use nlink::netlink::{Connection, Route, SockDiag};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};

use crate::collector::MetricCache;

// The expectation vocabulary moved to zensight-common in #849 so it could get
// a real schemars schema and join `@desired` (RFC 08 §7's gate refuses a
// summary stub, and this crate can never supply more than one). The checking
// logic below is unchanged and still owns the observation types.
//
// `ExpectationsConfig` is re-exported under its old name inside this module:
// the sensor's own config file, its `@rpc` handler and its tests all spell it
// that way, and renaming it here as well as on the wire would put two
// unrelated changes in one diff.
pub use zensight_common::netlink::{
    DeliveryFloorExpectation, LinkExpectation, MetricExpectation, NeighborExpectation,
    NetlinkExpectations as ExpectationsConfig, RateExpectation, RouteExpectation,
    RouteFlapExpectation, RuleExpectation, RuleSense, SocketExpectation,
};

/// A consecutive pair of samples for a rate-of-change check, plus the wall-clock
/// interval between them. Built by the [`Evaluator`] from its retained previous
/// sample; consumed by the pure [`check_rate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateSample {
    /// Value observed on this sweep.
    pub current: f64,
    /// Value observed on the previous sweep.
    pub previous: f64,
    /// Seconds elapsed between the two samples.
    pub interval_secs: f64,
}

/// One observed policy-routing rule, reduced to the facts the checks match on.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleFact {
    pub priority: u32,
    pub table: u32,
    /// The rule's action name (`"lookup"` / `"blackhole"` / …).
    pub action: String,
    /// A kernel baseline lookup rule (priority 0 / 32766 / 32767).
    pub is_default: bool,
}

/// Observed policy-routing rules (#323).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleObservation {
    pub rules: Vec<RuleFact>,
}

/// Observed default-route facts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteObservation {
    pub default_present: bool,
    pub default_gw: Option<String>,
}

/// A single currently-violated fact.
///
/// **`labels` must not carry a per-sweep measurement** (#932). Every non-`host.*`
/// label is hashed into `Alert::alert_key`, so a label that changes each sweep
/// mints a new alert key each sweep: `first_seen` resets, the entry is dropped
/// unpublished by the next `reconcile`, and a `for_secs` longer than the sweep
/// interval **can never elapse** — the rule silently never fires. It also
/// leaves a Put/Delete pair on the bus per sample, and, since #929's recovery
/// hold retains published entries, grows `active` without bound.
///
/// `check_metric`, `check_rate`, `check_delivery_floor`, `check_route_flap` and
/// `check_socket` all did this until #932. `AlertReporter::retire`'s own doc
/// records the same bug found twice before, in probe's `duration_ms` and
/// systemd's `overdue_secs`.
///
/// The measured value belongs in [`summary`](Self::summary), which every one of
/// them already puts it in, and which is *not* part of the key.
///
/// A **categorical** label is fine and is the point of the field: `up`/`down`,
/// `absent`, a peer address, a gateway. Those identify *which* thing is wrong,
/// which is exactly what should fork an alert key.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub summary: String,
    pub labels: Vec<(String, String)>,
}

// ---- Observed state (built from nlink; pure checks operate on these) --------

/// Observed socket facts for evaluating socket expectations.
#[derive(Debug, Clone, Default)]
pub struct SocketObservation {
    pub listening_ports: HashSet<u16>,
    pub established_remotes: Vec<SocketAddr>,
}

// ---- Pure checks ------------------------------------------------------------

/// Evaluate a socket expectation against observed state.
/// Refuse an expectation set that could not be reported on (#849).
///
/// Every expectation's `name` becomes the second half of its alert rule slug
/// (`sockets:<name>`, `rules:<name>`), which is hashed into the `alert_key`
/// (RFC 11 §3.1). Two expectations sharing a name within a family therefore
/// share one alert key: they fire and resolve over each other, and an
/// operator sees one alert flapping instead of two conditions. An empty name
/// produces the bare slug `sockets:` — every unnamed expectation in that
/// family collapsing into one.
///
/// Neither is caught anywhere else, and neither is visible in the output: the
/// set applies, the sweep runs, and the alerts are simply wrong. So the
/// `@desired` path refuses the whole document, which keeps the previous good
/// set running and puts the reason on the `applied/<topic>` marker.
pub fn validate(cfg: &ExpectationsConfig) -> Result<(), String> {
    fn family<'a>(kind: &str, names: impl Iterator<Item = &'a str>) -> Result<(), String> {
        let mut seen = HashSet::new();
        for name in names {
            if name.trim().is_empty() {
                return Err(format!("{kind}: an expectation has an empty name"));
            }
            if !seen.insert(name) {
                return Err(format!("{kind}: duplicate expectation name {name:?}"));
            }
        }
        Ok(())
    }
    family("sockets", cfg.sockets.iter().map(|e| e.name.as_str()))?;
    // links and neighbors are keyed by the thing they watch, not by a label:
    // their rule slugs are `links:<iface>` and `neighbors:<ip>`. Same
    // collision, different field.
    family("links", cfg.links.iter().map(|e| e.iface.as_str()))?;
    family("neighbors", cfg.neighbors.iter().map(|e| e.ip.as_str()))?;
    family("routes", cfg.routes.iter().map(|e| e.name.as_str()))?;
    family("metrics", cfg.metrics.iter().map(|e| e.name.as_str()))?;
    family("rates", cfg.rates.iter().map(|e| e.name.as_str()))?;
    family("delivery", cfg.delivery.iter().map(|e| e.name.as_str()))?;
    family(
        "route_flaps",
        cfg.route_flaps.iter().map(|e| e.name.as_str()),
    )?;
    family("rules", cfg.rules.iter().map(|e| e.name.as_str()))?;
    if cfg.eval_interval_secs == 0 {
        return Err("eval_interval_secs must be > 0 — a zero interval is a spin".to_string());
    }
    Ok(())
}

pub fn check_socket(exp: &SocketExpectation, obs: &SocketObservation) -> Vec<Violation> {
    let mut v = Vec::new();
    if let Some(port) = exp.listen
        && !obs.listening_ports.contains(&port)
    {
        v.push(Violation {
            summary: format!("{} not listening on :{}", exp.name, port),
            labels: vec![
                ("expected".into(), "listen".into()),
                ("port".into(), port.to_string()),
            ],
        });
    }
    if let Some(port) = exp.forbid_listen
        && obs.listening_ports.contains(&port)
    {
        v.push(Violation {
            summary: format!("unexpected listener on :{} ({})", port, exp.name),
            labels: vec![
                ("expected".into(), "no-listen".into()),
                ("port".into(), port.to_string()),
            ],
        });
    }
    if let Some(target) = &exp.established_to {
        let want: Option<SocketAddr> = target.parse().ok();
        let count = match want {
            Some(addr) => obs
                .established_remotes
                .iter()
                .filter(|r| **r == addr)
                .count(),
            None => 0,
        };
        if count < exp.min {
            v.push(Violation {
                summary: format!(
                    "{}/{} expected established connections to {}",
                    count, exp.min, target
                ),
                labels: vec![
                    ("expected".into(), format!("established>={}", exp.min)),
                    ("peer".into(), target.clone()),
                ],
            });
        }
    }
    v
}

/// Evaluate a link expectation against an interface's observed up-state.
/// `observed_up` is `None` when the interface is absent.
pub fn check_link(exp: &LinkExpectation, observed_up: Option<bool>) -> Vec<Violation> {
    match observed_up {
        None => vec![Violation {
            summary: format!("interface {} not found (expected present)", exp.iface),
            labels: vec![("expected".into(), "present".into())],
        }],
        Some(up) if up != exp.up => vec![Violation {
            summary: format!(
                "{} is {} (expected {})",
                exp.iface,
                if up { "up" } else { "down" },
                if exp.up { "up" } else { "down" }
            ),
            labels: vec![
                ("expected".into(), if exp.up { "up" } else { "down" }.into()),
                ("actual".into(), if up { "up" } else { "down" }.into()),
            ],
        }],
        _ => Vec::new(),
    }
}

/// Evaluate a neighbor expectation. `observed_reachable` is `None` when the IP is
/// absent from the neighbor table.
pub fn check_neighbor(
    exp: &NeighborExpectation,
    observed_reachable: Option<bool>,
) -> Vec<Violation> {
    match observed_reachable {
        None if exp.reachable => vec![Violation {
            summary: format!("neighbor {} not found in ARP/NDP table", exp.ip),
            labels: vec![
                ("expected".into(), "reachable".into()),
                ("ip".into(), exp.ip.clone()),
                ("actual".into(), "absent".into()),
            ],
        }],
        Some(reachable) if reachable != exp.reachable => vec![Violation {
            summary: format!(
                "neighbor {} is {} (expected {})",
                exp.ip,
                if reachable {
                    "reachable"
                } else {
                    "unreachable"
                },
                if exp.reachable {
                    "reachable"
                } else {
                    "unreachable"
                }
            ),
            labels: vec![
                (
                    "expected".into(),
                    if exp.reachable {
                        "reachable"
                    } else {
                        "unreachable"
                    }
                    .into(),
                ),
                ("ip".into(), exp.ip.clone()),
                (
                    "actual".into(),
                    if reachable {
                        "reachable"
                    } else {
                        "unreachable"
                    }
                    .into(),
                ),
            ],
        }],
        _ => Vec::new(),
    }
}

/// Evaluate a default-route expectation against observed routing state.
pub fn check_route(exp: &RouteExpectation, obs: &RouteObservation) -> Vec<Violation> {
    if exp.default_present && !obs.default_present {
        return vec![Violation {
            summary: format!("{}: no default route present", exp.name),
            labels: vec![("expected".into(), "default-route".into())],
        }];
    }
    if let Some(want_gw) = &exp.default_via
        && obs.default_present
        && obs.default_gw.as_deref() != Some(want_gw.as_str())
    {
        return vec![Violation {
            summary: format!(
                "{}: default gateway is {} (expected {})",
                exp.name,
                obs.default_gw.as_deref().unwrap_or("none"),
                want_gw
            ),
            labels: vec![
                ("expected".into(), format!("via {want_gw}")),
                (
                    "actual".into(),
                    obs.default_gw.clone().unwrap_or_else(|| "none".into()),
                ),
            ],
        }];
    }
    Vec::new()
}

/// Evaluate a policy-rule expectation against the observed rule set (#323).
/// Violations carry the MITRE ATT&CK technique **T1599** (Network Boundary
/// Bridging) — a policy-rule diversion moves traffic across a boundary the
/// analyst believes is enforced.
pub fn check_rules(exp: &RuleExpectation, obs: &RuleObservation) -> Vec<Violation> {
    let matches = |r: &&RuleFact| {
        exp.priority.is_none_or(|p| r.priority == p) && exp.table.is_none_or(|t| r.table == t)
    };
    match exp.sense {
        RuleSense::Forbid => obs
            .rules
            .iter()
            .filter(|r| !r.is_default)
            .filter(matches)
            .map(|r| Violation {
                summary: format!(
                    "{}: forbidden policy rule present (prio {} → {} table {})",
                    exp.name, r.priority, r.action, r.table
                ),
                labels: vec![
                    ("expected".into(), "no-policy-rule".into()),
                    ("priority".into(), r.priority.to_string()),
                    ("table".into(), r.table.to_string()),
                    ("action".into(), r.action.clone()),
                    ("technique".into(), "T1599".into()),
                ],
            })
            .collect(),
        RuleSense::Require => {
            if obs.rules.iter().any(|r| matches(&r)) {
                Vec::new()
            } else {
                let want = match (exp.priority, exp.table) {
                    (Some(p), Some(t)) => format!("prio {p} table {t}"),
                    (Some(p), None) => format!("prio {p}"),
                    (None, Some(t)) => format!("table {t}"),
                    (None, None) => "any".to_string(),
                };
                vec![Violation {
                    summary: format!("{}: required policy rule missing ({want})", exp.name),
                    labels: vec![
                        ("expected".into(), format!("policy-rule {want}")),
                        ("actual".into(), "absent".into()),
                        ("technique".into(), "T1599".into()),
                    ],
                }]
            }
        }
    }
}

/// Evaluate a metric-threshold expectation. `observed` is the metric's latest
/// value (`None` if not yet published). Absent → no violation (matches the GUI
/// threshold-rule semantics: a rule only fires on data it has actually seen).
pub fn check_metric(exp: &MetricExpectation, observed: Option<f64>) -> Vec<Violation> {
    match observed {
        Some(v) if !exp.op.evaluate(v, exp.value) => vec![Violation {
            summary: format!(
                "{}: {} is {} (expected {} {})",
                exp.name,
                exp.metric,
                v,
                exp.op.symbol(),
                exp.value
            ),
            labels: vec![
                (
                    "expected".into(),
                    format!("{} {} {}", exp.metric, exp.op.symbol(), exp.value),
                ),
                ("metric".into(), exp.metric.clone()),
            ],
        }],
        _ => Vec::new(),
    }
}

/// Evaluate a rate-of-change expectation. `sample` is the current+previous
/// values and the interval between them (`None` on the first sweep for a rule, or
/// while the metric has never been seen). Only *positive* deltas count: a counter
/// reset/wrap (negative delta) or a zero interval is treated as no violation.
pub fn check_rate(exp: &RateExpectation, sample: Option<RateSample>) -> Vec<Violation> {
    let Some(s) = sample else { return Vec::new() };
    let delta = s.current - s.previous;
    if delta <= 0.0 || s.interval_secs <= 0.0 {
        return Vec::new();
    }
    let per_min = delta / (s.interval_secs / 60.0);
    if per_min > exp.max_increase_per_min {
        vec![Violation {
            summary: format!(
                "{}: {} increasing at {:.1}/min (limit {}/min)",
                exp.name, exp.metric, per_min, exp.max_increase_per_min
            ),
            labels: vec![
                (
                    "expected".into(),
                    format!("{} rate <= {}/min", exp.metric, exp.max_increase_per_min),
                ),
                ("metric".into(), exp.metric.clone()),
            ],
        }]
    } else {
        Vec::new()
    }
}

/// Evaluate a delivery-rate floor expectation. `observed` is the metric's latest
/// value (`None` if not yet published). Fires strictly below the floor; absent →
/// no violation (matches the metric-rule semantics: only fires on data seen).
pub fn check_delivery_floor(
    exp: &DeliveryFloorExpectation,
    observed: Option<f64>,
) -> Vec<Violation> {
    match observed {
        Some(v) if v < exp.floor => vec![Violation {
            summary: format!(
                "{}: {} is {} (below floor {})",
                exp.name, exp.metric, v, exp.floor
            ),
            labels: vec![
                (
                    "expected".into(),
                    format!("{} >= {}", exp.metric, exp.floor),
                ),
                ("metric".into(), exp.metric.clone()),
            ],
        }],
        _ => Vec::new(),
    }
}

/// Increase of a cumulative flap counter within the trailing `window_secs`:
/// `current - counter-as-of-window-start`. `samples` are `(ts_secs, counter)`
/// pairs, oldest first. The baseline is the newest sample at/just before the
/// window cutoff (so flaps strictly inside the window are counted), falling back
/// to the oldest retained sample. Pure; unit-tested.
pub fn flaps_within(samples: &[(u64, u64)], now_secs: u64, window_secs: u64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let cutoff = now_secs.saturating_sub(window_secs);
    let baseline = samples
        .iter()
        .rev()
        .find(|(t, _)| *t <= cutoff)
        .or_else(|| samples.first())
        .map(|(_, c)| *c)
        .unwrap_or(0);
    let current = samples.last().map(|(_, c)| *c).unwrap_or(baseline);
    current.saturating_sub(baseline)
}

/// Evaluate a route-flap expectation against the flap count observed within the
/// window. Fires when the count exceeds `max_flaps`.
pub fn check_route_flap(exp: &RouteFlapExpectation, flaps_in_window: u64) -> Vec<Violation> {
    if flaps_in_window > exp.max_flaps {
        vec![Violation {
            summary: format!(
                "{}: default route flapped {} times in {}s (limit {})",
                exp.name, flaps_in_window, exp.window_secs, exp.max_flaps
            ),
            labels: vec![
                (
                    "expected".into(),
                    format!("flaps <= {} per {}s", exp.max_flaps, exp.window_secs),
                ),
                ("metric".into(), exp.metric.clone()),
            ],
        }]
    } else {
        Vec::new()
    }
}

// ---- Evaluator (live nlink + AlertReporter) ---------------------------------

/// Shared, hot-swappable expectation set. Cloning is cheap (Arc).
#[derive(Clone)]
pub struct SentinelHandle {
    expectations: Arc<RwLock<ExpectationsConfig>>,
}

impl SentinelHandle {
    /// Build a standalone handle over an expectation set (useful for tests and
    /// for sharing a set the [`Evaluator`] also reads).
    pub fn new(cfg: ExpectationsConfig) -> Self {
        Self {
            expectations: Arc::new(RwLock::new(cfg)),
        }
    }

    /// Replace the entire live expectation set.
    pub async fn replace(&self, cfg: ExpectationsConfig) {
        warn_metrics_deprecated(&cfg);
        *self.expectations.write().await = cfg;
    }
    /// Add (or replace by name) a socket expectation.
    pub async fn add_socket(&self, exp: SocketExpectation) {
        let mut c = self.expectations.write().await;
        c.sockets.retain(|e| e.name != exp.name);
        c.sockets.push(exp);
    }
    /// Add (or replace by iface) a link expectation.
    pub async fn add_link(&self, exp: LinkExpectation) {
        let mut c = self.expectations.write().await;
        c.links.retain(|e| e.iface != exp.iface);
        c.links.push(exp);
    }
    /// Add (or replace by ip) a neighbor expectation.
    pub async fn add_neighbor(&self, exp: NeighborExpectation) {
        let mut c = self.expectations.write().await;
        c.neighbors.retain(|e| e.ip != exp.ip);
        c.neighbors.push(exp);
    }
    /// Add (or replace by name) a route expectation.
    pub async fn add_route(&self, exp: RouteExpectation) {
        let mut c = self.expectations.write().await;
        c.routes.retain(|e| e.name != exp.name);
        c.routes.push(exp);
    }
    /// Add (or replace by name) a metric-threshold expectation.
    pub async fn add_metric(&self, exp: MetricExpectation) {
        let mut c = self.expectations.write().await;
        c.metrics.retain(|e| e.name != exp.name);
        c.metrics.push(exp);
    }
    /// Add (or replace by name) a rate-of-change expectation.
    pub async fn add_rate(&self, exp: RateExpectation) {
        let mut c = self.expectations.write().await;
        c.rates.retain(|e| e.name != exp.name);
        c.rates.push(exp);
    }
    /// Add (or replace by name) a delivery-rate floor expectation.
    pub async fn add_delivery(&self, exp: DeliveryFloorExpectation) {
        let mut c = self.expectations.write().await;
        c.delivery.retain(|e| e.name != exp.name);
        c.delivery.push(exp);
    }
    /// Add (or replace by name) a route-flap expectation.
    pub async fn add_route_flap(&self, exp: RouteFlapExpectation) {
        let mut c = self.expectations.write().await;
        c.route_flaps.retain(|e| e.name != exp.name);
        c.route_flaps.push(exp);
    }
    /// Add (or replace by name) a policy-rule expectation (#323).
    pub async fn add_rule(&self, exp: RuleExpectation) {
        let mut c = self.expectations.write().await;
        c.rules.retain(|e| e.name != exp.name);
        c.rules.push(exp);
    }
    /// Remove an expectation by rule slug (`socket:<name>` / `link:<iface>` /
    /// `neighbor:<ip>` / `route:<name>` / `metric:<name>` / `rate:<name>` /
    /// `delivery:<name>` / `route_flap:<name>` / `rules:<name>`).
    pub async fn remove(&self, rule: &str) {
        let mut c = self.expectations.write().await;
        if let Some(name) = rule.strip_prefix("socket:") {
            c.sockets.retain(|e| e.name != name);
        } else if let Some(iface) = rule.strip_prefix("link:") {
            c.links.retain(|e| e.iface != iface);
        } else if let Some(ip) = rule.strip_prefix("neighbor:") {
            c.neighbors.retain(|e| e.ip != ip);
        } else if let Some(name) = rule.strip_prefix("route_flap:") {
            c.route_flaps.retain(|e| e.name != name);
        } else if let Some(name) = rule.strip_prefix("route:") {
            c.routes.retain(|e| e.name != name);
        } else if let Some(name) = rule.strip_prefix("metric:") {
            c.metrics.retain(|e| e.name != name);
        } else if let Some(name) = rule.strip_prefix("rate:") {
            c.rates.retain(|e| e.name != name);
        } else if let Some(name) = rule.strip_prefix("delivery:") {
            c.delivery.retain(|e| e.name != name);
        // `rules:` (#323) is checked after the longer `route*:` prefixes so
        // slug matching stays unambiguous (no shared prefix, but keep the
        // established longest-first discipline).
        } else if let Some(name) = rule.strip_prefix("rules:") {
            c.rules.retain(|e| e.name != name);
        }
    }
    /// Snapshot the current expectation set (for the status queryable).
    pub async fn snapshot(&self) -> ExpectationsConfig {
        self.expectations.read().await.clone()
    }
}

/// Runs expectation sweeps on a cadence and feeds an [`AlertReporter`].
pub struct Evaluator {
    host: String,
    expectations: Arc<RwLock<ExpectationsConfig>>,
    reporter: Arc<AlertReporter>,
    /// Latest published metric values, for metric-threshold expectations.
    metric_cache: MetricCache,
    /// Rules evaluated on the previous sweep — used to resolve alerts for rules
    /// that were removed (hot-swap) so they don't linger forever.
    seen_rules: std::sync::Mutex<HashSet<String>>,
    /// Previous `(value, instant)` per rate-of-change rule (keyed by rule name).
    /// The rate is computed between consecutive sweeps from this retained sample
    /// (#113); kept here rather than in the [`MetricCache`] so the cache stays a
    /// plain latest-value store and the rate reflects the sweep cadence.
    rate_state: std::sync::Mutex<HashMap<String, (f64, Instant)>>,
    /// Sliding window of `(ts_secs, counter)` samples per route-flap rule (keyed
    /// by rule name), used to count flaps within the rule's window (#113).
    flap_state: std::sync::Mutex<HashMap<String, Vec<(u64, u64)>>>,
    /// Monotonic base for stamping flap samples in whole seconds.
    flap_base: Instant,
    /// Nudged by the real-time event task on a relevant transition (#8); the
    /// sweep loop wakes immediately instead of waiting for the next tick.
    wake: Option<Arc<Notify>>,
}

/// Say it out loud, once, when a set that uses `metrics` is loaded (#932).
///
/// A deprecation that only exists in a doc comment reaches nobody who is
/// running the thing; the operator with a live `metrics` block never opens
/// rustdoc. It fires on the `@desired` and `@rpc` paths too, because
/// `SentinelHandle::replace` is how a set arrives on a running sensor and a
/// pushed set is exactly the one an operator is still editing.
pub fn warn_metrics_deprecated(config: &ExpectationsConfig) {
    if config.metrics.is_empty() {
        return;
    }
    tracing::warn!(
        count = config.metrics.len(),
        rules = %config.metrics.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "),
        "netlink.expectations.metrics is DEPRECATED (#932) and is removed one \
         release after 0.13. Move these to `thresholds.rules`, which netlink \
         evaluates on its own publish path since #931: same metric, the \
         comparison the other way round (a threshold states the FIRING \
         condition), and a `clear` for value hysteresis. Unlike this block, a \
         threshold rule is a real schema, is authorable fleet-wide on \
         @desired, and is the same vocabulary on every sensor"
    );
}

impl Evaluator {
    pub fn new(
        host: String,
        config: ExpectationsConfig,
        reporter: Arc<AlertReporter>,
        metric_cache: MetricCache,
    ) -> Self {
        warn_metrics_deprecated(&config);
        Self {
            host,
            expectations: Arc::new(RwLock::new(config)),
            reporter,
            metric_cache,
            seen_rules: std::sync::Mutex::new(HashSet::new()),
            rate_state: std::sync::Mutex::new(HashMap::new()),
            flap_state: std::sync::Mutex::new(HashMap::new()),
            flap_base: Instant::now(),
            wake: None,
        }
    }

    /// Wire a real-time wake signal so a relevant RTNETLINK event (#8) triggers an
    /// immediate sweep (~0s latency) on top of the periodic cadence.
    pub fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// A cloneable handle to mutate the live expectation set (for the command
    /// channel / GUI authoring).
    pub fn handle(&self) -> SentinelHandle {
        SentinelHandle {
            expectations: self.expectations.clone(),
        }
    }

    pub async fn run(self) {
        let route = Connection::<Route>::new().ok();
        let sockdiag = Connection::<SockDiag>::new().ok();
        if route.is_none() {
            tracing::error!("sentinel: cannot open route connection; link expectations disabled");
        }
        if sockdiag.is_none() {
            tracing::error!("sentinel: cannot open sockdiag; socket expectations disabled");
        }

        loop {
            let interval = self.expectations.read().await.eval_interval_secs.max(1);
            // Sweep on the periodic tick OR immediately when an event nudges us.
            // Without a wake signal this degrades to a plain interval sweep.
            match &self.wake {
                Some(wake) => {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
                        _ = wake.notified() => {
                            tracing::debug!("sentinel: woken by RTNETLINK event");
                        }
                    }
                }
                None => tokio::time::sleep(Duration::from_secs(interval)).await,
            }
            self.sweep(route.as_ref(), sockdiag.as_ref()).await;
        }
    }

    async fn sweep(
        &self,
        route: Option<&Connection<Route>>,
        sockdiag: Option<&Connection<SockDiag>>,
    ) {
        let config = self.expectations.read().await.clone();
        let mut current_rules: HashSet<String> = HashSet::new();

        // Socket expectations.
        if !config.sockets.is_empty()
            && let Some(sd) = sockdiag
        {
            match observe_sockets(sd).await {
                Ok(obs) => {
                    for exp in &config.sockets {
                        let rule = format!("socket:{}", exp.name);
                        current_rules.insert(rule.clone());
                        let violations = check_socket(exp, &obs);
                        self.report(
                            &rule,
                            exp.severity,
                            exp.for_secs.or(Some(config.default_for_secs)),
                            exp.recover_after_secs
                                .or(Some(config.default_recover_after_secs)),
                            violations,
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sentinel: socket observation failed"),
            }
        }

        // Link expectations.
        if !config.links.is_empty()
            && let Some(rt) = route
        {
            match observe_links(rt).await {
                Ok(links) => {
                    for exp in &config.links {
                        let rule = format!("link:{}", exp.iface);
                        current_rules.insert(rule.clone());
                        let observed = links.iter().find(|(n, _)| n == &exp.iface).map(|(_, u)| *u);
                        let violations = check_link(exp, observed);
                        self.report(
                            &rule,
                            exp.severity,
                            exp.for_secs.or(Some(config.default_for_secs)),
                            exp.recover_after_secs
                                .or(Some(config.default_recover_after_secs)),
                            violations,
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sentinel: link observation failed"),
            }
        }

        // Neighbor (gateway/peer reachability) expectations.
        if !config.neighbors.is_empty()
            && let Some(rt) = route
        {
            match observe_neighbors(rt).await {
                Ok(neighbors) => {
                    for exp in &config.neighbors {
                        let rule = format!("neighbor:{}", exp.ip);
                        current_rules.insert(rule.clone());
                        let observed = neighbors
                            .iter()
                            .find(|(ip, _)| ip == &exp.ip)
                            .map(|(_, r)| *r);
                        let violations = check_neighbor(exp, observed);
                        self.report(
                            &rule,
                            exp.severity,
                            exp.for_secs.or(Some(config.default_for_secs)),
                            exp.recover_after_secs
                                .or(Some(config.default_recover_after_secs)),
                            violations,
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sentinel: neighbor observation failed"),
            }
        }

        // Default-route expectations.
        if !config.routes.is_empty()
            && let Some(rt) = route
        {
            match observe_routes(rt).await {
                Ok(obs) => {
                    for exp in &config.routes {
                        let rule = format!("route:{}", exp.name);
                        current_rules.insert(rule.clone());
                        let violations = check_route(exp, &obs);
                        self.report(
                            &rule,
                            exp.severity,
                            exp.for_secs.or(Some(config.default_for_secs)),
                            exp.recover_after_secs
                                .or(Some(config.default_recover_after_secs)),
                            violations,
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sentinel: route observation failed"),
            }
        }

        // Policy-rule expectations (#323): dump the live rule table and match
        // forbid/require selectors. Re-evaluated instantly on NewRule/DelRule
        // via the event wake path (`is_sentinel_relevant`).
        if !config.rules.is_empty()
            && let Some(rt) = route
        {
            match observe_rules(rt).await {
                Ok(obs) => {
                    for exp in &config.rules {
                        let rule = format!("rules:{}", exp.name);
                        current_rules.insert(rule.clone());
                        let violations = check_rules(exp, &obs);
                        self.report(
                            &rule,
                            exp.severity,
                            exp.for_secs.or(Some(config.default_for_secs)),
                            exp.recover_after_secs
                                .or(Some(config.default_recover_after_secs)),
                            violations,
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sentinel: rule observation failed"),
            }
        }

        // Metric-threshold expectations (read the collector's latest-value cache;
        // no nlink needed). Generic op/value comparison — the GUI-rule-promotion path.
        for exp in &config.metrics {
            let rule = format!("metric:{}", exp.name);
            current_rules.insert(rule.clone());
            let observed = self.metric_cache.get(&exp.metric).await;
            let violations = check_metric(exp, observed);
            self.report(
                &rule,
                exp.severity,
                exp.for_secs.or(Some(config.default_for_secs)),
                exp.recover_after_secs
                    .or(Some(config.default_recover_after_secs)),
                violations,
            )
            .await;
        }

        // Rate-of-change expectations (#113). The previous sample is retained per
        // rule in `rate_state`; the first sweep only records a baseline (no
        // violation). The rate spans the wall-clock interval between sweeps.
        for exp in &config.rates {
            let rule = format!("rate:{}", exp.name);
            current_rules.insert(rule.clone());
            let sample = match self.metric_cache.get(&exp.metric).await {
                Some(cur) => {
                    let now = Instant::now();
                    let prev = self
                        .rate_state
                        .lock()
                        .unwrap()
                        .insert(exp.name.clone(), (cur, now));
                    prev.map(|(pv, pt)| RateSample {
                        current: cur,
                        previous: pv,
                        interval_secs: now.duration_since(pt).as_secs_f64(),
                    })
                }
                None => None,
            };
            let violations = check_rate(exp, sample);
            self.report(
                &rule,
                exp.severity,
                exp.for_secs.or(Some(config.default_for_secs)),
                exp.recover_after_secs
                    .or(Some(config.default_recover_after_secs)),
                violations,
            )
            .await;
        }

        // Delivery-rate floor expectations (#113): a typed threshold over the
        // enriched tcp_info percentile metric (#108), read from the MetricCache.
        for exp in &config.delivery {
            let rule = format!("delivery:{}", exp.name);
            current_rules.insert(rule.clone());
            let observed = self.metric_cache.get(&exp.metric).await;
            let violations = check_delivery_floor(exp, observed);
            self.report(
                &rule,
                exp.severity,
                exp.for_secs.or(Some(config.default_for_secs)),
                exp.recover_after_secs
                    .or(Some(config.default_recover_after_secs)),
                violations,
            )
            .await;
        }

        // Route-flap expectations (#113): windowed increase of a cumulative
        // route-event counter, tracked per rule in `flap_state`.
        for exp in &config.route_flaps {
            let rule = format!("route_flap:{}", exp.name);
            current_rules.insert(rule.clone());
            let flaps = match self.metric_cache.get(&exp.metric).await {
                Some(cur) => {
                    let now_secs = self.flap_base.elapsed().as_secs();
                    let mut state = self.flap_state.lock().unwrap();
                    let samples = state.entry(exp.name.clone()).or_default();
                    samples.push((now_secs, cur as u64));
                    // Retain one sample at/before the cutoff (the baseline) plus
                    // all samples within the window — bounds the Vec growth.
                    let cutoff = now_secs.saturating_sub(exp.window_secs);
                    while samples.len() >= 2 && samples[1].0 <= cutoff {
                        samples.remove(0);
                    }
                    flaps_within(samples, now_secs, exp.window_secs)
                }
                None => 0,
            };
            let violations = check_route_flap(exp, flaps);
            self.report(
                &rule,
                exp.severity,
                exp.for_secs.or(Some(config.default_for_secs)),
                exp.recover_after_secs
                    .or(Some(config.default_recover_after_secs)),
                violations,
            )
            .await;
        }

        // Drop retained per-rule state for rate/flap rules no longer configured
        // (hot-swap): keeps the state maps from leaking removed rules.
        {
            let names: HashSet<&str> = config.rates.iter().map(|e| e.name.as_str()).collect();
            self.rate_state
                .lock()
                .unwrap()
                .retain(|k, _| names.contains(k.as_str()));
        }
        {
            let names: HashSet<&str> = config.route_flaps.iter().map(|e| e.name.as_str()).collect();
            self.flap_state
                .lock()
                .unwrap()
                .retain(|k, _| names.contains(k.as_str()));
        }

        // Resolve alerts for rules removed since the last sweep (hot-swap).
        let removed: Vec<String> = {
            let mut seen = self.seen_rules.lock().unwrap();
            let removed = seen.difference(&current_rules).cloned().collect::<Vec<_>>();
            *seen = current_rules;
            removed
        };
        for rule in removed {
            // Immediate, never held (#932): a recovery window says "wait, in
            // case it comes back", and a DELETED expectation is not coming
            // back — holding it would strand an alert for a rule nobody can
            // see or clear.
            if let Err(e) = self
                .reporter
                .reconcile_opts(&rule, &[], zensight_sensor_core::ReconcileOpts::immediate())
                .await
            {
                tracing::warn!(error = %e, rule = %rule, "sentinel: failed to resolve removed rule");
            }
        }
    }

    /// Turn the current violations for a rule into firing alerts and resolve any
    /// that are no longer present.
    async fn report(
        &self,
        rule: &str,
        severity: AlertSeverity,
        for_secs: Option<u64>,
        recover_after_secs: Option<u64>,
        violations: Vec<Violation>,
    ) {
        let for_duration = for_secs.map(Duration::from_secs);
        // `None` here means "use the reporter's own recovery", exactly as
        // `for_duration` means "use its debounce" — the sweep resolves the
        // per-expectation override and the set-wide default; the reporter
        // resolves the rest (#932).
        let opts = zensight_sensor_core::ReconcileOpts {
            recover_after: recover_after_secs.map(Duration::from_secs),
        };
        let mut firing_keys = Vec::new();
        for v in violations {
            let mut alert = Alert::new(
                &self.host,
                Protocol::Netlink,
                AlertKind::Expectation,
                rule,
                severity,
                v.summary,
            );
            for (k, val) in v.labels {
                alert = alert.with_label(k, val);
            }
            firing_keys.push(alert.alert_key());
            if let Err(e) = self.reporter.observe(alert, for_duration).await {
                tracing::warn!(error = %e, "sentinel: failed to publish alert");
            }
        }
        // Resolve previously-firing alerts under this rule that are now
        // satisfied — after the recovery hold, if one is configured (#932).
        if let Err(e) = self.reporter.reconcile_opts(rule, &firing_keys, opts).await {
            tracing::warn!(error = %e, "sentinel: failed to reconcile alerts");
        }
    }
}

/// Build a [`SocketObservation`] from live sockdiag.
async fn observe_sockets(conn: &Connection<SockDiag>) -> nlink::netlink::Result<SocketObservation> {
    let filter = SocketFilter::tcp().all_states().build();
    let socks = conn.query(&filter).await?;
    let mut obs = SocketObservation::default();
    for s in &socks {
        let SocketInfo::Inet(inet) = s else { continue };
        match inet.state {
            SocketState::Tcp(TcpState::Listen) | SocketState::Listen => {
                obs.listening_ports.insert(inet.local.port());
            }
            SocketState::Tcp(TcpState::Established) | SocketState::Established => {
                obs.established_remotes.push(inet.remote);
            }
            _ => {}
        }
    }
    Ok(obs)
}

/// Build a list of `(name, is_up)` from live netlink.
async fn observe_links(conn: &Connection<Route>) -> nlink::netlink::Result<Vec<(String, bool)>> {
    let links = conn.get_links().await?;
    Ok(links
        .into_iter()
        .map(|l| (l.name_or("?").to_string(), l.is_up()))
        .filter(|(n, _)| n != "?")
        .collect())
}

/// Build a list of `(ip, reachable)` from the live neighbor table. "Reachable"
/// excludes Failed/Incomplete/None (Stale/Delay/Probe/Reachable/Permanent count
/// as reachable — the entry resolves or is being revalidated).
async fn observe_neighbors(
    conn: &Connection<Route>,
) -> nlink::netlink::Result<Vec<(String, bool)>> {
    use nlink::netlink::neigh::State as NeighborState;
    let neighbors = conn.get_neighbors().await?;
    Ok(neighbors
        .into_iter()
        .filter_map(|n| {
            n.destination().map(|ip| {
                let reachable = !matches!(
                    n.state(),
                    NeighborState::Failed | NeighborState::Incomplete | NeighborState::None
                );
                (ip.to_string(), reachable)
            })
        })
        .collect())
}

/// Observe the policy-routing rule table from live netlink (#323), reduced to
/// the `(priority, table, action)` facts the pure checks match on.
async fn observe_rules(conn: &Connection<Route>) -> nlink::netlink::Result<RuleObservation> {
    let rules = conn.get_rules().await?;
    Ok(RuleObservation {
        rules: rules
            .iter()
            .map(|r| RuleFact {
                priority: r.priority(),
                table: r.table(),
                action: r.action().name().to_string(),
                is_default: r.is_default(),
            })
            .collect(),
    })
}

/// Observe the default-route state from live netlink.
async fn observe_routes(conn: &Connection<Route>) -> nlink::netlink::Result<RouteObservation> {
    let routes = conn.get_routes().await?;
    let mut obs = RouteObservation::default();
    for rt in &routes {
        // IPv4 default route (family AF_INET = 2).
        if rt.is_default() && rt.family() == 2 {
            obs.default_present = true;
            if obs.default_gw.is_none() {
                obs.default_gw = rt.gateway().map(|g| g.to_string());
            }
        }
    }
    Ok(obs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::ComparisonOp;

    fn obs_with(listening: &[u16], established: &[&str]) -> SocketObservation {
        SocketObservation {
            listening_ports: listening.iter().copied().collect(),
            established_remotes: established.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn listening_expectation_violated_when_absent() {
        let exp = SocketExpectation {
            name: "sshd".into(),
            listen: Some(22),
            established_to: None,
            min: 1,
            forbid_listen: None,
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        assert_eq!(check_socket(&exp, &obs_with(&[80], &[])).len(), 1);
        assert!(check_socket(&exp, &obs_with(&[22, 80], &[])).is_empty());
    }

    #[test]
    fn forbid_listen_violated_when_present() {
        let exp = SocketExpectation {
            name: "no-telnet".into(),
            listen: None,
            established_to: None,
            min: 1,
            forbid_listen: Some(23),
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        assert_eq!(check_socket(&exp, &obs_with(&[23], &[])).len(), 1);
        assert!(check_socket(&exp, &obs_with(&[22], &[])).is_empty());
    }

    #[test]
    fn established_to_counts_min() {
        let exp = SocketExpectation {
            name: "db".into(),
            listen: None,
            established_to: Some("10.0.0.5:5432".into()),
            min: 1,
            forbid_listen: None,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        // None established → violation.
        assert_eq!(check_socket(&exp, &obs_with(&[], &[])).len(), 1);
        // One to the right peer → satisfied.
        assert!(check_socket(&exp, &obs_with(&[], &["10.0.0.5:5432"])).is_empty());
        // One to a different peer → still violated.
        assert_eq!(
            check_socket(&exp, &obs_with(&[], &["10.0.0.9:5432"])).len(),
            1
        );
    }

    #[test]
    fn link_expectations() {
        let exp = LinkExpectation {
            iface: "eth0".into(),
            up: true,
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(check_link(&exp, Some(true)).is_empty());
        assert_eq!(check_link(&exp, Some(false)).len(), 1);
        assert_eq!(check_link(&exp, None).len(), 1); // absent
    }

    #[test]
    fn neighbor_expectations() {
        let exp = NeighborExpectation {
            ip: "10.0.0.1".into(),
            reachable: true,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(check_neighbor(&exp, Some(true)).is_empty()); // reachable → ok
        assert_eq!(check_neighbor(&exp, Some(false)).len(), 1); // unreachable → fire
        assert_eq!(check_neighbor(&exp, None).len(), 1); // absent → fire
    }

    #[test]
    fn metric_expectations() {
        // "retransmits should stay <= 100": observed 5 → ok; observed 250 → fire.
        let exp = MetricExpectation {
            name: "retrans".into(),
            metric: "sockets/tcp/retransmits_total".into(),
            op: ComparisonOp::LessOrEqual,
            value: 100.0,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(check_metric(&exp, Some(5.0)).is_empty());
        assert_eq!(check_metric(&exp, Some(250.0)).len(), 1);
        // Absent metric → no violation (only fires on data it has seen).
        assert!(check_metric(&exp, None).is_empty());
        // The firing violation names the metric in a label…
        let v = &check_metric(&exp, Some(250.0))[0];
        assert!(
            v.labels
                .iter()
                .any(|(k, val)| k == "metric" && val == "sockets/tcp/retransmits_total")
        );
        // …and puts the MEASURED VALUE in the summary and nowhere else (#932).
        // It used to ride an `actual` label, which `alert_key()` hashes — so a
        // moving value minted a new key every sweep and the `for_secs` window
        // could never elapse. See `Violation`'s doc.
        assert!(v.summary.contains("250"), "{}", v.summary);
        assert!(
            !v.labels.iter().any(|(k, _)| k == "actual"),
            "the measured value must not be a label: {:?}",
            v.labels
        );
        assert_eq!(
            check_metric(&exp, Some(250.0))[0].labels,
            check_metric(&exp, Some(999.0))[0].labels,
            "two different measurements must give ONE alert identity"
        );
    }

    #[test]
    fn rate_expectations() {
        // "rx_errors must not increase by > 60/min" (i.e. >1/sec).
        let exp = RateExpectation {
            name: "rx-err".into(),
            metric: "interfaces/eth0/rx_errors".into(),
            max_increase_per_min: 60.0,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        // No previous sample yet → no violation (baseline-only first sweep).
        assert!(check_rate(&exp, None).is_empty());
        // 100 → 110 over 30s = 20/min ≤ 60 → ok.
        assert!(
            check_rate(
                &exp,
                Some(RateSample {
                    current: 110.0,
                    previous: 100.0,
                    interval_secs: 30.0,
                })
            )
            .is_empty()
        );
        // 100 → 200 over 30s = 200/min > 60 → fire.
        let v = check_rate(
            &exp,
            Some(RateSample {
                current: 200.0,
                previous: 100.0,
                interval_secs: 30.0,
            }),
        );
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "metric" && val == "interfaces/eth0/rx_errors")
        );
        // The RATE is in the summary and not in a label (#932): it changes
        // every sweep, and `alert_key()` hashes every label.
        assert!(v[0].summary.contains("200.0/min"), "{}", v[0].summary);
        assert!(!v[0].labels.iter().any(|(k, _)| k == "rate_per_min"));
        // A counter reset (negative delta) does not fire.
        assert!(
            check_rate(
                &exp,
                Some(RateSample {
                    current: 5.0,
                    previous: 100.0,
                    interval_secs: 30.0,
                })
            )
            .is_empty()
        );
        // A zero interval does not divide-by-zero / fire.
        assert!(
            check_rate(
                &exp,
                Some(RateSample {
                    current: 200.0,
                    previous: 100.0,
                    interval_secs: 0.0,
                })
            )
            .is_empty()
        );
    }

    #[test]
    fn delivery_floor_expectations() {
        // "delivery_rate_p50 must stay >= 1_000_000 B/s".
        let exp = DeliveryFloorExpectation {
            name: "edge".into(),
            metric: "sockets/tcp/delivery_rate_p50".into(),
            floor: 1_000_000.0,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        // Above floor → ok.
        assert!(check_delivery_floor(&exp, Some(5_000_000.0)).is_empty());
        // Below floor → fire.
        let v = check_delivery_floor(&exp, Some(250_000.0));
        assert_eq!(v.len(), 1);
        // The measurement is in the summary, not in a label (#932).
        assert!(v[0].summary.contains("250000"), "{}", v[0].summary);
        assert!(!v[0].labels.iter().any(|(k, _)| k == "actual"));
        // Absent metric → no violation (only fires on data seen).
        assert!(check_delivery_floor(&exp, None).is_empty());
    }

    #[test]
    fn flaps_within_windowed_count() {
        // No samples → 0.
        assert_eq!(flaps_within(&[], 100, 60), 0);
        // Samples: counter rises 10 → 15 across the last 60s; baseline is the
        // sample at/just before cutoff (t=40, c=10), now=100, window=60 → cutoff=40.
        let samples = [(30u64, 8u64), (40, 10), (70, 12), (100, 15)];
        assert_eq!(flaps_within(&samples, 100, 60), 5); // 15 - 10
        // Wider window catches the earlier flaps too (baseline t=30, c=8).
        assert_eq!(flaps_within(&samples, 100, 80), 7); // 15 - 8
        // No baseline before cutoff → falls back to oldest in-window sample.
        assert_eq!(flaps_within(&[(95u64, 3u64), (100, 9)], 100, 60), 6); // 9 - 3
    }

    #[test]
    fn route_flap_expectations() {
        // "default route must not flap > 3 times per 60s".
        let exp = RouteFlapExpectation {
            name: "default".into(),
            metric: "events/route/removed_total".into(),
            max_flaps: 3,
            window_secs: 60,
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(check_route_flap(&exp, 0).is_empty());
        assert!(check_route_flap(&exp, 3).is_empty()); // at limit → ok
        let v = check_route_flap(&exp, 7); // above limit → fire
        assert_eq!(v.len(), 1);
        // The flap count is in the summary, not in a label (#932).
        assert!(v[0].summary.contains(" 7 times"), "{}", v[0].summary);
        assert!(!v[0].labels.iter().any(|(k, _)| k == "actual"));
    }

    #[test]
    fn route_expectations() {
        let exp = RouteExpectation {
            name: "default".into(),
            default_present: true,
            default_via: Some("10.0.0.1".into()),
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        // present + correct gw → ok
        assert!(
            check_route(
                &exp,
                &RouteObservation {
                    default_present: true,
                    default_gw: Some("10.0.0.1".into())
                }
            )
            .is_empty()
        );
        // absent → fire
        assert_eq!(check_route(&exp, &RouteObservation::default()).len(), 1);
        // present but wrong gw → fire
        assert_eq!(
            check_route(
                &exp,
                &RouteObservation {
                    default_present: true,
                    default_gw: Some("10.0.0.254".into())
                }
            )
            .len(),
            1
        );
    }

    // ---- policy-rule expectations (#323) ------------------------------------

    fn fact(priority: u32, table: u32, is_default: bool) -> RuleFact {
        RuleFact {
            priority,
            table,
            action: "lookup".into(),
            is_default,
        }
    }

    /// The kernel's baseline lookup rules (0/32766/32767 → local/main/default).
    fn baseline() -> Vec<RuleFact> {
        vec![
            fact(0, 255, true),
            fact(32766, 254, true),
            fact(32767, 253, true),
        ]
    }

    #[test]
    fn rule_expectations_forbid() {
        let exp = RuleExpectation {
            name: "no-diversion".into(),
            priority: None,
            table: None,
            sense: RuleSense::Forbid,
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        // Only the baseline lookup rules → quiet.
        let obs = RuleObservation { rules: baseline() };
        assert!(check_rules(&exp, &obs).is_empty());
        // An `ip rule add` diverting through table 200 → fire, tagged T1599.
        let mut rules = baseline();
        rules.push(fact(100, 200, false));
        let v = check_rules(&exp, &RuleObservation { rules });
        assert_eq!(v.len(), 1);
        assert!(v[0].summary.contains("prio 100"));
        assert!(v[0].summary.contains("table 200"));
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "technique" && val == "T1599")
        );
        // Two diversions → two violations (one alert per offending rule).
        let mut rules = baseline();
        rules.push(fact(100, 200, false));
        rules.push(fact(110, 201, false));
        assert_eq!(check_rules(&exp, &RuleObservation { rules }).len(), 2);
    }

    #[test]
    fn rule_expectations_forbid_selectors_narrow() {
        // Forbid only table 200; a rule into table 300 is someone else's problem.
        let exp = RuleExpectation {
            name: "no-table-200".into(),
            priority: None,
            table: Some(200),
            sense: RuleSense::Forbid,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        let mut rules = baseline();
        rules.push(fact(50, 300, false));
        assert!(check_rules(&exp, &RuleObservation { rules }).is_empty());
        let mut rules = baseline();
        rules.push(fact(50, 200, false));
        assert_eq!(check_rules(&exp, &RuleObservation { rules }).len(), 1);
    }

    #[test]
    fn rule_expectations_require() {
        // Require the VPN rule (prio 100 → table 51820) to stay installed.
        let exp = RuleExpectation {
            name: "vpn-rule".into(),
            priority: Some(100),
            table: Some(51820),
            sense: RuleSense::Require,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        // Present → quiet.
        let mut rules = baseline();
        rules.push(fact(100, 51820, false));
        assert!(check_rules(&exp, &RuleObservation { rules }).is_empty());
        // Withdrawn → fire with the missing selector in the summary.
        let v = check_rules(&exp, &RuleObservation { rules: baseline() });
        assert_eq!(v.len(), 1);
        assert!(v[0].summary.contains("prio 100 table 51820"));
        // Wrong table under the required priority still counts as missing.
        let mut rules = baseline();
        rules.push(fact(100, 200, false));
        assert_eq!(check_rules(&exp, &RuleObservation { rules }).len(), 1);
    }

    #[tokio::test]
    async fn rules_config_roundtrips_and_handle_mutates() {
        // SetExpectations round-trips the new field; add/remove by slug works.
        let json = r#"{
            "rules": [
                { "name": "no-diversion", "sense": "forbid", "severity": "critical" }
            ]
        }"#;
        let cfg: ExpectationsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].sense, RuleSense::Forbid);
        assert!(!cfg.is_empty());

        let handle = SentinelHandle::new(cfg);
        handle
            .add_rule(RuleExpectation {
                name: "vpn-rule".into(),
                priority: Some(100),
                table: Some(51820),
                sense: RuleSense::Require,
                severity: AlertSeverity::Warning,
                for_secs: None,
                recover_after_secs: None,
            })
            .await;
        assert_eq!(handle.snapshot().await.rules.len(), 2);
        handle.remove("rules:no-diversion").await;
        let snap = handle.snapshot().await;
        assert_eq!(snap.rules.len(), 1);
        assert_eq!(snap.rules[0].name, "vpn-rule");
        // `rules:` removal must not disturb `route:`/`route_flap:` slugs.
        handle.remove("route:default").await;
        assert_eq!(handle.snapshot().await.rules.len(), 1);
    }
    fn socket_exp() -> SocketExpectation {
        SocketExpectation {
            name: String::new(),
            listen: Some(22),
            established_to: None,
            min: 1,
            forbid_listen: None,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        }
    }

    fn route_exp() -> RouteExpectation {
        RouteExpectation {
            name: String::new(),
            default_present: true,
            default_via: None,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        }
    }

    /// Two expectations sharing a name within a family share one `alert_key`
    /// (RFC 11 §3.1 hashes the rule slug), so they fire and resolve over each
    /// other and an operator sees one alert flapping instead of two
    /// conditions. Nothing else catches it and nothing in the output shows it
    /// (#849).
    #[test]
    fn validate_refuses_a_name_collision_within_a_family() {
        let cfg = ExpectationsConfig {
            sockets: vec![
                SocketExpectation {
                    name: "ssh".into(),
                    ..socket_exp()
                },
                SocketExpectation {
                    name: "ssh".into(),
                    ..socket_exp()
                },
            ],
            ..Default::default()
        };
        let err = validate(&cfg).expect_err("two `sockets:ssh` are one alert key");
        assert!(err.contains("sockets") && err.contains("ssh"), "{err}");
    }

    /// An empty name yields the bare slug `sockets:` — every unnamed
    /// expectation in that family collapsing into one.
    #[test]
    fn validate_refuses_an_empty_name() {
        let cfg = ExpectationsConfig {
            sockets: vec![SocketExpectation {
                name: "   ".into(),
                ..socket_exp()
            }],
            ..Default::default()
        };
        assert!(validate(&cfg).unwrap_err().contains("empty name"));
    }

    /// The same name in *different* families is two different slugs and is
    /// fine — the check must not be fleet-wide-unique.
    #[test]
    fn validate_allows_one_name_across_two_families() {
        let cfg = ExpectationsConfig {
            sockets: vec![SocketExpectation {
                name: "gw".into(),
                ..socket_exp()
            }],
            routes: vec![RouteExpectation {
                name: "gw".into(),
                ..route_exp()
            }],
            ..Default::default()
        };
        assert!(validate(&cfg).is_ok(), "sockets:gw and routes:gw differ");
    }

    #[test]
    fn validate_refuses_a_zero_eval_interval() {
        let cfg = ExpectationsConfig {
            eval_interval_secs: 0,
            ..Default::default()
        };
        assert!(validate(&cfg).unwrap_err().contains("spin"));
    }

    #[test]
    fn validate_accepts_the_default_set() {
        assert!(validate(&ExpectationsConfig::default()).is_ok());
    }
}
