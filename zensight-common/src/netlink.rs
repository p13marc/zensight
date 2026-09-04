//! The netlink sentinel's wire types (#113, #323, moved here in #849): the
//! expectation vocabulary a fleet authors and the netlink sensor evaluates.
//!
//! They live in zensight-common for the same reason
//! [`crate::hostspec`]'s and [`crate::systemd`]'s do — they are WIRE
//! CONTRACTS with three consumers: the sensor (evaluates them), the GUI
//! (authors them), and the `@desired` fleet author (#816, which publishes
//! them per host). RFC 08 §7's schema gate requires a **real**
//! schemars-generated schema for every state-class payload, and a
//! sensor-crate type can never provide one: `zensight-common` cannot depend
//! on a sensor, so `describe` could only ever carry a summary stub. #815's
//! gate refused exactly that, correctly, which is why netlink's expectations
//! could not join `@desired` until they moved here.
//!
//! Checking logic — the pure `check_*` functions, the observation types they
//! match against, and the `Evaluator` that wires them to live netlink
//! connections — stays in the sensor. These are data.
//!
//! # The name
//!
//! [`NetlinkExpectations`], not `ExpectationsConfig`. The type table is a
//! flat namespace and systemd's set already holds that name on a shipped
//! `@rpc/systemd/expectations/set`; two shapes cannot share one entry.
//! Renaming netlink's request type is a breaking registry change, handled by
//! retire-and-sibling per RFC 08 §3 rather than in place.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::AlertSeverity;
use crate::comparison::ComparisonOp;

fn default_eval_interval() -> u64 {
    10
}
fn default_for_secs() -> u64 {
    15
}

/// Declared expectations for a host.
///
/// `Default` is hand-written rather than derived (#932): `main.rs` reaches it
/// through `expectations.clone().unwrap_or_default()`, and a derived `Default`
/// gave `eval_interval_secs = 0` and `default_for_secs = 0` — disagreeing with
/// the serde defaults a *file* gets for the same absent fields. A host with no
/// `expectations` block silently ran a different sentinel from one with an
/// empty `{}`. hostspec and systemd hand-wrote theirs to avoid exactly this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NetlinkExpectations {
    #[serde(default = "default_eval_interval")]
    pub eval_interval_secs: u64,
    #[serde(default = "default_for_secs")]
    pub default_for_secs: u64,
    /// Set-wide recovery hold (#932): how long every expectation must be
    /// **continuously clear** before its alert resolves, unless the
    /// expectation overrides it. `0` — the default — resolves on the first
    /// clear sweep, which is the behaviour before this field existed.
    ///
    /// This is *time* hysteresis. The value hysteresis a numeric rule wants —
    /// "fire above 90, clear below 80" — is `ThresholdRule::clear` (#928), on
    /// the threshold rules this sensor also evaluates.
    #[serde(default)]
    pub default_recover_after_secs: u64,
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
    /// Policy-routing rule expectations (#323): forbid non-baseline `ip rule`
    /// entries (traffic-diversion detection) or require a known rule to exist.
    #[serde(default)]
    pub rules: Vec<RuleExpectation>,
}

impl Default for NetlinkExpectations {
    fn default() -> Self {
        NetlinkExpectations {
            eval_interval_secs: default_eval_interval(),
            default_for_secs: default_for_secs(),
            default_recover_after_secs: 0,
            sockets: Vec::new(),
            links: Vec::new(),
            neighbors: Vec::new(),
            routes: Vec::new(),
            metrics: Vec::new(),
            rates: Vec::new(),
            delivery: Vec::new(),
            route_flaps: Vec::new(),
            rules: Vec::new(),
        }
    }
}

impl NetlinkExpectations {
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
            && self.links.is_empty()
            && self.neighbors.is_empty()
            && self.routes.is_empty()
            && self.metrics.is_empty()
            && self.rates.is_empty()
            && self.delivery.is_empty()
            && self.route_flaps.is_empty()
            && self.rules.is_empty()
    }
}

fn default_severity() -> AlertSeverity {
    AlertSeverity::Warning
}

/// A socket/connection expectation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

fn one() -> usize {
    1
}

/// An interface expectation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LinkExpectation {
    pub iface: String,
    /// The interface must be up (default true).
    #[serde(default = "default_true")]
    pub up: bool,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// A neighbor (gateway/peer) reachability expectation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

/// A default-route expectation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

/// A generic metric-threshold expectation: "metric `<op>` value should hold".
///
/// **Superseded by [`ThresholdRule`] (#931/#932).** This is the same idea in a
/// worse place: it lives in a sensor crate, so it can never carry a real
/// schemars schema and can never be a `@desired` document (`zensight-common`
/// cannot depend on a sensor — the #815 gate refused exactly that); it exists
/// only for netlink, so an operator has to learn a different vocabulary per
/// sensor; and it has no value hysteresis, so a metric sitting on the
/// threshold flaps.
///
/// `ThresholdsConfig` has all three, is evaluated on netlink's own publish
/// path since #931, and is authorable fleet-wide on `@desired`. A rule here:
///
/// ```json5
/// { name: "retrans", metric: "sockets/tcp/retransmits_total",
///   op: "LessOrEqual", value: 100.0 }
/// ```
///
/// becomes, under `thresholds.rules`, the same rule with the comparison the
/// right way round (a threshold rule states the FIRING condition, an
/// expectation states the healthy one) plus a `clear` if you want hysteresis:
///
/// ```json5
/// { name: "retrans", metric: "sockets/tcp/retransmits_total",
///   op: "GreaterThan", value: 100.0, clear: 80.0 }
/// ```
///
/// Kept working for now; removed one release after 0.13. Nothing else in the
/// expectation set is deprecated — the other eight kinds assert things about
/// the *host* that no metric threshold can express.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

/// A delivery-rate floor expectation (#113): alert when a socket-group's
/// delivery-rate percentile (from the enriched tcp_info, #108) falls below a
/// floor. Defaults to the `sockets/tcp/delivery_rate_p50` metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

/// A route-flap expectation (#113): alert when the default route changes or
/// withdraws more than `max_flaps` times within `window_secs`. Reads a cumulative
/// route-event counter (default `events/route/removed_total`) and compares its
/// increase over a sliding window — the windowing state lives in the
/// [`Evaluator`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}

/// Whether a rule expectation forbids or requires its matching rules (#323).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RuleSense {
    /// Fire when a matching **non-baseline** policy rule exists — the
    /// traffic-diversion guard ("table main not bypassed"). The kernel's three
    /// baseline lookup rules (priority 0 / 32766 / 32767) never count.
    #[default]
    Forbid,
    /// Fire when **no** matching policy rule exists — pins an expected rule
    /// (e.g. a VPN/mark rule that must stay installed).
    Require,
}

/// A policy-routing rule expectation (#323): forbid or require an `ip rule`
/// entry, matched by priority and/or lookup table (an unset field matches any).
/// An `ip rule add` that diverts traffic through another table re-evaluates this
/// instantly via the event wake path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuleExpectation {
    /// Label for the rule slug `rules:<name>` (e.g. "no-diversion").
    pub name: String,
    /// Match rules with this priority (`None` = any priority).
    #[serde(default)]
    pub priority: Option<u32>,
    /// Match rules looking up this table id (`None` = any table).
    #[serde(default)]
    pub table: Option<u32>,
    /// Forbid (default) or require the matching rules.
    #[serde(default)]
    pub sense: RuleSense,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default)]
    pub for_secs: Option<u64>,
    /// Per-expectation override of
    /// [`NetlinkExpectations::default_recover_after_secs`] (#932).
    #[serde(default)]
    pub recover_after_secs: Option<u64>,
}
