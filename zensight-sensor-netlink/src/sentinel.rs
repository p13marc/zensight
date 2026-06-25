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

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock};
use zensight_common::{Alert, AlertKind, AlertSeverity, ComparisonOp, Protocol};
use zensight_sensor_core::AlertReporter;

use nlink::netlink::{Connection, Route, SockDiag};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};

use crate::collector::MetricCache;

fn default_eval_interval() -> u64 {
    10
}
fn default_for_secs() -> u64 {
    15
}

/// Declared expectations for a host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExpectationsConfig {
    #[serde(default = "default_eval_interval")]
    pub eval_interval_secs: u64,
    #[serde(default = "default_for_secs")]
    pub default_for_secs: u64,
    #[serde(default)]
    pub sockets: Vec<SocketExpectation>,
    #[serde(default)]
    pub links: Vec<LinkExpectation>,
    #[serde(default)]
    pub neighbors: Vec<NeighborExpectation>,
    #[serde(default)]
    pub routes: Vec<RouteExpectation>,
    #[serde(default)]
    pub metrics: Vec<MetricExpectation>,
    /// Rate-of-change expectations (#113): "metric must not increase by > N/min".
    #[serde(default)]
    pub rates: Vec<RateExpectation>,
    /// Delivery-rate floor expectations (#113): per socket-group throughput floor.
    #[serde(default)]
    pub delivery: Vec<DeliveryFloorExpectation>,
    /// Route-flap expectations (#113): default route changing too often in a window.
    #[serde(default)]
    pub route_flaps: Vec<RouteFlapExpectation>,
}

impl ExpectationsConfig {
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
            && self.links.is_empty()
            && self.neighbors.is_empty()
            && self.routes.is_empty()
            && self.metrics.is_empty()
            && self.rates.is_empty()
            && self.delivery.is_empty()
            && self.route_flaps.is_empty()
    }
}

fn default_severity() -> AlertSeverity {
    AlertSeverity::Warning
}

/// A socket/connection expectation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocketExpectation {
    /// Human label, e.g. "sshd". Forms the rule slug `socket:<name>`.
    pub name: String,
    /// Port that must be LISTENing.
    #[serde(default)]
    pub listen: Option<u16>,
    /// `host:port` that must have at least `min` ESTABLISHED connections.
    #[serde(default)]
    pub established_to: Option<String>,
    #[serde(default = "one")]
    pub min: usize,
    /// Port that must NOT be listening.
    #[serde(default)]
    pub forbid_listen: Option<u16>,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    /// Per-expectation debounce override (seconds).
    #[serde(default)]
    pub for_secs: Option<u64>,
}

fn one() -> usize {
    1
}

/// An interface expectation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkExpectation {
    pub iface: String,
    /// The interface must be up (default true).
    #[serde(default = "default_true")]
    pub up: bool,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// A neighbor (gateway/peer) reachability expectation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborExpectation {
    /// IP address that must be a reachable neighbor (ARP/NDP).
    pub ip: String,
    /// Must be reachable (default true).
    #[serde(default = "default_true")]
    pub reachable: bool,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

/// A default-route expectation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteExpectation {
    /// Label for the rule slug `route:<name>` (e.g. "default").
    pub name: String,
    /// A default route must be present.
    #[serde(default = "default_true")]
    pub default_present: bool,
    /// If set, the default route must go via this gateway IP.
    #[serde(default)]
    pub default_via: Option<String>,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

/// A generic metric-threshold expectation: "metric `<op>` value should hold".
/// The keystone for promoting a GUI threshold rule into a headless expectation
/// (shares [`ComparisonOp`] with the frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricExpectation {
    /// Label for the rule slug `metric:<name>`.
    pub name: String,
    /// Metric path to watch, e.g. `sockets/tcp/retransmits_total`.
    pub metric: String,
    /// Comparison operator the metric value must satisfy.
    pub op: ComparisonOp,
    /// Right-hand side of the comparison.
    pub value: f64,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

fn default_delivery_metric() -> String {
    "sockets/tcp/delivery_rate_p50".to_string()
}

fn default_flap_metric() -> String {
    "events/route/removed_total".to_string()
}

fn default_flap_window() -> u64 {
    60
}

/// A rate-of-change expectation (#113): "metric `<name>` must not *increase* by
/// more than `max_increase_per_min` per minute".
///
/// This is the missing primitive: it needs two samples of the metric at known
/// instants to compute a delta/interval rate. The previous sample is retained in
/// the [`Evaluator`] (per-rule), *not* in the [`MetricCache`]: the rate is
/// measured between consecutive sentinel sweeps (the natural evaluation cadence)
/// and the cache stays a simple latest-value store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateExpectation {
    /// Label for the rule slug `rate:<name>`.
    pub name: String,
    /// Metric path to watch, e.g. `interfaces/eth0/rx_errors` or
    /// `sockets/tcp/retransmits_total`.
    pub metric: String,
    /// Maximum permitted increase per minute before the rule fires.
    pub max_increase_per_min: f64,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

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

/// A delivery-rate floor expectation (#113): alert when a socket-group's
/// delivery-rate percentile (from the enriched tcp_info, #108) falls below a
/// floor. Defaults to the `sockets/tcp/delivery_rate_p50` metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryFloorExpectation {
    /// Label for the rule slug `delivery:<name>`.
    pub name: String,
    /// Delivery-rate metric path to watch (default
    /// `sockets/tcp/delivery_rate_p50`).
    #[serde(default = "default_delivery_metric")]
    pub metric: String,
    /// Minimum delivery rate (bytes/sec) that must hold; fire strictly below it.
    pub floor: f64,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

/// A route-flap expectation (#113): alert when the default route changes or
/// withdraws more than `max_flaps` times within `window_secs`. Reads a cumulative
/// route-event counter (default `events/route/removed_total`) and compares its
/// increase over a sliding window — the windowing state lives in the
/// [`Evaluator`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteFlapExpectation {
    /// Label for the rule slug `route_flap:<name>`.
    pub name: String,
    /// Cumulative flap counter to watch (default `events/route/removed_total`).
    #[serde(default = "default_flap_metric")]
    pub metric: String,
    /// Maximum flaps permitted within the window before the rule fires.
    pub max_flaps: u64,
    /// Sliding window length in seconds (default 60).
    #[serde(default = "default_flap_window")]
    pub window_secs: u64,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
}

/// Observed default-route facts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteObservation {
    pub default_present: bool,
    pub default_gw: Option<String>,
}

/// A single currently-violated fact.
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
                    ("actual".into(), count.to_string()),
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
                ("actual".into(), v.to_string()),
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
                ("rate_per_min".into(), format!("{per_min:.1}")),
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
                ("actual".into(), v.to_string()),
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
                ("actual".into(), flaps_in_window.to_string()),
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
    /// Remove an expectation by rule slug (`socket:<name>` / `link:<iface>` /
    /// `neighbor:<ip>` / `route:<name>` / `metric:<name>` / `rate:<name>` /
    /// `delivery:<name>` / `route_flap:<name>`).
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

impl Evaluator {
    pub fn new(
        host: String,
        config: ExpectationsConfig,
        reporter: Arc<AlertReporter>,
        metric_cache: MetricCache,
    ) -> Self {
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
                        self.report(&rule, exp.severity, exp.for_secs, violations)
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
                        self.report(&rule, exp.severity, exp.for_secs, violations)
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
                        self.report(&rule, exp.severity, exp.for_secs, violations)
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
                        self.report(&rule, exp.severity, exp.for_secs, violations)
                            .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sentinel: route observation failed"),
            }
        }

        // Metric-threshold expectations (read the collector's latest-value cache;
        // no nlink needed). Generic op/value comparison — the GUI-rule-promotion path.
        for exp in &config.metrics {
            let rule = format!("metric:{}", exp.name);
            current_rules.insert(rule.clone());
            let observed = self.metric_cache.get(&exp.metric).await;
            let violations = check_metric(exp, observed);
            self.report(&rule, exp.severity, exp.for_secs, violations)
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
            self.report(&rule, exp.severity, exp.for_secs, violations)
                .await;
        }

        // Delivery-rate floor expectations (#113): a typed threshold over the
        // enriched tcp_info percentile metric (#108), read from the MetricCache.
        for exp in &config.delivery {
            let rule = format!("delivery:{}", exp.name);
            current_rules.insert(rule.clone());
            let observed = self.metric_cache.get(&exp.metric).await;
            let violations = check_delivery_floor(exp, observed);
            self.report(&rule, exp.severity, exp.for_secs, violations)
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
            self.report(&rule, exp.severity, exp.for_secs, violations)
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
            if let Err(e) = self.reporter.reconcile(&rule, &[]).await {
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
        violations: Vec<Violation>,
    ) {
        let for_duration = for_secs.map(Duration::from_secs);
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
        // Resolve previously-firing alerts under this rule that are now satisfied.
        if let Err(e) = self.reporter.reconcile(rule, &firing_keys).await {
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
        };
        assert!(check_metric(&exp, Some(5.0)).is_empty());
        assert_eq!(check_metric(&exp, Some(250.0)).len(), 1);
        // Absent metric → no violation (only fires on data it has seen).
        assert!(check_metric(&exp, None).is_empty());
        // The firing violation carries metric + actual labels.
        let v = &check_metric(&exp, Some(250.0))[0];
        assert!(
            v.labels
                .iter()
                .any(|(k, val)| k == "metric" && val == "sockets/tcp/retransmits_total")
        );
        assert!(
            v.labels
                .iter()
                .any(|(k, val)| k == "actual" && val == "250")
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
        assert!(v[0].labels.iter().any(|(k, _)| k == "rate_per_min"));
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
        };
        // Above floor → ok.
        assert!(check_delivery_floor(&exp, Some(5_000_000.0)).is_empty());
        // Below floor → fire.
        let v = check_delivery_floor(&exp, Some(250_000.0));
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "actual" && val == "250000")
        );
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
        };
        assert!(check_route_flap(&exp, 0).is_empty());
        assert!(check_route_flap(&exp, 3).is_empty()); // at limit → ok
        let v = check_route_flap(&exp, 7); // above limit → fire
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "actual" && val == "7")
        );
    }

    #[test]
    fn route_expectations() {
        let exp = RouteExpectation {
            name: "default".into(),
            default_present: true,
            default_via: Some("10.0.0.1".into()),
            severity: AlertSeverity::Critical,
            for_secs: None,
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
}
