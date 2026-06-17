//! Expectation engine (Pillar B): declare what the machine *should* look like,
//! evaluate it against observed kernel state, and emit alerts on deviation.
//!
//! Embedded in the netlink sensor (it needs the same netlink access). The check
//! logic is pure and unit-tested; the [`Evaluator`] wires it to live nlink
//! connections + an [`AlertReporter`].

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

use nlink::netlink::{Connection, Route, SockDiag};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};

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
}

impl ExpectationsConfig {
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty() && self.links.is_empty() && self.neighbors.is_empty()
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
    /// Remove an expectation by rule slug (`socket:<name>` / `link:<iface>` /
    /// `neighbor:<ip>`).
    pub async fn remove(&self, rule: &str) {
        let mut c = self.expectations.write().await;
        if let Some(name) = rule.strip_prefix("socket:") {
            c.sockets.retain(|e| e.name != name);
        } else if let Some(iface) = rule.strip_prefix("link:") {
            c.links.retain(|e| e.iface != iface);
        } else if let Some(ip) = rule.strip_prefix("neighbor:") {
            c.neighbors.retain(|e| e.ip != ip);
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
    /// Rules evaluated on the previous sweep — used to resolve alerts for rules
    /// that were removed (hot-swap) so they don't linger forever.
    seen_rules: std::sync::Mutex<HashSet<String>>,
}

impl Evaluator {
    pub fn new(host: String, config: ExpectationsConfig, reporter: Arc<AlertReporter>) -> Self {
        Self {
            host,
            expectations: Arc::new(RwLock::new(config)),
            reporter,
            seen_rules: std::sync::Mutex::new(HashSet::new()),
        }
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
            tokio::time::sleep(Duration::from_secs(interval)).await;
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
}
