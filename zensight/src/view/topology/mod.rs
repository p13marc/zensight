//! Network topology visualization.
//!
//! Displays interconnections between VMs/hosts as an interactive graph,
//! showing network bandwidth between each link.

pub mod graph;
pub mod layout;

use std::collections::HashMap;

use iced::widget::canvas::Cache;
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Element, Length};

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
        let initial_count = self.nodes.len();

        // For now, create a node for each sysinfo device
        for (device_id, device_state) in devices {
            if device_id.protocol != zensight_common::Protocol::Sysinfo {
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
            self.arrange_in_circle(150.0);
            self.layout_stable = false;
            self.cache.clear();
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

    /// Run one iteration of the force-directed layout.
    /// Returns true if the layout is stable.
    pub fn run_layout_step(&mut self) -> bool {
        self.layout_stable = layout_step(self, &self.layout_config.clone());
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
                    } else if name.starts_with("network/") && name.ends_with("/tx_bytes") {
                        if let TelemetryValue::Counter(v) = &point.value {
                            self.network_tx = Some(*v);
                        }
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

/// Render the topology view.
pub fn topology_view<'a>(state: &'a TopologyState) -> Element<'a, Message> {
    let header = render_header(state);
    let graph = TopologyGraph::new(state);

    let content = column![header, graph].spacing(10).padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
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

    row![
        back_button,
        title,
        node_count,
        edge_count,
        zoom_out_btn,
        zoom_label,
        zoom_in_btn,
        reset_btn,
        auto_layout_btn,
    ]
    .spacing(15)
    .align_y(Alignment::Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
        );

        state.select_node("node1".to_string());
        assert_eq!(state.selected_node, Some("node1".to_string()));

        state.clear_selection();
        assert!(state.selected_node.is_none());
    }
}
