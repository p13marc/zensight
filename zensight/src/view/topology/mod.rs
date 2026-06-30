//! Network topology visualization.
//!
//! Displays interconnections between VMs/hosts as an interactive graph,
//! showing network bandwidth between each link.

pub mod graph;
pub mod layout;

use std::collections::HashMap;

use iced::widget::canvas::Cache;
use iced::widget::{column, container, row, text, text_input};
use iced::{Alignment, Element, Length};
use iced_anim::widget::button;

use crate::app::AppTheme;
use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::icons::{self, IconSize};

pub use graph::TopologyGraph;
pub use layout::{LayoutConfig, arrange_circle, center_layout, layout_step};

/// Unique identifier for a topology node.
pub type NodeId = String;

/// State for the topology view.
#[derive(Debug)]
pub struct TopologyState {
    /// Graph nodes (devices/hosts).
    pub nodes: HashMap<NodeId, Node>,
    /// Graph edges (connections with bandwidth).
    pub edges: Vec<Edge>,
    /// Selected node (if any).
    pub selected_node: Option<NodeId>,
    /// Selected edge index (if any).
    pub selected_edge: Option<usize>,
    /// View zoom level (1.0 = 100%).
    pub zoom: f32,
    /// View pan offset (x, y).
    pub pan: (f32, f32),
    /// Whether auto-layout is enabled.
    pub auto_layout: bool,
    /// Rendering cache.
    pub cache: Cache,
    /// Search query for highlighting nodes.
    pub search_query: String,
    /// Layout algorithm configuration.
    pub layout_config: LayoutConfig,
    /// Whether the layout is currently stable.
    pub layout_stable: bool,
    /// Last netring flows fetched, kept so the edge set can be rebuilt when the
    /// netlink neighbor table arrives separately (#49).
    last_flows: Vec<zensight_common::FlowRecord>,
    /// Last netlink neighbor (ARP/NDP) table fetched, merged into the edge set
    /// as adjacency links (#49).
    last_neighbors: Vec<zensight_common::NeighborRecord>,
}

impl Default for TopologyState {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
            selected_node: None,
            selected_edge: None,
            zoom: 1.0,
            pan: (0.0, 0.0),
            auto_layout: true,
            cache: Cache::new(),
            search_query: String::new(),
            layout_config: LayoutConfig::default(),
            layout_stable: true,
            last_flows: Vec::new(),
            last_neighbors: Vec::new(),
        }
    }
}

impl TopologyState {
    /// Update topology from dashboard device states.
    pub fn update_from_devices(&mut self, devices: &HashMap<DeviceId, DeviceState>) {
        let initial_count = self.nodes.len();

        // Recompute the per-host metric tally from scratch each pass (#83): nodes
        // persist across calls and a host has one facet per protocol, so zero
        // first, then accumulate below.
        for node in self.nodes.values_mut() {
            node.metric_count = 0;
        }

        // A node per physical host/device, merged by source. Widened beyond
        // sysinfo/netlink (#83) so netflow exporters and gNMI/SNMP/Modbus gear
        // also appear; syslog/netring are overlays (logs / flow edges), not nodes.
        for (device_id, device_state) in devices {
            if !is_node_protocol(device_id.protocol) {
                continue;
            }

            let node_id = device_id.source.clone();

            if !self.nodes.contains_key(&node_id) {
                // Create new node - position will be set by arrange_in_circle
                self.nodes.insert(
                    node_id.clone(),
                    Node {
                        id: node_id.clone(),
                        label: device_id.source.clone(),
                        is_healthy: device_state.is_healthy,
                        ..Default::default()
                    },
                );
            }

            // Update node metrics from telemetry
            if let Some(node) = self.nodes.get_mut(&node_id) {
                node.is_healthy = device_state.is_healthy;
                node.protocols.insert(device_id.protocol);
                node.metric_count += device_state.metric_count;
                node.update_from_metrics(&device_state.metrics);
            }
        }

        // If new nodes were added, arrange in circle and trigger layout
        if self.nodes.len() > initial_count {
            self.arrange_in_circle(400.0);
            self.layout_stable = false;
            self.cache.clear();
        }

        // NB: edges are derived from *observed* flow/neighbor data via
        // `apply_flow_edges` (#25), not fabricated here. We no longer synthesize a
        // demo mesh between active nodes.
    }

    /// Overlay firing sensor alerts onto nodes: a node whose `source` matches a
    /// firing alert is tinted by the highest severity seen for that host.
    pub fn apply_alerts(&mut self, external: &HashMap<String, zensight_common::Alert>) {
        for node in self.nodes.values_mut() {
            node.alert = None;
            node.alerts.clear();
        }
        for alert in external.values() {
            if let Some(node) = self.nodes.get_mut(&alert.source) {
                node.alert = Some(match node.alert {
                    Some(cur) => cur.max(alert.severity),
                    None => alert.severity,
                });
                // Keep the per-host alert list for the info panel (#83).
                node.alerts.push(NodeAlert {
                    severity: alert.severity,
                    rule: alert.rule.clone(),
                    summary: alert.summary.clone(),
                });
            }
        }
        // Highest severity first, so the panel leads with the worst.
        for node in self.nodes.values_mut() {
            node.alerts
                .sort_by(|a, b| b.severity.cmp(&a.severity).then(a.rule.cmp(&b.rule)));
        }
        // Per-link health (#49): tint each edge by the worst of its endpoints.
        self.recompute_edge_health();
        self.cache.clear();
    }

    /// Annotate nodes with how many sensors have correlated each host (#25),
    /// from the cross-sensor correlation map. Matches a node by id appearing in a
    /// correlation entry's hostnames; aggregates the max sensor count seen.
    pub fn apply_correlations(
        &mut self,
        correlations: &HashMap<String, zensight_common::CorrelationEntry>,
    ) {
        for node in self.nodes.values_mut() {
            node.sensor_count = None;
        }
        for entry in correlations.values() {
            let count = entry.sensors.len();
            for host in &entry.hostnames {
                if let Some(node) = self.nodes.get_mut(host) {
                    node.sensor_count = Some(node.sensor_count.map_or(count, |c| c.max(count)));
                }
            }
        }
        self.cache.clear();
    }

    /// Replace the edge set with edges derived from *observed* netring flow
    /// records (#25). `ip_to_node` maps an endpoint IP to a topology node id
    /// (built from correlations / node sources). Flows whose src and dst both
    /// resolve to (distinct) known nodes are aggregated into one edge per
    /// unordered node pair, summing bytes/packets. Pure given its inputs; this
    /// is the Hubble model — topology from live flow data, not config.
    pub fn apply_flow_edges(
        &mut self,
        flows: &[zensight_common::FlowRecord],
        ip_to_node: &HashMap<String, NodeId>,
        now_ms: i64,
    ) {
        self.last_flows = flows.to_vec();
        self.rebuild_edges(ip_to_node, now_ms);
    }

    /// Merge the netlink neighbor (ARP/NDP) table into the topology (#49):
    /// remembers it and rebuilds the edge set so direct L2/L3 adjacencies appear
    /// as links even when netring sees no traffic, and `is_router` neighbors are
    /// classified as [`NodeType::Router`].
    pub fn apply_neighbor_edges(
        &mut self,
        neighbors: &[zensight_common::NeighborRecord],
        ip_to_node: &HashMap<String, NodeId>,
        now_ms: i64,
    ) {
        self.last_neighbors = neighbors.to_vec();
        self.rebuild_edges(ip_to_node, now_ms);
    }

    /// Rebuild the edge set from the remembered flow + neighbor inputs (#25/#49).
    /// Flow edges (with real bandwidth) take precedence; neighbor adjacencies add
    /// zero-bandwidth links for node pairs no flow covered. Router classification
    /// from `is_router` neighbors is reset and reapplied each pass so it tracks
    /// the live table. Pure given its remembered inputs + `ip_to_node`.
    fn rebuild_edges(&mut self, ip_to_node: &HashMap<String, NodeId>, now_ms: i64) {
        use std::collections::BTreeSet;
        use zensight_common::Protocol;

        let mut edges = edges_from_flows(&self.last_flows, ip_to_node, now_ms);
        let mut pairs: BTreeSet<(NodeId, NodeId)> =
            edges.iter().map(|e| ordered_pair(&e.from, &e.to)).collect();

        // Neighbor tables belong to the netlink host(s); the app treats the
        // netlink detail queryable as the local sensor (a single global key),
        // so attribute the table to every netlink node present.
        let host_nodes: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.protocols.contains(&Protocol::Netlink))
            .map(|(id, _)| id.clone())
            .collect();
        let (neighbor_edges, routers) =
            edges_from_neighbors(&host_nodes, &self.last_neighbors, ip_to_node, now_ms);
        for edge in neighbor_edges {
            if pairs.insert(ordered_pair(&edge.from, &edge.to)) {
                edges.push(edge);
            }
        }

        // Reset then reapply Router classification so it follows the live table.
        for node in self.nodes.values_mut() {
            node.node_type = NodeType::Host;
        }
        for id in &routers {
            if let Some(node) = self.nodes.get_mut(id) {
                node.node_type = NodeType::Router;
            }
        }

        self.edges = edges;
        self.selected_edge = None;
        self.recompute_edge_health();
        self.cache.clear();
    }

    /// Tint each edge by the worst alert severity of its two endpoint nodes
    /// (#49). Idempotent; safe to call after either node alerts or edges change.
    fn recompute_edge_health(&mut self) {
        for edge in &mut self.edges {
            let from = self.nodes.get(&edge.from).and_then(|n| n.alert);
            let to = self.nodes.get(&edge.to).and_then(|n| n.alert);
            edge.alert = match (from, to) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            };
        }
    }

    /// Select a node by ID.
    pub fn select_node(&mut self, node_id: NodeId) {
        self.selected_node = Some(node_id);
        self.selected_edge = None;
        self.cache.clear();
    }

    /// Select an edge by index.
    pub fn select_edge(&mut self, edge_index: usize) {
        self.selected_edge = Some(edge_index);
        self.selected_node = None;
        self.cache.clear();
    }

    /// Clear selection.
    pub fn clear_selection(&mut self) {
        self.selected_node = None;
        self.selected_edge = None;
        self.cache.clear();
    }

    /// Zoom in.
    pub fn zoom_in(&mut self) {
        self.zoom = (self.zoom * 1.2).min(3.0);
        self.cache.clear();
    }

    /// Zoom out.
    pub fn zoom_out(&mut self) {
        self.zoom = (self.zoom / 1.2).max(0.3);
        self.cache.clear();
    }

    /// Reset zoom to 100%.
    pub fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.cache.clear();
    }

    /// Start dragging a node.
    pub fn start_node_drag(&mut self, node_id: &NodeId) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.pinned = true;
        }
    }

    /// Update node position during drag.
    pub fn update_node_drag(&mut self, node_id: &NodeId, x: f32, y: f32) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.position = (x, y);
            node.velocity = (0.0, 0.0);
            self.cache.clear();
        }
    }

    /// End node drag.
    pub fn end_node_drag(&mut self, _node_id: &NodeId) {
        // Node stays pinned after drag
    }

    /// Update pan offset.
    pub fn update_pan(&mut self, dx: f32, dy: f32) {
        self.pan.0 += dx;
        self.pan.1 += dy;
        self.cache.clear();
    }

    /// Set search query.
    pub fn set_search(&mut self, query: String) {
        self.search_query = query;
        self.cache.clear();
    }

    /// Toggle auto-layout.
    pub fn toggle_auto_layout(&mut self) {
        self.auto_layout = !self.auto_layout;
        if self.auto_layout {
            // Reset layout stability when re-enabling
            self.layout_stable = false;
        }
    }

    /// Run layout iterations for smooth convergence.
    /// Returns true if the layout is stable.
    pub fn run_layout_step(&mut self) -> bool {
        // Run 3 iterations per frame - balance between speed and smoothness
        for _ in 0..3 {
            self.layout_stable = layout_step(self, &self.layout_config.clone());
            if self.layout_stable {
                break;
            }
        }
        self.layout_stable
    }

    /// Arrange nodes in a circle (useful for initial layout).
    pub fn arrange_in_circle(&mut self, radius: f32) {
        arrange_circle(self, radius);
        self.layout_stable = false;
    }

    /// Center the layout around the origin.
    pub fn center(&mut self) {
        center_layout(self);
    }

    /// Get the DeviceId for a node (if it corresponds to a device). Uses the
    /// node's primary protocol so "View Device Details" lands on a real device
    /// even for netlink-only hosts (#83).
    pub fn node_to_device_id(&self, node_id: &NodeId) -> Option<DeviceId> {
        self.nodes.get(node_id).map(|node| DeviceId {
            protocol: primary_protocol(node),
            source: node_id.clone(),
        })
    }
}

/// A firing sensor alert attached to a node, for the info panel (#83).
#[derive(Debug, Clone)]
pub struct NodeAlert {
    pub severity: zensight_common::AlertSeverity,
    pub rule: String,
    pub summary: String,
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
    /// Type of node.
    pub node_type: NodeType,
    /// Which protocols' devices map to this host (#83). Drives the header icon
    /// and the "covered by" badges in the info panel.
    pub protocols: std::collections::BTreeSet<zensight_common::Protocol>,
    /// CPU usage percentage (0-100). From sysinfo.
    pub cpu_usage: Option<f64>,
    /// Memory usage percentage (0-100). From sysinfo.
    pub memory_usage: Option<f64>,
    /// Network RX bytes/sec. From sysinfo.
    pub network_rx: Option<u64>,
    /// Network TX bytes/sec. From sysinfo.
    pub network_tx: Option<u64>,
    /// Interfaces up / total, from netlink `iface/<n>/up` (#83).
    pub iface_up: Option<u32>,
    pub iface_total: Option<u32>,
    /// TCP socket-state gauges, from netlink `sockets/tcp/*` (#83).
    pub tcp_established: Option<f64>,
    pub tcp_listen: Option<f64>,
    /// Route / neighbor table sizes, from netlink (#83).
    pub routes_total: Option<f64>,
    pub neighbors_total: Option<f64>,
    /// Whether the node is healthy.
    pub is_healthy: bool,
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

/// Type of topology node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeType {
    /// A host/VM.
    #[default]
    Host,
    /// A network router.
    Router,
    /// A network switch.
    Switch,
    /// Unknown device type.
    Unknown,
}

/// An edge (connection) in the topology graph.
#[derive(Debug, Clone)]
pub struct Edge {
    /// Source node ID.
    pub from: NodeId,
    /// Destination node ID.
    pub to: NodeId,
    /// Bytes transferred.
    pub bytes: u64,
    /// Packets transferred.
    pub packets: u64,
    /// Protocol (TCP, UDP, etc.).
    pub protocol: Option<String>,
    /// Last seen timestamp.
    pub last_seen: i64,
    /// Per-link health (#49): the max alert severity of the two endpoint nodes,
    /// so a link to/from a host in trouble is visually flagged. Set by
    /// [`TopologyState::apply_alerts`].
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
            bytes,
            packets,
            protocol: Some(protocol),
            last_seen: now_ms,
            alert: None,
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
fn ordered_pair(a: &NodeId, b: &NodeId) -> (NodeId, NodeId) {
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
/// node ids to classify [`NodeType::Router`]. Pure — the unit of testing.
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
            bytes: 0,
            packets: 0,
            protocol: None,
            last_seen: now_ms,
            alert: None,
        })
        .collect();
    (edges, routers)
}

/// Render the topology view.
pub fn topology_view<'a>(state: &'a TopologyState, theme: AppTheme) -> Element<'a, Message> {
    let is_dark = matches!(theme, AppTheme::Dark);
    let header = render_header(state);
    let graph = TopologyGraph::view(state, is_dark);

    // Show the node panel, or the edge detail panel (#25), beside the graph.
    let main_content: Element<'a, Message> = if let Some(ref node_id) = state.selected_node {
        if let Some(node) = state.nodes.get(node_id) {
            let panel = render_node_info_panel(node);
            row![graph, panel].spacing(10).into()
        } else {
            graph
        }
    } else if let Some(edge) = state.selected_edge.and_then(|i| state.edges.get(i)) {
        let panel = render_edge_info_panel(edge);
        row![graph, panel].spacing(10).into()
    } else {
        graph
    };

    let content = column![header, main_content].spacing(10).padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Generate a simple text-based progress bar.
fn progress_bar(percentage: f64, width: usize) -> String {
    let filled = ((percentage / 100.0) * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "=".repeat(filled), " ".repeat(empty))
}

/// Pick the icon protocol for a node: prefer sysinfo (the host identity), then
/// netlink, otherwise the first protocol that covers the host (#83).
/// Whether a protocol's `source` represents a physical host/device that should be
/// a topology node (#83). sysinfo/netlink hosts, netflow exporters, and
/// gNMI/SNMP/Modbus network gear are nodes; syslog (log overlay) and netring (flow
/// overlay that supplies the *edges*) annotate existing nodes rather than adding
/// their own.
fn is_node_protocol(p: zensight_common::Protocol) -> bool {
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

fn primary_protocol(node: &Node) -> zensight_common::Protocol {
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

/// Render the node info panel (shown when a node is selected).
fn render_node_info_panel(node: &Node) -> Element<'_, Message> {
    use iced::widget::rule;

    // Header with a protocol-aware icon and name (#83).
    let header = row![
        icons::protocol_icon(primary_protocol(node), IconSize::Large),
        column![
            text(&node.label).size(16),
            text(format!("{:?}", node.node_type)).size(10),
        ]
        .spacing(2)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Status indicator
    let status = if node.is_healthy {
        row![
            icons::status_healthy(IconSize::Small),
            text("Healthy - receiving data").size(11)
        ]
        .spacing(5)
        .align_y(Alignment::Center)
    } else {
        row![
            icons::status_warning(IconSize::Small),
            text("Stale - no recent data").size(11)
        ]
        .spacing(5)
        .align_y(Alignment::Center)
    };

    let mut info_items = column![header, status, rule::horizontal(1)].spacing(8);

    // Cross-sensor correlation: "seen by N sensors" (#25).
    if let Some(n) = node.sensor_count {
        info_items = info_items.push(text(format!("Seen by {n} sensor(s)")).size(11));
    }

    // Which protocols cover this host (#83).
    if !node.protocols.is_empty() {
        let names: Vec<String> = node
            .protocols
            .iter()
            .map(|p| format!("{p:?}").to_lowercase())
            .collect();
        info_items = info_items.push(text(format!("Covered by: {}", names.join(" · "))).size(11));
    }

    // Protocol-agnostic signal (#83): how many metrics this host is tracked by.
    // Keeps netflow/snmp/modbus/gnmi nodes — which have no dedicated section
    // below — from showing a near-empty panel.
    if node.metric_count > 0 {
        info_items =
            info_items.push(text(format!("Metrics tracked: {}", node.metric_count)).size(11));
    }

    // System resources section
    let has_system_metrics = node.cpu_usage.is_some() || node.memory_usage.is_some();
    if has_system_metrics {
        info_items = info_items.push(text("System Resources").size(12));

        if let Some(cpu) = node.cpu_usage {
            let cpu_bar = format!("CPU: {:.1}% {}", cpu, progress_bar(cpu, 20));
            info_items = info_items.push(text(cpu_bar).size(11));
        }

        if let Some(mem) = node.memory_usage {
            let mem_bar = format!("Mem: {:.1}% {}", mem, progress_bar(mem, 20));
            info_items = info_items.push(text(mem_bar).size(11));
        }
    }

    // Network section
    let has_network = node.network_rx.is_some() || node.network_tx.is_some();
    if has_network {
        info_items = info_items.push(rule::horizontal(1));
        info_items = info_items.push(text("Network I/O").size(12));

        if let Some(rx) = node.network_rx {
            info_items =
                info_items.push(text(format!("  RX: {}", graph::format_bytes(rx))).size(11));
        }
        if let Some(tx) = node.network_tx {
            info_items =
                info_items.push(text(format!("  TX: {}", graph::format_bytes(tx))).size(11));
        }
        // Total
        let total = node.network_rx.unwrap_or(0) + node.network_tx.unwrap_or(0);
        if total > 0 {
            info_items =
                info_items.push(text(format!("  Total: {}", graph::format_bytes(total))).size(11));
        }
    }

    // Netlink section: kernel networking summary (#83).
    let has_netlink = node.iface_total.is_some()
        || node.tcp_established.is_some()
        || node.tcp_listen.is_some()
        || node.routes_total.is_some()
        || node.neighbors_total.is_some();
    if has_netlink {
        info_items = info_items.push(rule::horizontal(1));
        info_items = info_items.push(text("Kernel Networking").size(12));

        if let (Some(up), Some(total)) = (node.iface_up, node.iface_total) {
            info_items = info_items.push(text(format!("  Interfaces: {up}/{total} up")).size(11));
        }
        if let (Some(est), Some(lis)) = (node.tcp_established, node.tcp_listen) {
            info_items = info_items
                .push(text(format!("  TCP: {est:.0} established, {lis:.0} listening")).size(11));
        } else if let Some(est) = node.tcp_established {
            info_items = info_items.push(text(format!("  TCP established: {est:.0}")).size(11));
        }
        if let Some(routes) = node.routes_total {
            info_items = info_items.push(text(format!("  Routes: {routes:.0}")).size(11));
        }
        if let Some(nbrs) = node.neighbors_total {
            info_items = info_items.push(text(format!("  Neighbors: {nbrs:.0}")).size(11));
        }
    }

    // Firing alerts for this host (#83).
    if !node.alerts.is_empty() {
        use crate::view::alerts::Severity;
        info_items = info_items.push(rule::horizontal(1));
        info_items = info_items.push(text(format!("Alerts ({})", node.alerts.len())).size(12));
        for a in &node.alerts {
            let color = Severity::from(a.severity).color();
            info_items = info_items.push(
                text(format!("  ● [{}] {} — {}", a.severity, a.rule, a.summary))
                    .size(11)
                    .style(move |_: &iced::Theme| iced::widget::text::Style { color: Some(color) }),
            );
        }
    }

    // Layout info
    info_items = info_items.push(rule::horizontal(1));
    if node.pinned {
        info_items = info_items.push(
            row![
                icons::status_warning(IconSize::Small),
                text("Position pinned").size(10)
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        );
    }

    // Action buttons
    info_items = info_items.push(rule::horizontal(1));

    let view_btn = button(
        row![
            icons::arrow_right(IconSize::Small),
            text("View Device Details").size(11)
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .on_press(Message::TopologyViewDeviceDetail(node.id.clone()))
    .style(iced::widget::button::primary)
    .width(Length::Fill);
    info_items = info_items.push(view_btn);

    let clear_btn = button(text("Clear Selection").size(11))
        .on_press(Message::TopologyClearSelection)
        .style(iced::widget::button::secondary)
        .width(Length::Fill);
    info_items = info_items.push(clear_btn);

    container(info_items)
        .padding(15)
        .width(Length::Fixed(200.0))
        .style(container::rounded_box)
        .into()
}

/// Render the edge detail panel (#25): src→dst, protocol, observed bytes/packets,
/// and when last seen. Shown when an edge is selected.
fn render_edge_info_panel(edge: &Edge) -> Element<'_, Message> {
    use crate::view::formatting::format_timestamp;
    use crate::view::topology::graph::format_bytes;
    use iced::widget::rule;

    let header = row![
        icons::network(IconSize::Large),
        text(format!("{} → {}", edge.from, edge.to)).size(15),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let proto = edge.protocol.as_deref().unwrap_or("?");
    let mut items = column![
        header,
        rule::horizontal(1),
        text("Observed flow").size(12),
        text(format!("Protocol: {proto}")).size(11),
        text(format!("Bytes: {}", format_bytes(edge.bytes))).size(11),
        text(format!("Packets: {}", edge.packets)).size(11),
        text(format!("Last seen: {}", format_timestamp(edge.last_seen))).size(11),
    ]
    .spacing(8);

    items = items.push(rule::horizontal(1));
    items = items.push(
        button(text("Clear Selection").size(11))
            .on_press(Message::TopologyClearSelection)
            .style(iced::widget::button::secondary)
            .width(Length::Fill),
    );

    container(items)
        .padding(15)
        .width(Length::Fixed(220.0))
        .style(container::rounded_box)
        .into()
}

/// Render the topology header.
fn render_header(state: &TopologyState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseTopology)
    .style(iced::widget::button::secondary);

    let title = text("Network Topology").size(24);

    let node_count = text(format!("{} nodes", state.nodes.len())).size(14);
    let edge_count = text(format!("{} connections", state.edges.len())).size(14);

    // Show layout status
    let layout_status = if !state.auto_layout {
        text("Layout: Manual").size(10)
    } else if state.layout_stable {
        text("Layout: Stable").size(10)
    } else {
        text("Layout: Adjusting...").size(10)
    };

    // Show search match count if searching
    let search_matches = if !state.search_query.is_empty() {
        let matches = state
            .nodes
            .values()
            .filter(|n| {
                n.label
                    .to_lowercase()
                    .contains(&state.search_query.to_lowercase())
            })
            .count();
        Some(text(format!("{} matches", matches)).size(10))
    } else {
        None
    };

    let zoom_label = text(format!("{}%", (state.zoom * 100.0) as i32)).size(12);

    let zoom_out_btn = button(text("-").size(14))
        .on_press(Message::TopologyZoomOut)
        .style(iced::widget::button::secondary);

    let zoom_in_btn = button(text("+").size(14))
        .on_press(Message::TopologyZoomIn)
        .style(iced::widget::button::secondary);

    let reset_btn = button(text("Reset").size(12))
        .on_press(Message::TopologyZoomReset)
        .style(iced::widget::button::secondary);

    let auto_layout_btn = button(
        text(if state.auto_layout {
            "Auto Layout: ON"
        } else {
            "Auto Layout: OFF"
        })
        .size(12),
    )
    .on_press(Message::TopologyToggleAutoLayout)
    .style(if state.auto_layout {
        iced::widget::button::primary
    } else {
        iced::widget::button::secondary
    });

    // Search input
    let search_input = text_input("Search nodes...", &state.search_query)
        .on_input(Message::TopologySetSearch)
        .padding(6)
        .width(Length::Fixed(150.0));

    let search_row = row![icons::search(IconSize::Small), search_input]
        .spacing(6)
        .align_y(Alignment::Center);

    let mut header = row![
        back_button,
        title,
        node_count,
        edge_count,
        layout_status,
        search_row,
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    if let Some(matches) = search_matches {
        header = header.push(matches);
    }

    header = header
        .push(zoom_out_btn)
        .push(zoom_label)
        .push(zoom_in_btn)
        .push(reset_btn)
        .push(auto_layout_btn);

    header.into()
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
    fn rebuild_edges_flow_precedence_and_router_classification() {
        use zensight_common::Protocol;
        let mut state = TopologyState::default();
        for id in ["hostA", "gw", "hostB"] {
            let mut node = Node {
                id: id.to_string(),
                ..Default::default()
            };
            if id == "hostA" {
                node.protocols.insert(Protocol::Netlink);
            }
            state.nodes.insert(id.to_string(), node);
        }
        let mut map = HashMap::new();
        for id in ["hostA", "gw", "hostB"] {
            map.insert(id.to_string(), id.to_string());
        }

        // A flow already covers hostA<->hostB with real bandwidth.
        state.apply_flow_edges(&[flow("hostA:1", "hostB:2", 1000, 10, "tcp")], &map, 1);
        // Neighbors add the gateway adjacency and re-cover hostA<->hostB.
        state.apply_neighbor_edges(&[neighbor("gw", true), neighbor("hostB", false)], &map, 2);

        // hostA<->hostB keeps its flow bytes (not overwritten by the 0-byte
        // neighbor edge); a new hostA<->gw adjacency edge is added.
        assert_eq!(state.edges.len(), 2);
        let ab = state
            .edges
            .iter()
            .find(|e| ordered_pair(&e.from, &e.to) == ("hostA".to_string(), "hostB".to_string()))
            .unwrap();
        assert_eq!(ab.bytes, 1000);
        assert!(
            state
                .edges
                .iter()
                .any(|e| ordered_pair(&e.from, &e.to) == ("gw".to_string(), "hostA".to_string()))
        );
        // The is_router gateway is classified Router; plain hosts stay Host.
        assert_eq!(state.nodes["gw"].node_type, NodeType::Router);
        assert_eq!(state.nodes["hostA"].node_type, NodeType::Host);
    }

    #[test]
    fn test_topology_state_default() {
        let state = TopologyState::default();
        assert!(state.nodes.is_empty());
        assert!(state.edges.is_empty());
        assert_eq!(state.zoom, 1.0);
        assert!(state.auto_layout);
    }

    #[test]
    fn test_zoom_limits() {
        let mut state = TopologyState::default();

        // Zoom in multiple times
        for _ in 0..20 {
            state.zoom_in();
        }
        assert!(state.zoom <= 3.0);

        // Zoom out multiple times
        for _ in 0..20 {
            state.zoom_out();
        }
        assert!(state.zoom >= 0.3);
    }

    #[test]
    fn test_selection() {
        let mut state = TopologyState::default();

        state.nodes.insert(
            "node1".to_string(),
            Node {
                id: "node1".to_string(),
                label: "Node 1".to_string(),
                position: (100.0, 100.0),
                velocity: (0.0, 0.0),
                node_type: NodeType::Host,
                cpu_usage: None,
                memory_usage: None,
                network_rx: None,
                network_tx: None,
                is_healthy: true,
                pinned: false,
                alert: None,
                sensor_count: None,
                ..Default::default()
            },
        );

        state.select_node("node1".to_string());
        assert_eq!(state.selected_node, Some("node1".to_string()));

        state.clear_selection();
        assert!(state.selected_node.is_none());
    }

    #[test]
    fn test_alert_overlay() {
        use std::collections::HashMap;
        use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

        let mut state = TopologyState::default();
        state.nodes.insert(
            "host1".to_string(),
            Node {
                id: "host1".to_string(),
                label: "host1".to_string(),
                position: (0.0, 0.0),
                velocity: (0.0, 0.0),
                node_type: NodeType::Host,
                cpu_usage: None,
                memory_usage: None,
                network_rx: None,
                network_tx: None,
                is_healthy: true,
                pinned: false,
                alert: None,
                sensor_count: None,
                ..Default::default()
            },
        );

        let mut external = HashMap::new();
        let warn = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "link:eth0",
            AlertSeverity::Warning,
            "down",
        );
        let crit = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd",
            AlertSeverity::Critical,
            "not listening",
        );
        external.insert(warn.alert_key(), warn);
        external.insert(crit.alert_key(), crit);

        state.apply_alerts(&external);
        // Highest severity wins.
        assert_eq!(state.nodes["host1"].alert, Some(AlertSeverity::Critical));

        // Clearing resolves the overlay.
        state.apply_alerts(&HashMap::new());
        assert_eq!(state.nodes["host1"].alert, None);
    }

    #[test]
    fn test_edge_health_overlay() {
        use std::collections::HashMap;
        use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

        let node = |id: &str| Node {
            id: id.to_string(),
            label: id.to_string(),
            position: (0.0, 0.0),
            velocity: (0.0, 0.0),
            node_type: NodeType::Host,
            cpu_usage: None,
            memory_usage: None,
            network_rx: None,
            network_tx: None,
            is_healthy: true,
            pinned: false,
            alert: None,
            sensor_count: None,
            ..Default::default()
        };

        let mut state = TopologyState::default();
        state.nodes.insert("a".to_string(), node("a"));
        state.nodes.insert("b".to_string(), node("b"));
        state.edges.push(Edge {
            from: "a".to_string(),
            to: "b".to_string(),
            bytes: 10,
            packets: 1,
            protocol: None,
            last_seen: 0,
            alert: None,
        });

        let mut external = HashMap::new();
        let crit = Alert::new(
            "a",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd",
            AlertSeverity::Critical,
            "down",
        );
        external.insert(crit.alert_key(), crit);

        state.apply_alerts(&external);
        // The link to the alerting endpoint inherits its severity (#49).
        assert_eq!(state.edges[0].alert, Some(AlertSeverity::Critical));

        state.apply_alerts(&HashMap::new());
        assert_eq!(state.edges[0].alert, None);
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
    fn test_node_sourcing_widened_excludes_overlays() {
        use std::collections::HashMap;
        use zensight_common::Protocol;

        let mut devices: HashMap<DeviceId, DeviceState> = HashMap::new();
        let mut add = |proto: Protocol, source: &str, metrics: usize| {
            let id = DeviceId::new(proto, source);
            let mut d = DeviceState::new(id.clone());
            d.metric_count = metrics;
            devices.insert(id, d);
        };
        // Host gear that should each become a node.
        add(Protocol::Sysinfo, "server01", 10);
        add(Protocol::Netlink, "server01", 5); // same host → merges, one node
        add(Protocol::Netflow, "exporter01", 3);
        add(Protocol::Snmp, "switch01", 7);
        add(Protocol::Modbus, "plc01", 2);
        add(Protocol::Gnmi, "router01", 4);
        // Overlays that must NOT add their own nodes.
        add(Protocol::Logs, "logbox01", 99);
        add(Protocol::Netring, "sensor01", 99);

        let mut state = TopologyState::default();
        state.update_from_devices(&devices);

        // 5 distinct hosts (server01 merged), no syslog/netring nodes.
        assert_eq!(state.nodes.len(), 5);
        assert!(state.nodes.contains_key("exporter01"));
        assert!(state.nodes.contains_key("switch01"));
        assert!(state.nodes.contains_key("plc01"));
        assert!(state.nodes.contains_key("router01"));
        assert!(!state.nodes.contains_key("logbox01"));
        assert!(!state.nodes.contains_key("sensor01"));

        // Merged host carries both protocols and the summed metric tally.
        let server = state.nodes.get("server01").unwrap();
        assert!(server.protocols.contains(&Protocol::Sysinfo));
        assert!(server.protocols.contains(&Protocol::Netlink));
        assert_eq!(server.metric_count, 15);

        // Re-running doesn't double-count the per-host metric tally.
        state.update_from_devices(&devices);
        assert_eq!(state.nodes.get("server01").unwrap().metric_count, 15);
        assert_eq!(state.nodes.len(), 5);
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

    #[test]
    fn test_apply_alerts_lists_node_alerts() {
        use std::collections::HashMap;
        use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

        let mut state = TopologyState::default();
        state.nodes.insert(
            "host1".to_string(),
            Node {
                id: "host1".to_string(),
                label: "host1".to_string(),
                ..Default::default()
            },
        );

        let mut external = HashMap::new();
        let warn = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd",
            AlertSeverity::Warning,
            "sshd not listening",
        );
        let crit = Alert::new(
            "host1",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScanTRW",
            AlertSeverity::Critical,
            "port scan",
        );
        external.insert(warn.alert_key(), warn);
        external.insert(crit.alert_key(), crit);

        state.apply_alerts(&external);
        let n = &state.nodes["host1"];
        assert_eq!(n.alert, Some(AlertSeverity::Critical));
        assert_eq!(n.alerts.len(), 2);
        // Highest severity first.
        assert_eq!(n.alerts[0].severity, AlertSeverity::Critical);
        assert_eq!(n.alerts[0].rule, "PortScanTRW");

        // Clearing removes the per-node list.
        state.apply_alerts(&HashMap::new());
        assert!(state.nodes["host1"].alerts.is_empty());
    }
}
