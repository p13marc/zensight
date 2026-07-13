//! Pure topology graph model: node/edge types and the derivation functions
//! that build them from observed sensor data (flows, neighbor tables).
//!
//! Everything in this module is pure given its inputs — no iced types beyond
//! what the data itself needs, no Zenoh, no app state — so the graph
//! derivation logic is unit-testable in isolation. Stateful orchestration
//! (caches, selection, layout) lives in [`super::TopologyState`].

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
    /// The host's identifying IPs (from its correlator entity, or the node id
    /// itself when it is an IP). Drives subnet grouping (#392).
    pub ips: Vec<String>,
    /// The host's identifying MACs (from its correlator entity). Shown by the
    /// L2 lens (#392).
    pub macs: Vec<String>,
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
                    // sysinfo network counters, via the registry (#475).
                    use zensight_keyspace::registry::sysinfo::Subject as SysSubject;
                    if matches!(
                        SysSubject::parse_metric(name),
                        Some(SysSubject::NetworkRxBytes { .. })
                    ) {
                        if let TelemetryValue::Counter(v) = &point.value {
                            self.network_rx = Some(*v);
                        }
                    } else if matches!(
                        SysSubject::parse_metric(name),
                        Some(SysSubject::NetworkTxBytes { .. })
                    ) && let TelemetryValue::Counter(v) = &point.value
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

/// Directed rate edges from the netring traffic matrix (#391). The matrix is
/// the primary edge source: pre-aggregated, top-N-capped server-side, and
/// carrying a rolling **bytes/sec** rate per `src → dst` pair. Rows whose
/// endpoints collapse to the same unordered node pair merge into one edge:
/// `from` is the heavier direction's source (lexicographic tiebreak on equal
/// rates), `rate` the forward and `reverse_rate` the responder bytes/sec.
/// Rows touching an unknown IP or a self-loop are dropped. Pure.
pub fn edges_from_matrix(
    matrix: &[zensight_common::MatrixRecord],
    ip_to_node: &HashMap<String, NodeId>,
    now_ms: i64,
) -> Vec<Edge> {
    // Per unordered pair: rate in pair-order direction, rate in reverse.
    let mut acc: HashMap<(NodeId, NodeId), (f64, f64)> = HashMap::new();
    for rec in matrix {
        let src_node = ip_to_node.get(endpoint_ip(&rec.src));
        let dst_node = ip_to_node.get(endpoint_ip(&rec.dst));
        let (Some(a), Some(b)) = (src_node, dst_node) else {
            continue;
        };
        if a == b {
            continue; // self-loop: same host both ends
        }
        let key = ordered_pair(a, b);
        let forward = *a == key.0; // does this row run in pair order?
        let entry = acc.entry(key).or_insert((0.0, 0.0));
        if forward {
            entry.0 += rec.bytes_per_sec;
        } else {
            entry.1 += rec.bytes_per_sec;
        }
    }
    let mut edges: Vec<Edge> = acc
        .into_iter()
        .map(|((a, b), (ab, ba))| {
            // `from` = the heavier direction's source; ties keep pair order.
            let (from, to, rate, reverse_rate) = if ab >= ba {
                (a, b, ab, ba)
            } else {
                (b, a, ba, ab)
            };
            Edge {
                from,
                to,
                kind: EdgeKind::Flow,
                rate,
                reverse_rate,
                last_seen: now_ms,
                ..Default::default()
            }
        })
        .collect();
    // Stable order: fastest links first, then by endpoints.
    edges.sort_by(|x, y| {
        let (rx, ry) = (x.rate + x.reverse_rate, y.rate + y.reverse_rate);
        ry.partial_cmp(&rx)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (x.from.clone(), x.to.clone()).cmp(&(y.from.clone(), y.to.clone())))
    });
    edges
}

/// Merge cumulative flow statistics into rate edges (#391): matrix edges gain
/// bytes/packets/protocol from the flow edge covering the same unordered pair,
/// and flow-only pairs (outside the matrix top-N) are appended so observed
/// links never vanish when the matrix arrives. Pure.
pub fn merge_flow_stats(mut rate_edges: Vec<Edge>, flow_edges: Vec<Edge>) -> Vec<Edge> {
    let mut by_pair: HashMap<(NodeId, NodeId), Edge> = flow_edges
        .into_iter()
        .map(|e| (ordered_pair(&e.from, &e.to), e))
        .collect();
    for edge in &mut rate_edges {
        if let Some(f) = by_pair.remove(&ordered_pair(&edge.from, &edge.to)) {
            edge.bytes = f.bytes;
            edge.packets = f.packets;
            edge.protocol = f.protocol;
        }
    }
    // Remaining flow edges cover pairs the matrix didn't (unrated).
    let mut rest: Vec<Edge> = by_pair.into_values().collect();
    rest.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| (a.from.clone(), a.to.clone()).cmp(&(b.from.clone(), b.to.clone())))
    });
    rate_edges.extend(rest);
    rate_edges
}

/// Extract a host's default IPv4 gateway from its netlink telemetry (#391):
/// the `routes/default_v4_gw` Text metric, honored only while
/// `routes/default_v4_present` is true (the sensor keeps publishing the last
/// gateway string across a flap). Pure.
pub fn gateway_from_metrics(
    metrics: &HashMap<String, zensight_common::TelemetryPoint>,
) -> Option<String> {
    use zensight_common::TelemetryValue;
    let present = matches!(
        metrics.get("routes/default_v4_present").map(|p| &p.value),
        Some(TelemetryValue::Boolean(true))
    );
    if !present {
        return None;
    }
    match metrics.get("routes/default_v4_gw").map(|p| &p.value) {
        Some(TelemetryValue::Text(gw)) if !gw.is_empty() => Some(gw.clone()),
        _ => None,
    }
}

/// Derive host → default-gateway edges (#391). The gateway resolves through
/// `ip_to_node` (an entity may own the address); unresolved gateway IPs are
/// returned so the caller can create wire-only router nodes for them, and
/// their edges target the bare IP as node id. Deterministic order. Pure.
pub fn edges_from_gateways(
    gateways: &HashMap<NodeId, String>,
    ip_to_node: &HashMap<String, NodeId>,
    now_ms: i64,
) -> (Vec<Edge>, Vec<String>) {
    use std::collections::BTreeSet;
    let mut missing: BTreeSet<String> = BTreeSet::new();
    let mut sorted: Vec<(&NodeId, &String)> = gateways.iter().collect();
    sorted.sort();
    let mut edges = Vec::new();
    for (host, gw_ip) in sorted {
        let target = match ip_to_node.get(gw_ip.as_str()) {
            Some(t) => t.clone(),
            None => {
                missing.insert(gw_ip.clone());
                gw_ip.clone()
            }
        };
        if &target == host {
            continue; // the host is its own gateway (or NATs for itself)
        }
        edges.push(Edge {
            from: host.clone(),
            to: target,
            kind: EdgeKind::Gateway,
            last_seen: now_ms,
            ..Default::default()
        });
    }
    (edges, missing.into_iter().collect())
}

/// Join the netring passive-asset inventory onto topology nodes (#391):
/// MAC-keyed assets resolve via `mac_to_node` (normalized MACs) first, then
/// via their IPv4/IPv6 addresses through `ip_to_node`. Returns per node the
/// asset role (possibly `Unknown` — the caller must not let that clobber
/// stronger evidence) and the vendor string. Deterministic (assets processed
/// in MAC order; first resolution wins per node, non-Unknown roles preferred).
/// Pure.
pub fn roles_from_assets(
    assets: &[zensight_common::AssetRecord],
    mac_to_node: &HashMap<String, NodeId>,
    ip_to_node: &HashMap<String, NodeId>,
) -> HashMap<NodeId, (NodeRole, Option<String>)> {
    let mut sorted: Vec<&zensight_common::AssetRecord> = assets.iter().collect();
    sorted.sort_by(|a, b| a.mac.cmp(&b.mac));
    let mut out: HashMap<NodeId, (NodeRole, Option<String>)> = HashMap::new();
    for asset in sorted {
        let node_id = mac_to_node
            .get(&crate::entity::normalize_mac(&asset.mac))
            .or_else(|| {
                asset
                    .ipv4
                    .iter()
                    .chain(asset.ipv6.iter())
                    .find_map(|ip| ip_to_node.get(ip.as_str()))
            });
        let Some(node_id) = node_id else { continue };
        let role = NodeRole::from_asset_role(&asset.role);
        let vendor = asset.vendor.clone();
        match out.entry(node_id.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((role, vendor));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let (cur_role, cur_vendor) = e.get_mut();
                if *cur_role == NodeRole::Unknown && role != NodeRole::Unknown {
                    *cur_role = role;
                }
                if cur_vendor.is_none() {
                    *cur_vendor = vendor;
                }
            }
        }
    }
    out
}

/// The synthetic node id aggregating off-LAN (public) traffic (#392): the
/// NDR-style north–south rollup. `@` keeps it out of any hostname/entity-id
/// namespace.
pub const INTERNET_NODE_ID: &str = "@internet";

/// Whether an IP string is a public (globally routable) address (#392).
/// Private/link-local/loopback/CGNAT/ULA/multicast/unspecified are all
/// non-public; unparseable strings are non-public (never aggregate garbage
/// into the Internet node). Pure.
pub fn is_public_ip(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                // CGNAT 100.64.0.0/10 — carrier-side, not global.
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                // fe80::/10 link-local, fc00::/7 unique-local.
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xfe00) == 0xfc00)
        }
        Err(_) => false,
    }
}

/// Aggregate matrix rows between a known node and an unmapped **public**
/// address into rated edges to the [`INTERNET_NODE_ID`] pseudo-node (#392) —
/// the NDR north–south rollup. Rows between two unmapped or two mapped
/// endpoints are not this function's business (see [`edges_from_matrix`]);
/// unmapped *private* endpoints stay dropped (unknown LAN noise, not
/// "the Internet"). Pure.
pub fn external_edges_from_matrix(
    matrix: &[zensight_common::MatrixRecord],
    ip_to_node: &HashMap<String, NodeId>,
    now_ms: i64,
) -> Vec<Edge> {
    // Per known node: (outbound to internet, inbound from internet) bytes/sec.
    let mut acc: HashMap<NodeId, (f64, f64)> = HashMap::new();
    for rec in matrix {
        let src_ip = endpoint_ip(&rec.src);
        let dst_ip = endpoint_ip(&rec.dst);
        match (ip_to_node.get(src_ip), ip_to_node.get(dst_ip)) {
            (Some(node), None) if is_public_ip(dst_ip) => {
                acc.entry(node.clone()).or_insert((0.0, 0.0)).0 += rec.bytes_per_sec;
            }
            (None, Some(node)) if is_public_ip(src_ip) => {
                acc.entry(node.clone()).or_insert((0.0, 0.0)).1 += rec.bytes_per_sec;
            }
            _ => {}
        }
    }
    let mut edges: Vec<Edge> = acc
        .into_iter()
        .map(|(node, (out_rate, in_rate))| {
            // `from` = the heavier direction's source, like edges_from_matrix.
            let (from, to, rate, reverse_rate) = if out_rate >= in_rate {
                (node, INTERNET_NODE_ID.to_string(), out_rate, in_rate)
            } else {
                (INTERNET_NODE_ID.to_string(), node, in_rate, out_rate)
            };
            Edge {
                from,
                to,
                kind: EdgeKind::Flow,
                rate,
                reverse_rate,
                last_seen: now_ms,
                ..Default::default()
            }
        })
        .collect();
    edges.sort_by(|x, y| {
        let (rx, ry) = (x.rate + x.reverse_rate, y.rate + y.reverse_rate);
        ry.partial_cmp(&rx)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (x.from.clone(), x.to.clone()).cmp(&(y.from.clone(), y.to.clone())))
    });
    edges
}

/// Which question the map is answering (#392): the Kiali-style lens. A lens
/// changes emphasis (edge kinds shown, node tint source, what labels say) —
/// never the underlying data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lens {
    /// Who talks to whom, how fast (default).
    #[default]
    Traffic,
    /// Alerts and anomalies; unknown/passive devices emphasized.
    Security,
    /// Physical structure: adjacency + gateways, MAC/vendor labels.
    L2,
    /// Sensor/liveness health tint.
    Health,
}

impl Lens {
    /// Every lens, in display order.
    pub const ALL: [Lens; 4] = [Lens::Traffic, Lens::Security, Lens::L2, Lens::Health];

    /// Button label.
    pub fn label(self) -> &'static str {
        match self {
            Lens::Traffic => "Traffic",
            Lens::Security => "Security",
            Lens::L2 => "L2",
            Lens::Health => "Health",
        }
    }

    /// The lens's presentation rules — data interpreted by one draw path,
    /// not four draw paths.
    pub fn spec(self) -> LensSpec {
        match self {
            Lens::Traffic => LensSpec {
                edge_kinds: &[EdgeKind::Flow, EdgeKind::L2Adjacency, EdgeKind::Gateway],
                tint: TintSource::Role,
                emphasize_passive: false,
                dim_unalerted: false,
                l2_labels: false,
            },
            Lens::Security => LensSpec {
                edge_kinds: &[EdgeKind::Flow, EdgeKind::L2Adjacency, EdgeKind::Gateway],
                tint: TintSource::Alert,
                emphasize_passive: true,
                dim_unalerted: true,
                l2_labels: false,
            },
            Lens::L2 => LensSpec {
                edge_kinds: &[EdgeKind::L2Adjacency, EdgeKind::Gateway],
                tint: TintSource::Role,
                emphasize_passive: false,
                dim_unalerted: false,
                l2_labels: true,
            },
            Lens::Health => LensSpec {
                edge_kinds: &[EdgeKind::Flow, EdgeKind::L2Adjacency, EdgeKind::Gateway],
                tint: TintSource::Health,
                emphasize_passive: false,
                dim_unalerted: false,
                l2_labels: false,
            },
        }
    }
}

/// A lens's presentation rules (#392).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LensSpec {
    /// Edge kinds this lens shows.
    pub edge_kinds: &'static [EdgeKind],
    /// What tints node fills.
    pub tint: TintSource,
    /// Wire-only nodes are the interesting ones (Security): don't dim them.
    pub emphasize_passive: bool,
    /// Dim nodes/edges with no firing alert (Security).
    pub dim_unalerted: bool,
    /// Label nodes with vendor/MAC (L2).
    pub l2_labels: bool,
}

/// What drives a node's fill color under a lens (#392).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TintSource {
    /// Role palette; a firing alert overrides the fill (default).
    Role,
    /// Alert severity; alert-less nodes render neutral.
    Alert,
    /// Health palette on the fill (not just the ring).
    Health,
}

/// What edge midpoint labels show (#392).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeLabelMode {
    /// Live rate when rated, cumulative bytes otherwise (default).
    #[default]
    Rate,
    Packets,
    Protocol,
    /// No edge labels.
    Hidden,
}

impl EdgeLabelMode {
    /// Every mode, in pick-list order.
    pub const ALL: [EdgeLabelMode; 4] = [
        EdgeLabelMode::Rate,
        EdgeLabelMode::Packets,
        EdgeLabelMode::Protocol,
        EdgeLabelMode::Hidden,
    ];
}

impl std::fmt::Display for EdgeLabelMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EdgeLabelMode::Rate => "Rate",
            EdgeLabelMode::Packets => "Packets",
            EdgeLabelMode::Protocol => "Protocol",
            EdgeLabelMode::Hidden => "None",
        })
    }
}

/// How nodes collapse into meta-nodes (#392).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupingMode {
    /// One node per host (default).
    #[default]
    None,
    /// Collapse by IPv4 /24 (via each node's identifying IPs).
    Subnet,
    /// Collapse by [`NodeRole`].
    Role,
    /// Collapse by the user's device groups (settings).
    DeviceGroup,
}

impl GroupingMode {
    /// Every mode, in pick-list order.
    pub const ALL: [GroupingMode; 4] = [
        GroupingMode::None,
        GroupingMode::Subnet,
        GroupingMode::Role,
        GroupingMode::DeviceGroup,
    ];
}

impl std::fmt::Display for GroupingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GroupingMode::None => "No grouping",
            GroupingMode::Subnet => "By subnet",
            GroupingMode::Role => "By role",
            GroupingMode::DeviceGroup => "By device group",
        })
    }
}

/// How nodes get their positions (#394).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutMode {
    /// Deterministic tiered hierarchy (default, #442): Internet → gateways/
    /// infrastructure → hosts banded by subnet → discovered devices. The
    /// layout every readable network map uses; positions never shuffle.
    #[default]
    Tiered,
    /// Force-directed: repulsion + springs, stepped at ~30 fps with alpha
    /// cooling while settling (#441). Organic, but positions are arbitrary.
    Force,
    /// Static grid ranked by alert severity then traffic (Grafana's
    /// "most interesting first" overview).
    Grid,
    /// Static circle (deterministic).
    Circular,
}

impl LayoutMode {
    /// Every mode, in pick-list order.
    pub const ALL: [LayoutMode; 4] = [
        LayoutMode::Tiered,
        LayoutMode::Force,
        LayoutMode::Grid,
        LayoutMode::Circular,
    ];
}

impl std::fmt::Display for LayoutMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LayoutMode::Tiered => "Tiered layout",
            LayoutMode::Force => "Force layout",
            LayoutMode::Grid => "Grid layout",
            LayoutMode::Circular => "Circular layout",
        })
    }
}

/// Edge/node visibility filters (#392).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TopoFilters {
    /// Hide wire-only (passive) nodes.
    #[serde(default)]
    pub hide_passive: bool,
    /// Hide edges not seen for [`IDLE_EDGE_MS`].
    #[serde(default)]
    pub hide_idle: bool,
    /// Hide the Internet external aggregate.
    #[serde(default)]
    pub hide_external: bool,
    /// Keep only the N fastest flow edges (structural L2/gateway edges are
    /// never truncated). `0` = unlimited.
    #[serde(default = "default_top_n")]
    pub top_n: usize,
}

fn default_top_n() -> usize {
    50
}

impl Default for TopoFilters {
    fn default() -> Self {
        Self {
            hide_passive: false,
            hide_idle: false,
            hide_external: false,
            top_n: default_top_n(),
        }
    }
}

/// Focus mode (#392): restrict the map to a node's N-hop neighborhood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusState {
    pub root: NodeId,
    pub hops: u8,
}

/// The topology view's presentation preferences (#392). `focus` and
/// `expanded_groups` are session-transient; the rest persists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopoPrefs {
    pub lens: Lens,
    pub edge_label: EdgeLabelMode,
    pub grouping: GroupingMode,
    pub filters: TopoFilters,
    pub layout: LayoutMode,
    pub focus: Option<FocusState>,
    pub expanded_groups: HashSet<String>,
}

/// An edge older than this with no live rate is "idle" (#392).
pub const IDLE_EDGE_MS: i64 = 5 * 60 * 1000;

/// A search-box action (#392): plain text (or `find:`) highlights matches,
/// `hide:` removes them.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchAction {
    None,
    Highlight(SearchPred),
    Hide(SearchPred),
}

/// A node predicate for find/hide (#392).
#[derive(Debug, Clone, PartialEq)]
pub enum SearchPred {
    /// Case-insensitive label substring.
    Text(String),
    /// `role:router` etc. (asset-role vocabulary + "internet"/"unknown").
    Role(NodeRole),
    /// `alert:critical|warning|info` or `alert:any`.
    Alert(Option<zensight_common::AlertSeverity>),
    /// `health:healthy|degraded|down|stale`.
    Health(NodeHealth),
}

impl SearchPred {
    /// Does `node` match this predicate?
    pub fn matches(&self, node: &Node) -> bool {
        match self {
            Self::Text(needle) => node.label.to_lowercase().contains(needle),
            Self::Role(role) => node.role == *role,
            Self::Alert(None) => node.alert.is_some(),
            Self::Alert(Some(sev)) => node.alert == Some(*sev),
            Self::Health(h) => node.health == *h,
        }
    }
}

/// Parse the search box (#392): `hide:<pred>` removes, `find:<pred>` (or bare
/// text) highlights. Predicates: `role:<r>`, `alert:<sev|any>`,
/// `health:<state>`, else label substring. Unparseable structured predicates
/// fall back to substring so typing never breaks. Pure.
pub fn parse_search(query: &str) -> SearchAction {
    let q = query.trim();
    if q.is_empty() {
        return SearchAction::None;
    }
    let (hide, rest) = match q.strip_prefix("hide:") {
        Some(rest) => (true, rest.trim()),
        None => (false, q.strip_prefix("find:").unwrap_or(q).trim()),
    };
    if rest.is_empty() {
        return SearchAction::None;
    }
    let pred = parse_pred(rest);
    if hide {
        SearchAction::Hide(pred)
    } else {
        SearchAction::Highlight(pred)
    }
}

fn parse_pred(s: &str) -> SearchPred {
    use zensight_common::AlertSeverity;
    let lower = s.to_lowercase();
    if let Some(role) = lower.strip_prefix("role:") {
        let role = match role.trim() {
            "internet" => NodeRole::Internet,
            "unknown" => NodeRole::Unknown,
            other => NodeRole::from_asset_role(other),
        };
        return SearchPred::Role(role);
    }
    if let Some(sev) = lower.strip_prefix("alert:") {
        return match sev.trim() {
            "critical" => SearchPred::Alert(Some(AlertSeverity::Critical)),
            "warning" => SearchPred::Alert(Some(AlertSeverity::Warning)),
            "info" => SearchPred::Alert(Some(AlertSeverity::Info)),
            _ => SearchPred::Alert(None), // "any" and friends
        };
    }
    if let Some(h) = lower.strip_prefix("health:") {
        return match h.trim() {
            "healthy" => SearchPred::Health(NodeHealth::Healthy),
            "degraded" => SearchPred::Health(NodeHealth::Degraded),
            "down" => SearchPred::Health(NodeHealth::Down),
            "stale" => SearchPred::Health(NodeHealth::Stale),
            other => SearchPred::Text(other.to_string()),
        };
    }
    SearchPred::Text(lower)
}

/// The N-hop undirected neighborhood of `root` over `edges` (#392). Always
/// contains `root`. Pure.
pub fn focus_neighborhood(edges: &[Edge], root: &NodeId, hops: u8) -> HashSet<NodeId> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges {
        adjacency.entry(&e.from).or_default().push(&e.to);
        adjacency.entry(&e.to).or_default().push(&e.from);
    }
    let mut seen: HashSet<NodeId> = HashSet::new();
    seen.insert(root.clone());
    let mut frontier: Vec<&str> = vec![root.as_str()];
    for _ in 0..hops {
        let mut next = Vec::new();
        for id in frontier {
            for neighbor in adjacency.get(id).into_iter().flatten() {
                if seen.insert((*neighbor).to_string()) {
                    next.push(*neighbor);
                }
            }
        }
        frontier = next;
    }
    seen
}

/// What a rendered element stands for (#392).
#[derive(Debug, Clone, PartialEq)]
pub enum RenderSource {
    /// A real topology node (selectable, draggable).
    Node(NodeId),
    /// A collapsed group meta-node (click to expand).
    Group(String),
}

/// A fully-resolved node ready to draw (#392). Positions are *not* cached
/// here — they resolve live via [`render_node_position`] so the 1 Hz layout
/// and drags don't force a render-graph rebuild.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderNode {
    pub source: RenderSource,
    pub label: String,
    pub role: NodeRole,
    pub provenance: Provenance,
    pub health: NodeHealth,
    pub alert: Option<zensight_common::AlertSeverity>,
    pub alert_count: usize,
    pub pinned: bool,
    pub dimmed: bool,
    pub highlighted: bool,
    /// What drives the fill color (per the active lens, #392).
    pub tint: TintSource,
    /// Members of a collapsed group (empty for plain nodes).
    pub members: Vec<NodeId>,
    pub cpu_usage: Option<f64>,
    pub memory_usage: Option<f64>,
    pub rx_rate: Option<f64>,
    pub tx_rate: Option<f64>,
}

/// A fully-resolved edge ready to draw (#392): endpoints are indices into
/// [`RenderGraph::nodes`].
#[derive(Debug, Clone, PartialEq)]
pub struct RenderEdge {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
    pub rate: f64,
    pub reverse_rate: f64,
    pub bytes: u64,
    pub packets: u64,
    pub protocol: Option<String>,
    pub alert: Option<zensight_common::AlertSeverity>,
    /// Index into the state's edge list when this renders exactly one edge
    /// (drives selection); `None` for group-aggregated edges.
    pub source_index: Option<usize>,
    pub dimmed: bool,
}

/// The resolved, filtered, (later: grouped and lens-tinted) graph the canvas
/// draws (#392). Rebuilt on structural/pref changes, not per frame — and
/// [`super::TopologyState::invalidate`] change-gates on `PartialEq` so an
/// identical rebuild never clears the canvas cache.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderGraph {
    pub nodes: Vec<RenderNode>,
    pub edges: Vec<RenderEdge>,
    /// Eligible flow-edge count before top-N truncation — drives the honest
    /// "showing top N of M" label.
    pub total_flow_edges: usize,
    /// Render key (node id / group id) → index into `nodes`.
    pub id_to_index: HashMap<String, usize>,
}

/// Resolve a rendered node's position: plain nodes read their backing node,
/// groups sit at their members' centroid. Pure.
pub fn render_node_position(rnode: &RenderNode, nodes: &HashMap<NodeId, Node>) -> (f32, f32) {
    match &rnode.source {
        RenderSource::Node(id) => nodes.get(id).map(|n| n.position).unwrap_or((0.0, 0.0)),
        RenderSource::Group(_) => {
            let mut x = 0.0;
            let mut y = 0.0;
            let mut count = 0.0;
            for id in &rnode.members {
                if let Some(n) = nodes.get(id) {
                    x += n.position.0;
                    y += n.position.1;
                    count += 1.0;
                }
            }
            if count > 0.0 {
                (x / count, y / count)
            } else {
                (0.0, 0.0)
            }
        }
    }
}

/// Worst-wins ordering for health bubbling (#392).
fn health_rank(h: NodeHealth) -> u8 {
    match h {
        NodeHealth::Healthy => 0,
        NodeHealth::Stale => 1,
        NodeHealth::Degraded => 2,
        NodeHealth::Down => 3,
    }
}

/// The /24 bucket of a node's first usable IPv4 (#392/#442), e.g.
/// `"192.168.1.0/24"`. Shared by subnet grouping and the tiered layout's
/// host banding. Pure.
pub fn subnet24(node: &Node) -> Option<String> {
    let v4 = node.ips.iter().find_map(|ip| {
        ip.parse::<std::net::Ipv4Addr>()
            .ok()
            .filter(|a| !a.is_loopback() && !a.is_link_local() && !a.is_unspecified())
    })?;
    let o = v4.octets();
    Some(format!("{}.{}.{}.0/24", o[0], o[1], o[2]))
}

/// The grouping bucket for a node under `mode` (#392). `None` = ungrouped
/// (renders as a plain node). The Internet aggregate is never grouped.
/// Group ids are namespaced (`group:<mode>:<key>`) so they can't collide with
/// entity ids or hostnames. Pure.
pub fn group_key(
    node: &Node,
    mode: GroupingMode,
    group_labels: &HashMap<NodeId, String>,
) -> Option<(String, String)> {
    if node.provenance == Provenance::External {
        return None;
    }
    match mode {
        GroupingMode::None => None,
        GroupingMode::Subnet => {
            let label = subnet24(node)?;
            Some((format!("group:subnet:{label}"), label))
        }
        GroupingMode::Role => {
            let label = node.role.label().to_string();
            Some((
                format!("group:role:{}", label.to_lowercase().replace(' ', "-")),
                label,
            ))
        }
        GroupingMode::DeviceGroup => {
            let label = group_labels.get(&node.id)?.clone();
            Some((format!("group:dg:{label}"), label))
        }
    }
}

/// Build the render graph (#392): apply focus, search find/hide, and
/// visibility filters over the model, then remap edges onto the surviving
/// nodes and truncate flow edges to the top N by rate (structural edges are
/// never truncated). Deterministic. Pure.
pub fn build_render_graph(
    nodes: &HashMap<NodeId, Node>,
    edges: &[Edge],
    prefs: &TopoPrefs,
    group_labels: &HashMap<NodeId, String>,
    search: &str,
    now_ms: i64,
) -> RenderGraph {
    let action = parse_search(search);
    let spec = prefs.lens.spec();
    let focus: Option<HashSet<NodeId>> = prefs
        .focus
        .as_ref()
        .map(|f| focus_neighborhood(edges, &f.root, f.hops));

    // Deterministic node order.
    let mut ids: Vec<&NodeId> = nodes.keys().collect();
    ids.sort();

    // Pass 1: visibility-filtered survivors.
    let mut survivors: Vec<&NodeId> = Vec::new();
    for id in ids {
        let node = &nodes[id];
        if prefs.filters.hide_passive && node.provenance == Provenance::Passive {
            continue;
        }
        if prefs.filters.hide_external && node.provenance == Provenance::External {
            continue;
        }
        if let Some(ref keep) = focus
            && !keep.contains(id.as_str())
        {
            continue;
        }
        if let SearchAction::Hide(ref pred) = action
            && pred.matches(node)
        {
            continue;
        }
        survivors.push(id);
    }

    // Pass 2: partition into collapsed groups (#392). Buckets need ≥ 2
    // members and must not be user-expanded; everything else stays plain.
    let mut bucket_members: HashMap<String, (String, Vec<&NodeId>)> = HashMap::new();
    let mut bucket_of: HashMap<&str, String> = HashMap::new();
    for id in &survivors {
        let node = &nodes[*id];
        if let Some((gid, label)) = group_key(node, prefs.grouping, group_labels)
            && !prefs.expanded_groups.contains(&gid)
        {
            bucket_members
                .entry(gid.clone())
                .or_insert_with(|| (label, Vec::new()))
                .1
                .push(id);
            bucket_of.insert(id.as_str(), gid);
        }
    }
    bucket_members.retain(|_, (_, members)| members.len() >= 2);
    bucket_of.retain(|_, gid| bucket_members.contains_key(gid));

    let mut out = RenderGraph::default();
    // Endpoint id → render index (plain nodes map to themselves, grouped
    // members to their meta-node).
    let mut endpoint_index: HashMap<&str, usize> = HashMap::new();

    // Plain nodes first, in sorted order.
    for id in &survivors {
        if bucket_of.contains_key(id.as_str()) {
            continue;
        }
        let node = &nodes[*id];
        let highlighted = matches!(action, SearchAction::Highlight(ref pred) if pred.matches(node));
        // Security lens (#392): alert-less monitored nodes fade; wire-only
        // unknowns are exactly what that lens is for.
        let dimmed = spec.dim_unalerted
            && node.alert.is_none()
            && !(spec.emphasize_passive && node.provenance == Provenance::Passive);
        // L2 lens labels lead with hardware identity (#392).
        let label = if spec.l2_labels {
            match (&node.vendor, node.macs.first()) {
                (Some(vendor), _) => format!("{} · {}", node.label, vendor),
                (None, Some(mac)) => format!("{} · {}", node.label, mac),
                (None, None) => node.label.clone(),
            }
        } else {
            node.label.clone()
        };
        let index = out.nodes.len();
        out.id_to_index.insert((*id).clone(), index);
        endpoint_index.insert(id.as_str(), index);
        out.nodes.push(RenderNode {
            source: RenderSource::Node((*id).clone()),
            label,
            role: node.role,
            provenance: node.provenance,
            health: node.health,
            alert: node.alert,
            alert_count: node.alerts.len(),
            pinned: node.pinned,
            dimmed,
            highlighted,
            tint: spec.tint,
            members: Vec::new(),
            cpu_usage: node.cpu_usage,
            memory_usage: node.memory_usage,
            rx_rate: node.rx_rate,
            tx_rate: node.tx_rate,
        });
    }

    // Then meta-nodes, in sorted group order: worst member health/alert
    // bubbles up; dim only when every member would dim; highlight when any
    // member matches.
    let mut group_ids: Vec<&String> = bucket_members.keys().collect();
    group_ids.sort();
    for gid in group_ids {
        let (label, members) = &bucket_members[gid];
        let mut health = NodeHealth::Healthy;
        let mut alert: Option<zensight_common::AlertSeverity> = None;
        let mut alert_count = 0;
        let mut any_highlight = false;
        let mut all_dim = true;
        let mut role = nodes[members[0]].role;
        for id in members {
            let node = &nodes[*id];
            if health_rank(node.health) > health_rank(health) {
                health = node.health;
            }
            alert = match (alert, node.alert) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            alert_count += node.alerts.len();
            any_highlight |=
                matches!(action, SearchAction::Highlight(ref pred) if pred.matches(node));
            all_dim &= spec.dim_unalerted
                && node.alert.is_none()
                && !(spec.emphasize_passive && node.provenance == Provenance::Passive);
            if node.role != role {
                role = NodeRole::Unknown; // mixed bucket (subnet/dg modes)
            }
        }
        let index = out.nodes.len();
        out.id_to_index.insert((*gid).clone(), index);
        for id in members {
            endpoint_index.insert(id.as_str(), index);
        }
        out.nodes.push(RenderNode {
            source: RenderSource::Group((*gid).clone()),
            label: label.clone(),
            role,
            provenance: Provenance::Monitored,
            health,
            alert,
            alert_count,
            pinned: false,
            dimmed: spec.dim_unalerted && all_dim,
            highlighted: any_highlight,
            tint: spec.tint,
            members: members.iter().map(|id| (*id).clone()).collect(),
            cpu_usage: None,
            memory_usage: None,
            rx_rate: None,
            tx_rate: None,
        });
    }

    // Edges over surviving endpoints; intra-group edges vanish; edges meeting
    // a meta-node aggregate per endpoint pair (worst alert, summed volume).
    let mut flow_edges: Vec<RenderEdge> = Vec::new();
    let mut aggregated: HashMap<(usize, usize, EdgeKind), RenderEdge> = HashMap::new();
    for (index, edge) in edges.iter().enumerate() {
        if !spec.edge_kinds.contains(&edge.kind) {
            continue;
        }
        let (Some(&from), Some(&to)) = (
            endpoint_index.get(edge.from.as_str()),
            endpoint_index.get(edge.to.as_str()),
        ) else {
            continue;
        };
        if from == to {
            continue; // intra-group traffic collapses away
        }
        if prefs.filters.hide_idle
            && edge.rate + edge.reverse_rate == 0.0
            && now_ms - edge.last_seen > IDLE_EDGE_MS
        {
            continue;
        }
        let dimmed = spec.dim_unalerted && edge.alert.is_none();
        let touches_group = matches!(out.nodes[from].source, RenderSource::Group(_))
            || matches!(out.nodes[to].source, RenderSource::Group(_));
        if touches_group {
            let key = (from.min(to), from.max(to), edge.kind);
            // Aggregate in canonical (min,max) orientation: swap the
            // per-direction rates when this edge runs against it.
            let (fwd, rev) = if from <= to {
                (edge.rate, edge.reverse_rate)
            } else {
                (edge.reverse_rate, edge.rate)
            };
            let entry = aggregated.entry(key).or_insert_with(|| RenderEdge {
                from: from.min(to),
                to: from.max(to),
                kind: edge.kind,
                rate: 0.0,
                reverse_rate: 0.0,
                bytes: 0,
                packets: 0,
                protocol: None,
                alert: None,
                source_index: Some(index),
                dimmed: true,
            });
            entry.rate += fwd;
            entry.reverse_rate += rev;
            entry.bytes += edge.bytes;
            entry.packets += edge.packets;
            entry.alert = match (entry.alert, edge.alert) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            entry.dimmed &= dimmed;
            if entry.source_index != Some(index) {
                entry.source_index = None; // >1 backing edge: no selection identity
            }
            continue;
        }
        let redge = RenderEdge {
            from,
            to,
            kind: edge.kind,
            rate: edge.rate,
            reverse_rate: edge.reverse_rate,
            bytes: edge.bytes,
            packets: edge.packets,
            protocol: edge.protocol.clone(),
            alert: edge.alert,
            source_index: Some(index),
            dimmed,
        };
        if edge.kind == EdgeKind::Flow {
            flow_edges.push(redge);
        } else {
            out.edges.push(redge);
        }
    }
    // Aggregated edges join their kind's pool (deterministic order).
    let mut agg: Vec<RenderEdge> = aggregated.into_values().collect();
    agg.sort_by_key(|e| (e.from, e.to, e.kind as u8));
    for redge in agg {
        if redge.kind == EdgeKind::Flow {
            flow_edges.push(redge);
        } else {
            out.edges.push(redge);
        }
    }
    out.total_flow_edges = flow_edges.len();
    // Fastest first; cumulative bytes break rate ties (unrated fallbacks).
    flow_edges.sort_by(|a, b| {
        let (ra, rb) = (a.rate + a.reverse_rate, b.rate + b.reverse_rate);
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.bytes.cmp(&a.bytes))
            .then_with(|| a.source_index.cmp(&b.source_index))
    });
    if prefs.filters.top_n > 0 {
        flow_edges.truncate(prefs.filters.top_n);
    }
    out.edges.extend(flow_edges);
    out
}

/// Compute a node's health (#391) from its facets' liveness, host-scoped
/// sensor health snapshots, and entity staleness. `facets` pairs each device
/// facet's liveness status with whether its telemetry is fresh. Precedence:
///
/// 1. **Down** — every facet's liveness says `Offline`.
/// 2. **Stale** — nothing fresh and no liveness verdict to the contrary: a
///    facet-less (passive) node whose entity went stale, or every facet
///    `Unknown` with quiet telemetry / a stale entity.
/// 3. **Degraded** — any facet `Offline`/`Degraded` or quiet, or any sensor
///    on the host reporting trouble (a sensor that publishes an Unhealthy
///    snapshot proves the host is up — sensor trouble degrades, never downs).
/// 4. **Healthy** — otherwise.
///
/// Pure.
pub fn node_health(
    facets: &[(zensight_common::DeviceStatus, bool)],
    sensor_statuses: &[zensight_common::HealthStatus],
    entity_stale: bool,
) -> NodeHealth {
    use zensight_common::{DeviceStatus, HealthStatus};

    let has_facets = !facets.is_empty();
    if has_facets && facets.iter().all(|(s, _)| *s == DeviceStatus::Offline) {
        return NodeHealth::Down;
    }

    let all_unknown = facets.iter().all(|(s, _)| *s == DeviceStatus::Unknown);
    let none_fresh = facets.iter().all(|(_, fresh)| !fresh);
    if (!has_facets && entity_stale) || (has_facets && all_unknown && (none_fresh || entity_stale))
    {
        return NodeHealth::Stale;
    }

    let facet_trouble = facets
        .iter()
        .any(|(s, fresh)| matches!(s, DeviceStatus::Offline | DeviceStatus::Degraded) || !fresh);
    let sensor_trouble = sensor_statuses.iter().any(|s| {
        matches!(
            s,
            HealthStatus::Degraded
                | HealthStatus::Unhealthy
                | HealthStatus::Error
                | HealthStatus::Starting
        )
    });
    if facet_trouble || sensor_trouble {
        return NodeHealth::Degraded;
    }
    NodeHealth::Healthy
}

/// Bytes/sec from the last two samples of a monotonic counter series (#391).
/// `None` on short series, non-advancing clocks, or counter resets (negative
/// delta) — a reset yields one missing reading, not a bogus spike. Pure.
pub fn counter_rate(samples: &[crate::store::Sample]) -> Option<f64> {
    let [.., prev, last] = samples else {
        return None;
    };
    let dt_ms = last.ts - prev.ts;
    if dt_ms <= 0 {
        return None;
    }
    let dv = last.value - prev.value;
    if dv < 0.0 {
        return None; // counter reset
    }
    Some(dv / (dt_ms as f64 / 1000.0))
}

/// Format a bytes/sec rate for display ("2.1 MB/s"). Pure.
pub fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bytes_per_sec / 1_000_000_000.0)
    } else if bytes_per_sec >= 1_000_000.0 {
        format!("{:.1} MB/s", bytes_per_sec / 1_000_000.0)
    } else if bytes_per_sec >= 1_000.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1_000.0)
    } else {
        format!("{bytes_per_sec:.0} B/s")
    }
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

    /// The sysinfo `network/{iface}/{rx,tx}_bytes` arms (#475). These feed the
    /// topology's per-node throughput, and they are the *only* consumer of that
    /// subject in the model — so if the registry pattern moves and this match arm
    /// does not, the map silently shows zero traffic on every node.
    ///
    /// The literal-headed siblings (`network/tcp/*`, `network/sockets/*`) must not
    /// be read as interfaces; a positional parse would have taken chunk 1 blindly.
    #[test]
    fn node_extracts_sysinfo_network_counters() {
        use std::collections::HashMap;
        use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

        let mk = |metric: &str, v: TelemetryValue| TelemetryPoint {
            timestamp: 0,
            source: "h".to_string(),
            protocol: Protocol::Sysinfo,
            metric: metric.to_string(),
            value: v,
            labels: HashMap::new(),
        };
        let mut m = HashMap::new();
        for (k, v) in [
            ("network/eth0/rx_bytes", TelemetryValue::Counter(1000)),
            ("network/eth0/tx_bytes", TelemetryValue::Counter(2000)),
            // Not an interface — a literal family that shares the `network/` head.
            (
                "network/tcp/retrans_segs_total",
                TelemetryValue::Counter(999_999),
            ),
        ] {
            m.insert(k.to_string(), mk(k, v));
        }

        let mut node = Node {
            id: "h".to_string(),
            label: "h".to_string(),
            ..Default::default()
        };
        node.update_from_metrics(&m);

        assert_eq!(node.network_rx, Some(1000));
        assert_eq!(node.network_tx, Some(2000));
    }

    fn matrix(src: &str, dst: &str, rate: f64) -> zensight_common::MatrixRecord {
        zensight_common::MatrixRecord {
            src: src.to_string(),
            dst: dst.to_string(),
            bytes_per_sec: rate,
            names: Vec::new(),
        }
    }

    #[test]
    fn edges_from_matrix_merges_directions() {
        let mut map = HashMap::new();
        map.insert("10.0.0.1".to_string(), "hostA".to_string());
        map.insert("10.0.0.2".to_string(), "hostB".to_string());
        let records = vec![
            matrix("10.0.0.1", "10.0.0.2", 500.0),
            matrix("10.0.0.2", "10.0.0.1", 2000.0), // heavier: B → A
            matrix("10.0.0.1", "8.8.8.8", 999.0),   // unknown dst -> dropped
            matrix("10.0.0.1", "10.0.0.1", 1.0),    // self-loop -> dropped
        ];
        let edges = edges_from_matrix(&records, &map, 42);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        // Forward = the heavier direction (B → A).
        assert_eq!((e.from.as_str(), e.to.as_str()), ("hostB", "hostA"));
        assert_eq!(e.rate, 2000.0);
        assert_eq!(e.reverse_rate, 500.0);
        assert_eq!(e.kind, EdgeKind::Flow);
        assert_eq!(e.last_seen, 42);
    }

    #[test]
    fn edges_from_matrix_sums_multiple_ips_per_node() {
        // Two source IPs of the same host aggregate into one direction.
        let mut map = HashMap::new();
        map.insert("10.0.0.1".to_string(), "hostA".to_string());
        map.insert("192.168.1.1".to_string(), "hostA".to_string());
        map.insert("10.0.0.2".to_string(), "hostB".to_string());
        let records = vec![
            matrix("10.0.0.1", "10.0.0.2", 100.0),
            matrix("192.168.1.1", "10.0.0.2", 50.0),
        ];
        let edges = edges_from_matrix(&records, &map, 0);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].rate, 150.0);
        assert_eq!(edges[0].reverse_rate, 0.0);
    }

    #[test]
    fn edges_from_matrix_tiebreak_keeps_pair_order() {
        let mut map = HashMap::new();
        map.insert("b-host".to_string(), "b-host".to_string());
        map.insert("a-host".to_string(), "a-host".to_string());
        let records = vec![
            matrix("b-host", "a-host", 100.0),
            matrix("a-host", "b-host", 100.0),
        ];
        let edges = edges_from_matrix(&records, &map, 0);
        // Equal rates: pair (lexicographic) order wins.
        assert_eq!(edges[0].from, "a-host");
        assert_eq!(edges[0].to, "b-host");
    }

    #[test]
    fn merge_flow_stats_enriches_and_appends() {
        let mut map = HashMap::new();
        for ip_host in [("1.1.1.1", "a"), ("2.2.2.2", "b"), ("3.3.3.3", "c")] {
            map.insert(ip_host.0.to_string(), ip_host.1.to_string());
        }
        let rate_edges = edges_from_matrix(&[matrix("1.1.1.1", "2.2.2.2", 100.0)], &map, 0);
        let flow_edges = edges_from_flows(
            &[
                flow("2.2.2.2:1", "1.1.1.1:2", 5000, 50, "tcp"), // covers the a<->b pair
                flow("1.1.1.1:1", "3.3.3.3:2", 700, 7, "udp"),   // a<->c: matrix missed it
            ],
            &map,
            0,
        );
        let merged = merge_flow_stats(rate_edges, flow_edges);
        assert_eq!(merged.len(), 2);
        // Rated edge enriched with the pair's cumulative flow stats.
        assert_eq!(merged[0].rate, 100.0);
        assert_eq!(merged[0].bytes, 5000);
        assert_eq!(merged[0].protocol.as_deref(), Some("tcp"));
        // Flow-only pair appended, unrated.
        assert_eq!(merged[1].bytes, 700);
        assert_eq!(merged[1].rate, 0.0);
    }

    #[test]
    fn gateway_from_metrics_needs_present_flag() {
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
        m.insert(
            "routes/default_v4_gw".to_string(),
            mk(
                "routes/default_v4_gw",
                TelemetryValue::Text("10.0.0.254".into()),
            ),
        );
        // Gateway string alone is not enough — the present flag gates it.
        assert_eq!(gateway_from_metrics(&m), None);
        m.insert(
            "routes/default_v4_present".to_string(),
            mk("routes/default_v4_present", TelemetryValue::Boolean(true)),
        );
        assert_eq!(gateway_from_metrics(&m), Some("10.0.0.254".to_string()));
        m.insert(
            "routes/default_v4_present".to_string(),
            mk("routes/default_v4_present", TelemetryValue::Boolean(false)),
        );
        assert_eq!(gateway_from_metrics(&m), None);
    }

    #[test]
    fn edges_from_gateways_resolves_and_reports_missing() {
        let mut gateways = HashMap::new();
        gateways.insert("hostA".to_string(), "10.0.0.254".to_string());
        gateways.insert("hostB".to_string(), "192.168.1.1".to_string());
        gateways.insert("gw-self".to_string(), "10.0.0.254".to_string());
        let mut map = HashMap::new();
        map.insert("10.0.0.254".to_string(), "gw-self".to_string()); // entity-owned
        let (edges, missing) = edges_from_gateways(&gateways, &map, 9);

        // hostA → resolved node; hostB → the bare IP (reported missing);
        // gw-self skipped (it is its own gateway).
        assert_eq!(edges.len(), 2);
        assert!(
            edges
                .iter()
                .all(|e| e.kind == EdgeKind::Gateway && e.last_seen == 9)
        );
        assert!(edges.iter().any(|e| e.from == "hostA" && e.to == "gw-self"));
        assert!(
            edges
                .iter()
                .any(|e| e.from == "hostB" && e.to == "192.168.1.1")
        );
        assert_eq!(missing, vec!["192.168.1.1".to_string()]);
    }

    #[test]
    fn roles_from_assets_joins_mac_then_ip() {
        use zensight_common::AssetRecord;
        let assets = vec![
            AssetRecord {
                mac: "AA:BB:CC:00:01:01".to_string(), // MAC join (case-insensitive)
                role: "router".to_string(),
                vendor: Some("Cisco".to_string()),
                ..Default::default()
            },
            AssetRecord {
                mac: "aa:bb:cc:00:02:02".to_string(), // unknown MAC → IP join
                ipv4: vec!["10.0.0.42".to_string()],
                role: "iot".to_string(),
                vendor: Some("Hikvision".to_string()),
                ..Default::default()
            },
            AssetRecord {
                mac: "aa:bb:cc:00:03:03".to_string(), // resolves nowhere → dropped
                ipv4: vec!["203.0.113.9".to_string()],
                role: "phone".to_string(),
                ..Default::default()
            },
        ];
        let mut mac_to_node = HashMap::new();
        mac_to_node.insert("aa:bb:cc:00:01:01".to_string(), "gw".to_string());
        let mut ip_to_node = HashMap::new();
        ip_to_node.insert("10.0.0.42".to_string(), "cam".to_string());

        let roles = roles_from_assets(&assets, &mac_to_node, &ip_to_node);
        assert_eq!(roles.len(), 2);
        assert_eq!(roles["gw"], (NodeRole::Router, Some("Cisco".to_string())));
        assert_eq!(roles["cam"], (NodeRole::Iot, Some("Hikvision".to_string())));
    }

    #[test]
    fn roles_from_assets_prefers_known_role_per_node() {
        use zensight_common::AssetRecord;
        // Two assets resolve to the same node; the unknown-role one sorts
        // first but must not shadow the router claim.
        let assets = vec![
            AssetRecord {
                mac: "aa:00:00:00:00:01".to_string(),
                ipv4: vec!["10.0.0.1".to_string()],
                role: "unknown".to_string(),
                ..Default::default()
            },
            AssetRecord {
                mac: "aa:00:00:00:00:02".to_string(),
                ipv4: vec!["10.0.0.1".to_string()],
                role: "router".to_string(),
                vendor: Some("MikroTik".to_string()),
                ..Default::default()
            },
        ];
        let mut ip_to_node = HashMap::new();
        ip_to_node.insert("10.0.0.1".to_string(), "gw".to_string());
        let roles = roles_from_assets(&assets, &HashMap::new(), &ip_to_node);
        assert_eq!(roles["gw"].0, NodeRole::Router);
        assert_eq!(roles["gw"].1, Some("MikroTik".to_string()));
    }

    #[test]
    fn node_health_precedence_rungs() {
        use zensight_common::{DeviceStatus as D, HealthStatus as H};

        // Down: all facets offline.
        assert_eq!(
            node_health(&[(D::Offline, false), (D::Offline, false)], &[], false),
            NodeHealth::Down
        );
        // Not Down while any facet is alive — degraded instead.
        assert_eq!(
            node_health(&[(D::Offline, false), (D::Online, true)], &[], false),
            NodeHealth::Degraded
        );
        // Stale: passive node (no facets) with a stale entity.
        assert_eq!(node_health(&[], &[], true), NodeHealth::Stale);
        // Stale: no liveness verdict and telemetry quiet.
        assert_eq!(
            node_health(&[(D::Unknown, false)], &[], false),
            NodeHealth::Stale
        );
        // Degraded: liveness fine but a host sensor reports trouble.
        assert_eq!(
            node_health(&[(D::Online, true)], &[H::Unhealthy], false),
            NodeHealth::Degraded
        );
        // Degraded: liveness Online but telemetry quiet.
        assert_eq!(
            node_health(&[(D::Online, false)], &[], false),
            NodeHealth::Degraded
        );
        // Healthy: fresh facets, quiet sensors, live entity.
        assert_eq!(
            node_health(&[(D::Online, true)], &[H::Healthy], false),
            NodeHealth::Healthy
        );
        // Passive node with a live entity is healthy, not stale.
        assert_eq!(node_health(&[], &[], false), NodeHealth::Healthy);
    }

    #[test]
    fn counter_rate_deltas_and_resets() {
        use crate::store::Sample;
        let s = |ts, value| Sample { ts, value };
        // 1000 bytes over 2 s → 500 B/s (uses the last two samples).
        assert_eq!(
            counter_rate(&[s(0, 0.0), s(1_000, 100.0), s(3_000, 1_100.0)]),
            Some(500.0)
        );
        // Counter reset → None, not a negative spike.
        assert_eq!(counter_rate(&[s(0, 5_000.0), s(1_000, 10.0)]), None);
        // Too short / non-advancing clock.
        assert_eq!(counter_rate(&[s(0, 1.0)]), None);
        assert_eq!(counter_rate(&[]), None);
        assert_eq!(counter_rate(&[s(5, 1.0), s(5, 2.0)]), None);
    }

    #[test]
    fn is_public_ip_classification() {
        for private in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "fe80::1",
            "fd00::1",
            "::1",
            "ff02::1",
            "not-an-ip",
        ] {
            assert!(!is_public_ip(private), "{private} should be non-public");
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "142.250.74.110",
            "2001:4860:4860::8888",
        ] {
            assert!(is_public_ip(public), "{public} should be public");
        }
    }

    #[test]
    fn external_edges_aggregate_public_only() {
        let mut map = HashMap::new();
        map.insert("10.0.0.11".to_string(), "server01".to_string());
        let records = vec![
            matrix("10.0.0.11", "142.250.74.110", 900.0), // out to public
            matrix("10.0.0.11", "1.1.1.1", 100.0),        // out to public
            matrix("8.8.8.8", "10.0.0.11", 5_000.0),      // in from public (heavier)
            matrix("10.0.0.11", "192.168.99.1", 777.0),   // unmapped PRIVATE -> dropped
            matrix("10.0.0.11", "10.0.0.12", 50.0),       // unmapped private -> dropped
        ];
        let edges = external_edges_from_matrix(&records, &map, 3);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        // Inbound outweighs outbound: internet -> server01.
        assert_eq!(e.from, INTERNET_NODE_ID);
        assert_eq!(e.to, "server01");
        assert_eq!(e.rate, 5_000.0);
        assert_eq!(e.reverse_rate, 1_000.0);
        assert_eq!(e.last_seen, 3);
    }

    #[test]
    fn parse_search_predicates() {
        use zensight_common::AlertSeverity;
        assert_eq!(parse_search(""), SearchAction::None);
        assert_eq!(parse_search("  "), SearchAction::None);
        assert_eq!(
            parse_search("web"),
            SearchAction::Highlight(SearchPred::Text("web".into()))
        );
        assert_eq!(
            parse_search("find:role:iot"),
            SearchAction::Highlight(SearchPred::Role(NodeRole::Iot))
        );
        assert_eq!(
            parse_search("hide:role:router"),
            SearchAction::Hide(SearchPred::Role(NodeRole::Router))
        );
        assert_eq!(
            parse_search("alert:critical"),
            SearchAction::Highlight(SearchPred::Alert(Some(AlertSeverity::Critical)))
        );
        assert_eq!(
            parse_search("alert:any"),
            SearchAction::Highlight(SearchPred::Alert(None))
        );
        assert_eq!(
            parse_search("health:stale"),
            SearchAction::Highlight(SearchPred::Health(NodeHealth::Stale))
        );
        // Unparseable structured predicate falls back to substring.
        assert_eq!(
            parse_search("health:gibberish"),
            SearchAction::Highlight(SearchPred::Text("gibberish".into()))
        );
    }

    #[test]
    fn focus_neighborhood_bfs_hops() {
        let edge = |a: &str, b: &str| Edge {
            from: a.to_string(),
            to: b.to_string(),
            ..Default::default()
        };
        // a - b - c - d chain plus isolated e.
        let edges = vec![edge("a", "b"), edge("b", "c"), edge("c", "d")];
        // Explicit HashSet<String> annotations: extra FromIterator/PartialEq
        // impls from optional-feature deps (smol_str/rkyv via the `h264`
        // feature) would otherwise make inference ambiguous.
        let hop1 = focus_neighborhood(&edges, &"b".to_string(), 1);
        assert_eq!(
            hop1,
            ["a", "b", "c"]
                .iter()
                .map(|s| s.to_string())
                .collect::<HashSet<String>>()
        );
        let hop2 = focus_neighborhood(&edges, &"a".to_string(), 2);
        assert_eq!(
            hop2,
            ["a", "b", "c"]
                .iter()
                .map(|s| s.to_string())
                .collect::<HashSet<String>>()
        );
        // Root always present, even with no edges.
        let lonely = focus_neighborhood(&[], &"e".to_string(), 3);
        assert_eq!(
            lonely,
            ["e".to_string()].into_iter().collect::<HashSet<String>>()
        );
    }

    fn render_fixture() -> (HashMap<NodeId, Node>, Vec<Edge>) {
        let mut nodes = HashMap::new();
        let mut add = |id: &str, provenance: Provenance, role: NodeRole| {
            nodes.insert(
                id.to_string(),
                Node {
                    id: id.to_string(),
                    label: id.to_string(),
                    provenance,
                    role,
                    ..Default::default()
                },
            );
        };
        add("alpha", Provenance::Monitored, NodeRole::Host);
        add("beta", Provenance::Monitored, NodeRole::Host);
        add("ghost", Provenance::Passive, NodeRole::Unknown);
        add(INTERNET_NODE_ID, Provenance::External, NodeRole::Internet);
        let mk_edge = |a: &str, b: &str, kind: EdgeKind, rate: f64, last_seen: i64| Edge {
            from: a.to_string(),
            to: b.to_string(),
            kind,
            rate,
            last_seen,
            ..Default::default()
        };
        let edges = vec![
            mk_edge("alpha", "beta", EdgeKind::Flow, 1000.0, 0),
            mk_edge("alpha", "ghost", EdgeKind::L2Adjacency, 0.0, 0),
            mk_edge("alpha", INTERNET_NODE_ID, EdgeKind::Flow, 50.0, 0),
        ];
        (nodes, edges)
    }

    #[test]
    fn render_graph_passthrough_at_defaults() {
        let (nodes, edges) = render_fixture();
        let render = build_render_graph(
            &nodes,
            &edges,
            &TopoPrefs::default(),
            &HashMap::new(),
            "",
            0,
        );
        assert_eq!(render.nodes.len(), 4);
        assert_eq!(render.edges.len(), 3);
        assert_eq!(render.total_flow_edges, 2);
        // Edge endpoints resolve through id_to_index.
        for e in &render.edges {
            assert!(e.from < render.nodes.len() && e.to < render.nodes.len());
            assert!(e.source_index.is_some());
        }
    }

    #[test]
    fn render_graph_filters_and_search() {
        let (nodes, edges) = render_fixture();

        // hide_passive drops ghost and its L2 edge.
        let prefs = TopoPrefs {
            filters: TopoFilters {
                hide_passive: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert!(!render.id_to_index.contains_key("ghost"));
        assert_eq!(render.edges.len(), 2);

        // hide_external drops the internet aggregate.
        let prefs = TopoPrefs {
            filters: TopoFilters {
                hide_external: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert!(!render.id_to_index.contains_key(INTERNET_NODE_ID));

        // hide: search removes; bare text highlights.
        let render = build_render_graph(
            &nodes,
            &edges,
            &TopoPrefs::default(),
            &HashMap::new(),
            "hide:alpha",
            0,
        );
        assert!(!render.id_to_index.contains_key("alpha"));
        assert!(render.edges.is_empty()); // every edge touched alpha
        let render = build_render_graph(
            &nodes,
            &edges,
            &TopoPrefs::default(),
            &HashMap::new(),
            "beta",
            0,
        );
        let beta = &render.nodes[render.id_to_index["beta"]];
        assert!(beta.highlighted);
        assert!(!render.nodes[render.id_to_index["alpha"]].highlighted);
    }

    #[test]
    fn render_graph_focus_idle_and_top_n() {
        let (nodes, mut edges) = render_fixture();
        // Focus on ghost, 1 hop: alpha + ghost only.
        let prefs = TopoPrefs {
            focus: Some(FocusState {
                root: "ghost".to_string(),
                hops: 1,
            }),
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert_eq!(render.nodes.len(), 2);
        assert!(render.id_to_index.contains_key("alpha"));
        assert!(render.id_to_index.contains_key("ghost"));

        // Idle filter: an unrated old flow edge disappears; structural L2
        // edges refresh their last_seen so they survive.
        edges.push(Edge {
            from: "alpha".to_string(),
            to: "beta".to_string(),
            kind: EdgeKind::Flow,
            bytes: 5,
            last_seen: 0,
            ..Default::default()
        });
        let prefs = TopoPrefs {
            filters: TopoFilters {
                hide_idle: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let now = IDLE_EDGE_MS + 1;
        // Rated edges keep last_seen=0 but have live rates -> kept.
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", now);
        assert!(
            render
                .edges
                .iter()
                .all(|e| e.rate > 0.0 || e.kind != EdgeKind::Flow)
        );

        // top_n = 1 keeps only the fastest flow edge but never the L2 edge.
        let prefs = TopoPrefs {
            filters: TopoFilters {
                top_n: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert_eq!(render.total_flow_edges, 3);
        let flows: Vec<_> = render
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Flow)
            .collect();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].rate, 1000.0);
        assert!(render.edges.iter().any(|e| e.kind == EdgeKind::L2Adjacency));
    }

    #[test]
    fn lenses_filter_edge_kinds_and_dim() {
        let (mut nodes, edges) = render_fixture();
        nodes.get_mut("ghost").unwrap().alert = None;
        nodes.get_mut("alpha").unwrap().alert = Some(zensight_common::AlertSeverity::Critical);

        // L2 lens: flow edges disappear, structural stay.
        let prefs = TopoPrefs {
            lens: Lens::L2,
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert!(render.edges.iter().all(|e| e.kind != EdgeKind::Flow));
        assert_eq!(render.edges.len(), 1);

        // Security lens: alert-less monitored nodes dim, passive ones don't,
        // alerted ones don't; tint source is Alert.
        let prefs = TopoPrefs {
            lens: Lens::Security,
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        let by = |id: &str| &render.nodes[render.id_to_index[id]];
        assert!(!by("alpha").dimmed); // has an alert
        assert!(by("beta").dimmed); // monitored, no alert
        assert!(!by("ghost").dimmed); // passive = emphasized
        assert_eq!(by("beta").tint, TintSource::Alert);

        // L2 labels lead with vendor when known.
        nodes.get_mut("beta").unwrap().vendor = Some("Dell".to_string());
        let prefs = TopoPrefs {
            lens: Lens::L2,
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert_eq!(
            render.nodes[render.id_to_index["beta"]].label,
            "beta · Dell"
        );
    }

    #[test]
    fn grouping_collapses_and_aggregates() {
        use zensight_common::AlertSeverity;
        let mut nodes = HashMap::new();
        let mut add = |id: &str, ip: &str, health: NodeHealth, alert: Option<AlertSeverity>| {
            nodes.insert(
                id.to_string(),
                Node {
                    id: id.to_string(),
                    label: id.to_string(),
                    ips: vec![ip.to_string()],
                    health,
                    alert,
                    ..Default::default()
                },
            );
        };
        // Two hosts in 10.0.2.0/24, one elsewhere.
        add("a1", "10.0.2.10", NodeHealth::Healthy, None);
        add(
            "a2",
            "10.0.2.20",
            NodeHealth::Down,
            Some(AlertSeverity::Warning),
        );
        add("b1", "10.0.9.5", NodeHealth::Healthy, None);
        let mk = |from: &str, to: &str, rate: f64| Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: EdgeKind::Flow,
            rate,
            ..Default::default()
        };
        // Intra-group + two group-external edges that must aggregate.
        let edges = vec![
            mk("a1", "a2", 999.0),
            mk("a1", "b1", 100.0),
            mk("a2", "b1", 50.0),
        ];
        let prefs = TopoPrefs {
            grouping: GroupingMode::Subnet,
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);

        // b1 plain + one meta-node.
        assert_eq!(render.nodes.len(), 2);
        let gid = "group:subnet:10.0.2.0/24";
        let group = &render.nodes[render.id_to_index[gid]];
        assert!(matches!(group.source, RenderSource::Group(_)));
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.label, "10.0.2.0/24");
        // Worst member health/alert bubbles up.
        assert_eq!(group.health, NodeHealth::Down);
        assert_eq!(group.alert, Some(AlertSeverity::Warning));

        // Intra-group edge vanished; the two external edges aggregated into
        // one with summed rate and no selection identity.
        assert_eq!(render.edges.len(), 1);
        let e = &render.edges[0];
        assert_eq!(e.rate + e.reverse_rate, 150.0);
        assert_eq!(e.source_index, None);

        // Expanding the group restores plain nodes.
        let prefs = TopoPrefs {
            grouping: GroupingMode::Subnet,
            expanded_groups: [gid.to_string()].into_iter().collect(),
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges, &prefs, &HashMap::new(), "", 0);
        assert_eq!(render.nodes.len(), 3);
        assert_eq!(render.edges.len(), 3);
    }

    #[test]
    fn grouping_singletons_stay_plain() {
        let mut nodes = HashMap::new();
        for (id, ip) in [("a", "10.0.1.1"), ("b", "10.0.2.1")] {
            nodes.insert(
                id.to_string(),
                Node {
                    id: id.to_string(),
                    label: id.to_string(),
                    ips: vec![ip.to_string()],
                    ..Default::default()
                },
            );
        }
        let prefs = TopoPrefs {
            grouping: GroupingMode::Subnet,
            ..Default::default()
        };
        let render = build_render_graph(&nodes, &edges_none(), &prefs, &HashMap::new(), "", 0);
        // Each subnet has one member -> no meta-nodes.
        assert_eq!(render.nodes.len(), 2);
        assert!(
            render
                .nodes
                .iter()
                .all(|n| matches!(n.source, RenderSource::Node(_)))
        );
    }

    fn edges_none() -> Vec<Edge> {
        Vec::new()
    }

    #[test]
    fn group_key_modes() {
        let node = |ips: &[&str], role: NodeRole| Node {
            id: "x".to_string(),
            ips: ips.iter().map(|s| s.to_string()).collect(),
            role,
            ..Default::default()
        };
        assert_eq!(
            group_key(
                &node(&["10.1.2.3"], NodeRole::Host),
                GroupingMode::Subnet,
                &HashMap::new()
            ),
            Some((
                "group:subnet:10.1.2.0/24".to_string(),
                "10.1.2.0/24".to_string()
            ))
        );
        // No v4 -> ungrouped under Subnet.
        assert_eq!(
            group_key(
                &node(&["fe80::1"], NodeRole::Host),
                GroupingMode::Subnet,
                &HashMap::new()
            ),
            None
        );
        assert_eq!(
            group_key(
                &node(&[], NodeRole::Iot),
                GroupingMode::Role,
                &HashMap::new()
            ),
            Some((
                "group:role:iot-device".to_string(),
                "IoT device".to_string()
            ))
        );
        let mut labels = HashMap::new();
        labels.insert("x".to_string(), "lab".to_string());
        assert_eq!(
            group_key(
                &node(&[], NodeRole::Host),
                GroupingMode::DeviceGroup,
                &labels
            ),
            Some(("group:dg:lab".to_string(), "lab".to_string()))
        );
        // External aggregate never groups.
        let mut internet = node(&[], NodeRole::Internet);
        internet.provenance = Provenance::External;
        assert_eq!(
            group_key(&internet, GroupingMode::Role, &HashMap::new()),
            None
        );
    }

    #[test]
    fn render_node_position_group_centroid() {
        let (mut nodes, _) = render_fixture();
        nodes.get_mut("alpha").unwrap().position = (0.0, 0.0);
        nodes.get_mut("beta").unwrap().position = (100.0, 40.0);
        let group = RenderNode {
            source: RenderSource::Group("group:test".to_string()),
            label: "test".to_string(),
            role: NodeRole::Unknown,
            provenance: Provenance::Monitored,
            health: NodeHealth::Healthy,
            alert: None,
            alert_count: 0,
            pinned: false,
            dimmed: false,
            highlighted: false,
            tint: TintSource::Role,
            members: vec!["alpha".to_string(), "beta".to_string()],
            cpu_usage: None,
            memory_usage: None,
            rx_rate: None,
            tx_rate: None,
        };
        assert_eq!(render_node_position(&group, &nodes), (50.0, 20.0));
        let plain = RenderNode {
            source: RenderSource::Node("beta".to_string()),
            members: Vec::new(),
            ..group
        };
        assert_eq!(render_node_position(&plain, &nodes), (100.0, 40.0));
    }

    #[test]
    fn format_rate_scales_units() {
        assert_eq!(format_rate(500.0), "500 B/s");
        assert_eq!(format_rate(1_500.0), "1.5 KB/s");
        assert_eq!(format_rate(2_100_000.0), "2.1 MB/s");
        assert_eq!(format_rate(1_500_000_000.0), "1.5 GB/s");
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
