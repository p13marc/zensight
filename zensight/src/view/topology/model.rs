//! Pure topology graph model: node/edge types and the derivation functions
//! that build them from observed sensor data (flows, neighbor tables).
//!
//! Everything in this module is pure given its inputs — no iced types beyond
//! what the data itself needs, no Zenoh, no app state — so the graph
//! derivation logic is unit-testable in isolation. Stateful orchestration
//! (caches, selection, layout) lives in [`super::TopologyState`].

use std::collections::HashMap;

/// Unique identifier for a topology node.
pub type NodeId = String;

/// A firing sensor alert attached to a node, for the info panel (#83).
#[derive(Debug, Clone)]
pub struct NodeAlert {
    pub severity: zensight_common::AlertSeverity,
    pub rule: String,
    pub summary: String,
}

/// What kind of observation an edge represents (#391). Drives line style and
/// which lenses show it (P2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeKind {
    /// Observed traffic (netring matrix/flows) — carries a rate when the
    /// traffic matrix supplied one.
    #[default]
    Flow,
    /// L2 adjacency from a netlink neighbor (ARP/NDP) table entry.
    L2Adjacency,
    /// Host → its default gateway, from the `routes/default_v4_gw` metric.
    Gateway,
}

/// What a node *is* on the network (#391). From the netring passive-asset
/// inventory (`AssetRecord.role`) when available, else inferred (neighbor
/// `is_router`), else `Host`/`Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeRole {
    /// An ordinary monitored host/VM.
    #[default]
    Host,
    Router,
    Switch,
    AccessPoint,
    Phone,
    Iot,
    /// The aggregate of off-subnet/public destinations (produced by the P2
    /// external-aggregate grouping; the variant exists from P1 so match arms
    /// don't churn).
    Internet,
    Unknown,
}

impl NodeRole {
    /// Parse the netring `AssetRecord.role` vocabulary
    /// (`router`/`switch`/`ap`/`phone`/`iot`/`host`/`unknown`, #329).
    pub fn from_asset_role(s: &str) -> Self {
        match s {
            "router" => Self::Router,
            "switch" => Self::Switch,
            "ap" => Self::AccessPoint,
            "phone" => Self::Phone,
            "iot" => Self::Iot,
            "host" => Self::Host,
            _ => Self::Unknown,
        }
    }

    /// Short glyph drawn inside non-host nodes on the canvas (monochrome in
    /// P1; proper icons are P4 polish).
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Host => "",
            Self::Router => "R",
            Self::Switch => "SW",
            Self::AccessPoint => "AP",
            Self::Phone => "Ph",
            Self::Iot => "IoT",
            Self::Internet => "@",
            Self::Unknown => "?",
        }
    }

    /// Human-readable role name for panels.
    pub fn label(self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::Router => "Router",
            Self::Switch => "Switch",
            Self::AccessPoint => "Access point",
            Self::Phone => "Phone",
            Self::Iot => "IoT device",
            Self::Internet => "Internet",
            Self::Unknown => "Unknown",
        }
    }
}

/// How we know about a node (#391): from its own sensors, purely from the
/// wire, or as an external aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provenance {
    /// Has at least one live sensor facet reporting from the host itself.
    #[default]
    Monitored,
    /// Wire-only (#306): a correlator entity observed purely on the wire
    /// (netring/netlink), with no live sensor device of its own. Rendered
    /// dimmed / dashed.
    Passive,
    /// Off-subnet/public aggregate (P2 external grouping).
    External,
}

/// Node health (#391), replacing the old boolean: device liveness + host-scoped
/// sensor health + entity staleness, worst wins. See [`node_health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeHealth {
    #[default]
    Healthy,
    /// Some facet is offline/degraded, or a sensor on the host reports trouble.
    Degraded,
    /// Every device facet is offline.
    Down,
    /// No fresh data: entity past its staleness window (previously invisible).
    Stale,
}

/// A node in the topology graph.
#[derive(Debug, Clone, Default)]
pub struct Node {
    /// Unique node identifier.
    pub id: NodeId,
    /// Display label.
    pub label: String,
    /// Position in graph coordinates.
    pub position: (f32, f32),
    /// Velocity for force-directed layout.
    pub velocity: (f32, f32),
    /// What the node is on the network (#391): router/switch/ap/iot/…
    pub role: NodeRole,
    /// How we know about it (#391): monitored / wire-only / external.
    pub provenance: Provenance,
    /// Node health (#391): liveness + sensor health + staleness, worst wins.
    pub health: NodeHealth,
    /// Hardware vendor, from the passive-asset inventory (#391).
    pub vendor: Option<String>,
    /// Which protocols' devices map to this host (#83). Drives the header icon
    /// and the "covered by" badges in the info panel.
    pub protocols: std::collections::BTreeSet<zensight_common::Protocol>,
    /// CPU usage percentage (0-100). From sysinfo.
    pub cpu_usage: Option<f64>,
    /// Memory usage percentage (0-100). From sysinfo.
    pub memory_usage: Option<f64>,
    /// Cumulative network RX bytes (counter). From sysinfo.
    pub network_rx: Option<u64>,
    /// Cumulative network TX bytes (counter). From sysinfo.
    pub network_tx: Option<u64>,
    /// Live receive rate, bytes/sec, from hot-ring counter deltas (#391).
    pub rx_rate: Option<f64>,
    /// Live transmit rate, bytes/sec, from hot-ring counter deltas (#391).
    pub tx_rate: Option<f64>,
    /// Interfaces up / total, from netlink `iface/<n>/up` (#83).
    pub iface_up: Option<u32>,
    pub iface_total: Option<u32>,
    /// TCP socket-state gauges, from netlink `sockets/tcp/*` (#83).
    pub tcp_established: Option<f64>,
    pub tcp_listen: Option<f64>,
    /// Route / neighbor table sizes, from netlink (#83).
    pub routes_total: Option<f64>,
    pub neighbors_total: Option<f64>,
    /// Whether the node position is pinned (not affected by layout).
    pub pinned: bool,
    /// Highest-severity firing sensor alert for this host, if any (overlay).
    pub alert: Option<zensight_common::AlertSeverity>,
    /// Firing sensor alerts for this host, listed in the info panel (#83).
    pub alerts: Vec<NodeAlert>,
    /// Number of sensors that have correlated this host (#25). `None` until a
    /// correlation entry references it; surfaces the otherwise-dead correlations
    /// map as a "seen by N sensors" node label.
    pub sensor_count: Option<usize>,
    /// Total telemetry metrics tracked across this host's facets (#83). A
    /// protocol-agnostic signal so nodes whose protocol has no dedicated panel
    /// section (netflow / snmp / modbus / gnmi) still show something useful.
    pub metric_count: usize,
}

impl Node {
    /// Whether the node is in the nominal health state.
    pub fn is_healthy(&self) -> bool {
        matches!(self.health, NodeHealth::Healthy)
    }

    /// Update node metrics from telemetry.
    pub fn update_from_metrics(
        &mut self,
        metrics: &HashMap<String, zensight_common::TelemetryPoint>,
    ) {
        use zensight_common::TelemetryValue;

        // Netlink interface inventory: `iface/<name>/up` booleans. Counted in a
        // single pass since they're spread across many keys.
        let mut iface_up = 0u32;
        let mut iface_total = 0u32;
        let mut saw_iface = false;

        for (name, point) in metrics {
            match name.as_str() {
                // ── sysinfo ──
                "cpu/usage" => {
                    if let TelemetryValue::Gauge(v) = &point.value {
                        self.cpu_usage = Some(*v);
                    }
                }
                "memory/usage_percent" => {
                    if let TelemetryValue::Gauge(v) = &point.value {
                        self.memory_usage = Some(*v);
                    }
                }
                // ── netlink (#83) ──
                "sockets/tcp/established" => {
                    if let TelemetryValue::Gauge(v) = &point.value {
                        self.tcp_established = Some(*v);
                    }
                }
                "sockets/tcp/listen" => {
                    if let TelemetryValue::Gauge(v) = &point.value {
                        self.tcp_listen = Some(*v);
                    }
                }
                "routes/total" => {
                    if let TelemetryValue::Gauge(v) = &point.value {
                        self.routes_total = Some(*v);
                    }
                }
                "neighbors/total" => {
                    if let TelemetryValue::Gauge(v) = &point.value {
                        self.neighbors_total = Some(*v);
                    }
                }
                _ => {
                    // sysinfo network counters
                    if name.starts_with("network/") && name.ends_with("/rx_bytes") {
                        if let TelemetryValue::Counter(v) = &point.value {
                            self.network_rx = Some(*v);
                        }
                    } else if name.starts_with("network/")
                        && name.ends_with("/tx_bytes")
                        && let TelemetryValue::Counter(v) = &point.value
                    {
                        self.network_tx = Some(*v);
                    } else if name.starts_with("iface/") && name.ends_with("/up") {
                        // netlink per-interface up/down
                        saw_iface = true;
                        iface_total += 1;
                        if let TelemetryValue::Boolean(true) = &point.value {
                            iface_up += 1;
                        }
                    }
                }
            }
        }

        if saw_iface {
            self.iface_up = Some(iface_up);
            self.iface_total = Some(iface_total);
        }
    }
}

/// An edge (connection) in the topology graph.
#[derive(Debug, Clone, Default)]
pub struct Edge {
    /// Source node ID. For rated [`EdgeKind::Flow`] edges this is the heavier
    /// direction's initiator; for [`EdgeKind::Gateway`] the host.
    pub from: NodeId,
    /// Destination node ID.
    pub to: NodeId,
    /// What kind of observation this link is (#391).
    pub kind: EdgeKind,
    /// Forward (`from` → `to`) rate in **bytes/sec**, from the netring traffic
    /// matrix (#391). `0.0` when unrated (flow-fallback / L2 / gateway edges) —
    /// direction is only drawn when a rate was actually observed.
    pub rate: f64,
    /// Reverse (`to` → `from`) rate in bytes/sec (#391).
    pub reverse_rate: f64,
    /// Cumulative bytes transferred (from flow records).
    pub bytes: u64,
    /// Cumulative packets transferred (from flow records).
    pub packets: u64,
    /// Protocol (TCP, UDP, etc.).
    pub protocol: Option<String>,
    /// Last seen timestamp.
    pub last_seen: i64,
    /// Per-link health (#49): the max alert severity of the two endpoint nodes,
    /// so a link to/from a host in trouble is visually flagged. Set by
    /// [`super::TopologyState::apply_alerts`].
    pub alert: Option<zensight_common::AlertSeverity>,
}

/// Extract the bare IP from an `ip:port` endpoint string. Handles IPv6 in
/// brackets (`[::1]:443`) and bare IPs (no port). Pure.
pub fn endpoint_ip(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        // `[v6]:port` -> the part before `]`.
        return rest.split(']').next().unwrap_or(rest);
    }
    // `v4:port` -> before the (single) colon; bare IPv6 has many colons, no port.
    match endpoint.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            host
        }
        _ => endpoint,
    }
}

/// Aggregate observed flows into topology edges (#25). One edge per unordered
/// pair of *distinct* known nodes, summing bytes/packets; the protocol of the
/// highest-volume contributing flow labels the edge. Flows touching an unknown
/// IP or a self-loop are skipped. Pure — the unit of testing for edge derivation.
pub fn edges_from_flows(
    flows: &[zensight_common::FlowRecord],
    ip_to_node: &HashMap<String, NodeId>,
    now_ms: i64,
) -> Vec<Edge> {
    // Keyed by ordered node pair so (a,b) and (b,a) aggregate together.
    let mut acc: HashMap<(NodeId, NodeId), (u64, u64, String, u64)> = HashMap::new();
    for f in flows {
        let src_node = ip_to_node.get(endpoint_ip(&f.src));
        let dst_node = ip_to_node.get(endpoint_ip(&f.dst));
        let (Some(a), Some(b)) = (src_node, dst_node) else {
            continue;
        };
        if a == b {
            continue; // self-loop: same host both ends
        }
        let key = if a <= b {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        };
        let entry = acc.entry(key).or_insert((0, 0, f.proto.clone(), 0));
        entry.0 += f.bytes;
        entry.1 += f.packets;
        // Label the edge with the protocol of its largest single flow.
        if f.bytes > entry.3 {
            entry.2 = f.proto.clone();
            entry.3 = f.bytes;
        }
    }
    let mut edges: Vec<Edge> = acc
        .into_iter()
        .map(|((from, to), (bytes, packets, protocol, _))| Edge {
            from,
            to,
            kind: EdgeKind::Flow,
            bytes,
            packets,
            protocol: Some(protocol),
            last_seen: now_ms,
            ..Default::default()
        })
        .collect();
    // Stable order: heaviest edges first, then by endpoints.
    edges.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| (a.from.clone(), a.to.clone()).cmp(&(b.from.clone(), b.to.clone())))
    });
    edges
}

/// Order a node pair canonically so `(a,b)` and `(b,a)` compare equal. Pure.
pub(crate) fn ordered_pair(a: &NodeId, b: &NodeId) -> (NodeId, NodeId) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

/// Derive adjacency edges from a netlink host's neighbor (ARP/NDP) table (#49).
/// Each neighbor whose IP resolves (via `ip_to_node`) to a *distinct* known node
/// becomes a zero-bandwidth link from its owning `host_nodes` entry — so a host
/// and its directly-attached gateway/peer connect even when netring observes no
/// flow between them. Neighbors flagged `is_router` are returned as the set of
/// node ids to classify [`NodeRole::Router`]. Pure — the unit of testing.
pub fn edges_from_neighbors(
    host_nodes: &[NodeId],
    neighbors: &[zensight_common::NeighborRecord],
    ip_to_node: &HashMap<String, NodeId>,
    now_ms: i64,
) -> (Vec<Edge>, std::collections::BTreeSet<NodeId>) {
    use std::collections::{BTreeMap, BTreeSet};

    let mut pairs: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
    let mut routers: BTreeSet<NodeId> = BTreeSet::new();
    // Deterministic order: BTreeMap keyed by ordered pair.
    let mut acc: BTreeMap<(NodeId, NodeId), ()> = BTreeMap::new();
    for host in host_nodes {
        for nb in neighbors {
            let Some(ip) = nb.ip.as_deref() else { continue };
            let Some(target) = ip_to_node.get(ip) else {
                continue;
            };
            if target == host {
                continue; // the host's own address
            }
            if nb.is_router {
                routers.insert(target.clone());
            }
            let key = ordered_pair(host, target);
            if pairs.insert(key.clone()) {
                acc.insert(key, ());
            }
        }
    }
    let edges = acc
        .into_keys()
        .map(|(from, to)| Edge {
            from,
            to,
            kind: EdgeKind::L2Adjacency,
            last_seen: now_ms,
            ..Default::default()
        })
        .collect();
    (edges, routers)
}

/// Whether a protocol's `source` represents a physical host/device that should be
/// a topology node (#83). sysinfo/netlink hosts, netflow exporters, and
/// gNMI/SNMP/Modbus network gear are nodes; syslog (log overlay) and netring (flow
/// overlay that supplies the *edges*) annotate existing nodes rather than adding
/// their own.
pub(crate) fn is_node_protocol(p: zensight_common::Protocol) -> bool {
    use zensight_common::Protocol;
    matches!(
        p,
        Protocol::Sysinfo
            | Protocol::Netlink
            | Protocol::Netflow
            | Protocol::Gnmi
            | Protocol::Snmp
            | Protocol::Modbus
    )
}

/// Display label for an entity-backed node: hostname > fqdn > short entity id.
pub(crate) fn entity_node_label(e: &zensight_common::HostEntity) -> String {
    e.hostname
        .clone()
        .or_else(|| e.fqdn.clone())
        .unwrap_or_else(|| e.entity_id.clone())
}

/// Pick the icon protocol for a node: prefer sysinfo (the host identity), then
/// netlink, otherwise the first protocol that covers the host (#83).
pub(crate) fn primary_protocol(node: &Node) -> zensight_common::Protocol {
    use zensight_common::Protocol;
    if node.protocols.contains(&Protocol::Sysinfo) {
        Protocol::Sysinfo
    } else if node.protocols.contains(&Protocol::Netlink) {
        Protocol::Netlink
    } else {
        node.protocols
            .iter()
            .next()
            .copied()
            .unwrap_or(Protocol::Sysinfo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(
        src: &str,
        dst: &str,
        bytes: u64,
        packets: u64,
        proto: &str,
    ) -> zensight_common::FlowRecord {
        zensight_common::FlowRecord {
            src: src.to_string(),
            dst: dst.to_string(),
            proto: proto.to_string(),
            bytes,
            packets,
            duration_ms: 0,
            reason: "fin".to_string(),
            community_id: None,
            directed: true,
            bytes_initiator: bytes / 2,
            bytes_responder: bytes - bytes / 2,
            packets_initiator: packets / 2,
            packets_responder: packets - packets / 2,
            dst_names: Vec::new(),
        }
    }

    #[test]
    fn endpoint_ip_parses_v4_v6_bare() {
        assert_eq!(endpoint_ip("10.0.0.1:443"), "10.0.0.1");
        assert_eq!(endpoint_ip("[2001:db8::1]:80"), "2001:db8::1");
        assert_eq!(endpoint_ip("10.0.0.2"), "10.0.0.2"); // no port
        assert_eq!(endpoint_ip("::1"), "::1"); // bare v6, no port
    }

    #[test]
    fn edges_from_flows_aggregates_known_pairs() {
        let mut map = HashMap::new();
        map.insert("10.0.0.1".to_string(), "hostA".to_string());
        map.insert("10.0.0.2".to_string(), "hostB".to_string());
        let flows = vec![
            flow("10.0.0.1:5000", "10.0.0.2:443", 1000, 10, "tcp"),
            // Reverse direction aggregates into the same unordered pair.
            flow("10.0.0.2:443", "10.0.0.1:5000", 500, 5, "tcp"),
            // Touches an unknown IP -> skipped.
            flow("10.0.0.1:5001", "8.8.8.8:53", 999, 9, "udp"),
        ];
        let edges = edges_from_flows(&flows, &map, 42);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!((e.from.as_str(), e.to.as_str()), ("hostA", "hostB"));
        assert_eq!(e.bytes, 1500);
        assert_eq!(e.packets, 15);
        assert_eq!(e.last_seen, 42);
        assert_eq!(e.protocol.as_deref(), Some("tcp"));
    }

    #[test]
    fn edges_from_flows_skips_self_loops_and_unknown() {
        let mut map = HashMap::new();
        map.insert("10.0.0.1".to_string(), "hostA".to_string());
        let flows = vec![
            // self-loop (same node both ends) -> skipped
            flow("10.0.0.1:1", "10.0.0.1:2", 100, 1, "tcp"),
            // both unknown -> skipped
            flow("1.1.1.1:1", "2.2.2.2:2", 100, 1, "tcp"),
        ];
        assert!(edges_from_flows(&flows, &map, 0).is_empty());
    }

    #[test]
    fn edges_sorted_heaviest_first() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "a".to_string());
        map.insert("b".to_string(), "b".to_string());
        map.insert("c".to_string(), "c".to_string());
        let flows = vec![
            flow("a:1", "b:2", 100, 1, "tcp"),
            flow("a:1", "c:2", 5000, 1, "tcp"),
        ];
        let edges = edges_from_flows(&flows, &map, 0);
        assert_eq!(edges[0].bytes, 5000);
        assert_eq!(edges[1].bytes, 100);
    }

    fn neighbor(ip: &str, is_router: bool) -> zensight_common::NeighborRecord {
        zensight_common::NeighborRecord {
            family: 2,
            ip: Some(ip.to_string()),
            mac: Some("aa:bb:cc:dd:ee:ff".to_string()),
            ifindex: 2,
            state: "reachable".to_string(),
            is_router,
        }
    }

    #[test]
    fn edges_from_neighbors_builds_adjacency_and_routers() {
        let mut map = HashMap::new();
        map.insert("10.0.0.1".to_string(), "hostA".to_string()); // the netlink host
        map.insert("10.0.0.254".to_string(), "gw".to_string());
        map.insert("10.0.0.2".to_string(), "hostB".to_string());
        let hosts = vec!["hostA".to_string()];
        let neighbors = vec![
            neighbor("10.0.0.254", true), // gateway -> Router + edge
            neighbor("10.0.0.2", false),  // peer -> edge
            neighbor("10.0.0.1", false),  // host's own addr -> skipped
            neighbor("8.8.8.8", true),    // unknown node -> skipped
        ];
        let (edges, routers) = edges_from_neighbors(&hosts, &neighbors, &map, 7);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.bytes == 0 && e.last_seen == 7));
        let pairs: std::collections::BTreeSet<_> =
            edges.iter().map(|e| ordered_pair(&e.from, &e.to)).collect();
        assert!(pairs.contains(&("gw".to_string(), "hostA".to_string())));
        assert!(pairs.contains(&("hostA".to_string(), "hostB".to_string())));
        assert_eq!(
            routers,
            std::collections::BTreeSet::from(["gw".to_string()])
        );
    }

    #[test]
    fn test_node_extracts_netlink_summary() {
        use std::collections::HashMap;
        use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

        let mk = |metric: &str, v: TelemetryValue| TelemetryPoint {
            timestamp: 0,
            source: "h".to_string(),
            protocol: Protocol::Netlink,
            metric: metric.to_string(),
            value: v,
            labels: HashMap::new(),
        };
        let mut m = HashMap::new();
        for (k, v) in [
            ("iface/eth0/up", TelemetryValue::Boolean(true)),
            ("iface/lo/up", TelemetryValue::Boolean(true)),
            ("iface/eth1/up", TelemetryValue::Boolean(false)),
            ("sockets/tcp/established", TelemetryValue::Gauge(120.0)),
            ("sockets/tcp/listen", TelemetryValue::Gauge(12.0)),
            ("routes/total", TelemetryValue::Gauge(20.0)),
            ("neighbors/total", TelemetryValue::Gauge(18.0)),
        ] {
            m.insert(k.to_string(), mk(k, v));
        }

        let mut node = Node {
            id: "h".to_string(),
            label: "h".to_string(),
            ..Default::default()
        };
        node.update_from_metrics(&m);

        assert_eq!(node.iface_up, Some(2));
        assert_eq!(node.iface_total, Some(3));
        assert_eq!(node.tcp_established, Some(120.0));
        assert_eq!(node.tcp_listen, Some(12.0));
        assert_eq!(node.routes_total, Some(20.0));
        assert_eq!(node.neighbors_total, Some(18.0));
    }

    #[test]
    fn edges_carry_their_kind() {
        let mut map = HashMap::new();
        map.insert("10.0.0.1".to_string(), "a".to_string());
        map.insert("10.0.0.2".to_string(), "b".to_string());
        let flows = vec![flow("10.0.0.1:1", "10.0.0.2:2", 10, 1, "tcp")];
        assert_eq!(edges_from_flows(&flows, &map, 0)[0].kind, EdgeKind::Flow);

        let hosts = vec!["a".to_string()];
        let (edges, _) = edges_from_neighbors(&hosts, &[neighbor("10.0.0.2", false)], &map, 0);
        assert_eq!(edges[0].kind, EdgeKind::L2Adjacency);
    }

    #[test]
    fn node_role_parses_asset_vocabulary() {
        assert_eq!(NodeRole::from_asset_role("router"), NodeRole::Router);
        assert_eq!(NodeRole::from_asset_role("switch"), NodeRole::Switch);
        assert_eq!(NodeRole::from_asset_role("ap"), NodeRole::AccessPoint);
        assert_eq!(NodeRole::from_asset_role("phone"), NodeRole::Phone);
        assert_eq!(NodeRole::from_asset_role("iot"), NodeRole::Iot);
        assert_eq!(NodeRole::from_asset_role("host"), NodeRole::Host);
        assert_eq!(NodeRole::from_asset_role("unknown"), NodeRole::Unknown);
        assert_eq!(NodeRole::from_asset_role("gibberish"), NodeRole::Unknown);
    }

    #[test]
    fn node_health_default_is_healthy() {
        let n = Node::default();
        assert_eq!(n.health, NodeHealth::Healthy);
        assert!(n.is_healthy());
        assert_eq!(n.role, NodeRole::Host);
        assert_eq!(n.provenance, Provenance::Monitored);
    }

    #[test]
    fn test_primary_protocol_prefers_sysinfo_then_netlink() {
        use zensight_common::Protocol;
        let mut n = Node::default();
        assert_eq!(primary_protocol(&n), Protocol::Sysinfo); // empty -> fallback
        n.protocols.insert(Protocol::Netlink);
        assert_eq!(primary_protocol(&n), Protocol::Netlink);
        n.protocols.insert(Protocol::Sysinfo);
        assert_eq!(primary_protocol(&n), Protocol::Sysinfo);
    }
}
