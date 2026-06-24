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
        }
    }
}

impl TopologyState {
    /// Update topology from dashboard device states.
    pub fn update_from_devices(&mut self, devices: &HashMap<DeviceId, DeviceState>) {
        use zensight_common::Protocol;
        let initial_count = self.nodes.len();

        // A node per host: sysinfo or netlink devices (a host running both is one
        // node, merged by source).
        for (device_id, device_state) in devices {
            if !matches!(device_id.protocol, Protocol::Sysinfo | Protocol::Netlink) {
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
                        position: (0.0, 0.0),
                        velocity: (0.0, 0.0),
                        node_type: NodeType::Host,
                        cpu_usage: None,
                        memory_usage: None,
                        network_rx: None,
                        network_tx: None,
                        is_healthy: device_state.is_healthy,
                        pinned: false,
                        alert: None,
                        sensor_count: None,
                    },
                );
            }

            // Update node metrics from telemetry
            if let Some(node) = self.nodes.get_mut(&node_id) {
                node.is_healthy = device_state.is_healthy;
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
        }
        for alert in external.values() {
            if let Some(node) = self.nodes.get_mut(&alert.source) {
                node.alert = Some(match node.alert {
                    Some(cur) => cur.max(alert.severity),
                    None => alert.severity,
                });
            }
        }
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
        self.edges = edges_from_flows(flows, ip_to_node, now_ms);
        self.selected_edge = None;
        self.cache.clear();
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

    /// Get the DeviceId for a node (if it corresponds to a device).
    pub fn node_to_device_id(&self, node_id: &NodeId) -> Option<DeviceId> {
        self.nodes.get(node_id).map(|_| DeviceId {
            protocol: zensight_common::Protocol::Sysinfo,
            source: node_id.clone(),
        })
    }
}

/// A node in the topology graph.
#[derive(Debug, Clone)]
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
    /// CPU usage percentage (0-100).
    pub cpu_usage: Option<f64>,
    /// Memory usage percentage (0-100).
    pub memory_usage: Option<f64>,
    /// Network RX bytes/sec.
    pub network_rx: Option<u64>,
    /// Network TX bytes/sec.
    pub network_tx: Option<u64>,
    /// Whether the node is healthy.
    pub is_healthy: bool,
    /// Whether the node position is pinned (not affected by layout).
    pub pinned: bool,
    /// Highest-severity firing sensor alert for this host, if any (overlay).
    pub alert: Option<zensight_common::AlertSeverity>,
    /// Number of sensors that have correlated this host (#25). `None` until a
    /// correlation entry references it; surfaces the otherwise-dead correlations
    /// map as a "seen by N sensors" node label.
    pub sensor_count: Option<usize>,
}

impl Node {
    /// Update node metrics from telemetry.
    pub fn update_from_metrics(
        &mut self,
        metrics: &HashMap<String, zensight_common::TelemetryPoint>,
    ) {
        use zensight_common::TelemetryValue;

        for (name, point) in metrics {
            match name.as_str() {
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
                _ => {
                    // Check for network metrics
                    if name.starts_with("network/") && name.ends_with("/rx_bytes") {
                        if let TelemetryValue::Counter(v) = &point.value {
                            self.network_rx = Some(*v);
                        }
                    } else if name.starts_with("network/")
                        && name.ends_with("/tx_bytes")
                        && let TelemetryValue::Counter(v) = &point.value
                    {
                        self.network_tx = Some(*v);
                    }
                }
            }
        }
    }
}

/// Type of topology node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    /// A host/VM.
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

/// Render the node info panel (shown when a node is selected).
fn render_node_info_panel(node: &Node) -> Element<'_, Message> {
    use iced::widget::rule;

    // Header with icon and name
    let header = row![
        icons::protocol_sysinfo(IconSize::Large),
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
}
