//! Network topology visualization.
//!
//! Displays interconnections between VMs/hosts as an interactive graph,
//! showing network bandwidth between each link.

pub mod graph;
pub mod layout;
pub mod model;

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
pub use model::{
    Edge, EdgeKind, INTERNET_NODE_ID, Node, NodeAlert, NodeHealth, NodeId, NodeRole, Provenance,
    counter_rate, edges_from_flows, edges_from_gateways, edges_from_matrix, edges_from_neighbors,
    endpoint_ip, external_edges_from_matrix, format_rate, gateway_from_metrics, is_public_ip,
    merge_flow_stats, node_health, roles_from_assets,
};

use model::{entity_node_label, is_node_protocol, ordered_pair, primary_protocol};

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
    /// Last netring traffic matrix fetched (#391): the primary, rate-weighted
    /// edge source. Flows remain the fallback + cumulative-stat enrichment.
    last_matrix: Vec<zensight_common::MatrixRecord>,
    /// Default gateway per node (#391), freshly collected from netlink
    /// telemetry on every [`Self::update_from_devices`] pass.
    pending_gateways: HashMap<NodeId, String>,
    /// The gateway map the current edge set was built from (#391); compared
    /// against `pending_gateways` so rebuilds only happen on change.
    last_gateways: HashMap<NodeId, String>,
    /// Asset-derived role + vendor per node (#391), from the netring passive
    /// inventory; reapplied on every edge rebuild (strongest role evidence).
    asset_roles: HashMap<NodeId, (NodeRole, Option<String>)>,
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
            last_matrix: Vec::new(),
            pending_gateways: HashMap::new(),
            last_gateways: HashMap::new(),
            asset_roles: HashMap::new(),
        }
    }
}

impl TopologyState {
    /// Update topology from dashboard device states. `sensor_health` is the
    /// per-sensor `@/health` snapshot map (host-scoped via `host_id`, #391);
    /// `now_ms` drives entity staleness.
    pub fn update_from_devices(
        &mut self,
        devices: &HashMap<DeviceId, DeviceState>,
        entities: &crate::entity::EntityStore,
        sensor_health: &HashMap<String, zensight_common::HealthSnapshot>,
        now_ms: i64,
    ) {
        let initial_count = self.nodes.len();

        // Recompute the per-host metric tally from scratch each pass (#83): nodes
        // persist across calls and a host has one facet per protocol, so zero
        // first, then accumulate below.
        for node in self.nodes.values_mut() {
            node.metric_count = 0;
        }

        // Freshly collect each netlink host's default gateway (#391); applied
        // as Gateway edges by `apply_gateway_edges` once the caller has an
        // ip_to_node map in hand.
        let mut gateways: HashMap<NodeId, String> = HashMap::new();

        // Per-node facet health inputs (#391): each device facet contributes
        // its liveness status + whether its telemetry is fresh.
        let mut facet_health: HashMap<NodeId, Vec<(zensight_common::DeviceStatus, bool)>> =
            HashMap::new();

        // A node per physical host (#83/#306): keyed by the correlator entity id
        // when the device maps into one, else by `source`. Widened beyond
        // sysinfo/netlink so netflow exporters and gNMI/SNMP/Modbus gear also
        // appear; syslog/netring are overlays (logs / flow edges), not nodes.
        for (device_id, device_state) in devices {
            if !is_node_protocol(device_id.protocol) {
                continue;
            }

            let source = device_id.source.clone();
            let node_id = match entities.by_device.get(device_id) {
                Some(eid) => entities.resolve_alias(eid).to_string(),
                None => source.clone(),
            };

            // Re-key migration (#306): when an entity claims a device that
            // already has a source-keyed node, transplant the old node's
            // position/pin to the entity node so the layout stays stable, and
            // follow the selection through the id change.
            if node_id != source
                && self.nodes.contains_key(&source)
                && !self.nodes.contains_key(&node_id)
            {
                if let Some(mut old) = self.nodes.remove(&source) {
                    old.id = node_id.clone();
                    self.nodes.insert(node_id.clone(), old);
                }
                if self.selected_node.as_deref() == Some(source.as_str()) {
                    self.selected_node = Some(node_id.clone());
                }
            }

            let label = entities
                .hosts
                .get(&node_id)
                .map(entity_node_label)
                .unwrap_or_else(|| source.clone());

            if !self.nodes.contains_key(&node_id) {
                // Create new node - position will be set by arrange_in_circle
                self.nodes.insert(
                    node_id.clone(),
                    Node {
                        id: node_id.clone(),
                        label: label.clone(),
                        ..Default::default()
                    },
                );
            }

            if device_id.protocol == zensight_common::Protocol::Netlink
                && let Some(gw) = model::gateway_from_metrics(&device_state.metrics)
            {
                gateways.insert(node_id.clone(), gw);
            }

            facet_health
                .entry(node_id.clone())
                .or_default()
                .push((device_state.sensor_status, device_state.is_healthy));

            // Update node metrics from telemetry
            if let Some(node) = self.nodes.get_mut(&node_id) {
                node.label = label;
                node.provenance = Provenance::Monitored;
                node.ips = match entities.hosts.get(&node_id) {
                    Some(e) => e.ips.clone(),
                    None if node_id.parse::<std::net::IpAddr>().is_ok() => {
                        vec![node_id.clone()]
                    }
                    None => Vec::new(),
                };
                node.protocols.insert(device_id.protocol);
                node.metric_count += device_state.metric_count;
                node.update_from_metrics(&device_state.metrics);
            }
        }

        self.pending_gateways = gateways;

        // Entity-derived overlays: passive wire-only nodes + sensor-count badge.
        self.apply_entities(entities);

        // Health pass (#391): liveness + host-scoped sensor health + entity
        // staleness, worst wins. Sensor snapshots join a node only when their
        // host_id matches the entity's (pre-#389 sensors publish none and
        // contribute nothing — device liveness still covers those hosts).
        for (id, node) in self.nodes.iter_mut() {
            let facets = facet_health.get(id).map(Vec::as_slice).unwrap_or(&[]);
            let entity = entities.hosts.get(id);
            let entity_stale = entity
                .map(|e| crate::entity::EntityStore::is_stale(e, now_ms))
                .unwrap_or(false);
            let sensor_statuses: Vec<zensight_common::HealthStatus> =
                match entity.and_then(|e| e.host_id.as_deref()) {
                    Some(host_id) => sensor_health
                        .values()
                        .filter(|s| s.host_id.as_deref() == Some(host_id))
                        .map(|s| s.status)
                        .collect(),
                    None => Vec::new(),
                };
            node.health = node_health(facets, &sensor_statuses, entity_stale);
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

    /// Overlay correlator entity data onto the node set (#306):
    /// - entity-backed nodes get a `sensor_count` badge (members merged),
    /// - entities whose members map to **no** live device node become passive
    ///   wire-only nodes ([`Provenance::Passive`]) so pure netring/netlink
    ///   observations still appear on the map.
    pub fn apply_entities(&mut self, entities: &crate::entity::EntityStore) {
        for entity in entities.hosts.values() {
            let id = entity.entity_id.as_str();
            if let Some(node) = self.nodes.get_mut(id) {
                // Live entity node: badge it with the merged member count.
                node.sensor_count = Some(entity.members.len());
                node.ips = entity.ips.clone();
            } else if !entity.members.is_empty() {
                // Wire-only: no live device-backed node for this entity.
                self.nodes.insert(
                    id.to_string(),
                    Node {
                        id: id.to_string(),
                        label: entity_node_label(entity),
                        provenance: Provenance::Passive,
                        role: NodeRole::Unknown,
                        sensor_count: Some(entity.members.len()),
                        ips: entity.ips.clone(),
                        ..Default::default()
                    },
                );
            }
        }
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

    /// Replace the edge set with edges derived from *observed* netring flow
    /// records (#25). `ip_to_node` maps an endpoint IP to a topology node id
    /// (built from node sources; entity IPs join in with #306). Flows whose src and dst both
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

    /// Merge the netring traffic matrix into the topology (#391): remembers it
    /// and rebuilds the edge set so links carry a live, directed bytes/sec
    /// rate. When present the matrix is the primary edge source; remembered
    /// flows enrich it with cumulative bytes/packets/protocol.
    pub fn apply_matrix_edges(
        &mut self,
        matrix: &[zensight_common::MatrixRecord],
        ip_to_node: &HashMap<String, NodeId>,
        now_ms: i64,
    ) {
        self.last_matrix = matrix.to_vec();
        self.rebuild_edges(ip_to_node, now_ms);
    }

    /// Apply live rx/tx rates (bytes/sec, from hot-ring counter deltas) onto
    /// nodes (#391). Change-gated cache clear: this runs on the 1 Hz tick.
    pub fn apply_rates(&mut self, rates: &HashMap<NodeId, (f64, f64)>) {
        let mut changed = false;
        for (id, node) in self.nodes.iter_mut() {
            let (rx, tx) = match rates.get(id).copied() {
                Some((rx, tx)) => (Some(rx), Some(tx)),
                None => (None, None),
            };
            if node.rx_rate != rx || node.tx_rate != tx {
                node.rx_rate = rx;
                node.tx_rate = tx;
                changed = true;
            }
        }
        if changed {
            self.cache.clear();
        }
    }

    /// Join the netring passive-asset inventory onto the node set (#391):
    /// remembers the per-node role/vendor map and rebuilds so roles and
    /// vendor labels apply. Asset roles are the strongest role evidence and
    /// win over neighbor/gateway router inference on every rebuild.
    pub fn apply_assets(
        &mut self,
        assets: &[zensight_common::AssetRecord],
        mac_to_node: &HashMap<String, NodeId>,
        ip_to_node: &HashMap<String, NodeId>,
        now_ms: i64,
    ) {
        self.asset_roles = roles_from_assets(assets, mac_to_node, ip_to_node);
        self.rebuild_edges(ip_to_node, now_ms);
    }

    /// Apply the gateway map collected by the last [`Self::update_from_devices`]
    /// pass (#391), rebuilding edges only when it actually changed — this runs
    /// on the 1 Hz tick, and an unconditional rebuild would clear the canvas
    /// cache and drop the edge selection every second.
    pub fn apply_gateway_edges(&mut self, ip_to_node: &HashMap<String, NodeId>, now_ms: i64) {
        if self.pending_gateways == self.last_gateways {
            return;
        }
        self.last_gateways = self.pending_gateways.clone();
        self.rebuild_edges(ip_to_node, now_ms);
    }

    /// Merge the netlink neighbor (ARP/NDP) table into the topology (#49):
    /// remembers it and rebuilds the edge set so direct L2/L3 adjacencies appear
    /// as links even when netring sees no traffic, and `is_router` neighbors are
    /// classified as [`NodeRole::Router`].
    pub fn apply_neighbor_edges(
        &mut self,
        neighbors: &[zensight_common::NeighborRecord],
        ip_to_node: &HashMap<String, NodeId>,
        now_ms: i64,
    ) {
        self.last_neighbors = neighbors.to_vec();
        self.rebuild_edges(ip_to_node, now_ms);
    }

    /// Rebuild the edge set from the remembered matrix + flow + neighbor inputs
    /// (#25/#49/#391). The rate-weighted matrix is the primary source, enriched
    /// with cumulative flow stats (and flows alone are the fallback when no
    /// matrix has arrived — older netring, queryable absent). Neighbor
    /// adjacencies add zero-bandwidth links for node pairs nothing covered.
    /// Router classification from `is_router` neighbors is reset and reapplied
    /// each pass so it tracks the live table. Pure given its remembered inputs
    /// + `ip_to_node`.
    fn rebuild_edges(&mut self, ip_to_node: &HashMap<String, NodeId>, now_ms: i64) {
        use std::collections::BTreeSet;
        use zensight_common::Protocol;

        let flow_edges = edges_from_flows(&self.last_flows, ip_to_node, now_ms);
        let mut edges = if self.last_matrix.is_empty() {
            flow_edges
        } else {
            merge_flow_stats(
                edges_from_matrix(&self.last_matrix, ip_to_node, now_ms),
                flow_edges,
            )
        };
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
        let (neighbor_edges, mut routers) =
            edges_from_neighbors(&host_nodes, &self.last_neighbors, ip_to_node, now_ms);
        for edge in neighbor_edges {
            if pairs.insert(ordered_pair(&edge.from, &edge.to)) {
                edges.push(edge);
            }
        }

        // Gateway edges (#391): host → default gw for pairs nothing covered.
        // Unresolved gateway addresses get a wire-only router node so the
        // physical topology reads even on quiet networks. Being somebody's
        // default gateway is router evidence, resolved or not.
        let (gateway_edges, missing_gws) =
            edges_from_gateways(&self.last_gateways, ip_to_node, now_ms);
        for gw_ip in missing_gws {
            if !self.nodes.contains_key(&gw_ip) {
                self.nodes.insert(
                    gw_ip.clone(),
                    Node {
                        id: gw_ip.clone(),
                        label: gw_ip.clone(),
                        role: NodeRole::Router,
                        provenance: Provenance::Passive,
                        ..Default::default()
                    },
                );
                self.layout_stable = false;
            }
        }
        for edge in gateway_edges {
            routers.insert(edge.to.clone());
            if pairs.insert(ordered_pair(&edge.from, &edge.to)) {
                edges.push(edge);
            }
        }

        // North–south rollup (#392): matrix rows to unmapped public addresses
        // aggregate into one Internet pseudo-node instead of vanishing. The
        // node exists only while external traffic does.
        let external_edges = external_edges_from_matrix(&self.last_matrix, ip_to_node, now_ms);
        if external_edges.is_empty() {
            if self.nodes.remove(INTERNET_NODE_ID).is_some()
                && self.selected_node.as_deref() == Some(INTERNET_NODE_ID)
            {
                self.selected_node = None;
            }
        } else {
            if !self.nodes.contains_key(INTERNET_NODE_ID) {
                self.nodes.insert(
                    INTERNET_NODE_ID.to_string(),
                    Node {
                        id: INTERNET_NODE_ID.to_string(),
                        label: "Internet".to_string(),
                        role: NodeRole::Internet,
                        provenance: Provenance::External,
                        ..Default::default()
                    },
                );
                self.layout_stable = false;
            }
            for edge in external_edges {
                if pairs.insert(ordered_pair(&edge.from, &edge.to)) {
                    edges.push(edge);
                }
            }
        }

        // Reset then reapply role classification so it follows the live
        // table. Monitored hosts default to Host, wire-only nodes to Unknown;
        // `is_router` neighbors become routers regardless of provenance (a
        // passive gateway keeps its dashed ring but reads as a router).
        for node in self.nodes.values_mut() {
            node.role = match node.provenance {
                Provenance::Monitored => NodeRole::Host,
                Provenance::Passive => NodeRole::Unknown,
                // The Internet aggregate's role is intrinsic (#392).
                Provenance::External => NodeRole::Internet,
            };
        }
        for id in &routers {
            if let Some(node) = self.nodes.get_mut(id) {
                node.role = NodeRole::Router;
            }
        }
        // Asset inventory (#391) is the strongest role evidence: a device the
        // wire says is a switch/ap/iot stays that way even if it also routes.
        // Unknown asset roles never clobber the inferences above.
        for (id, (role, vendor)) in &self.asset_roles {
            if let Some(node) = self.nodes.get_mut(id) {
                if *role != NodeRole::Unknown {
                    node.role = *role;
                }
                if vendor.is_some() {
                    node.vendor = vendor.clone();
                }
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

    // Header with a protocol-aware icon and name (#83).
    let header = row![
        icons::protocol_icon(primary_protocol(node), IconSize::Large),
        column![
            text(&node.label).size(16),
            text(match node.provenance {
                Provenance::Passive => format!("{} · wire-only", node.role.label()),
                _ => node.role.label().to_string(),
            })
            .size(10),
        ]
        .spacing(2)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Status indicator (#391): the four health states.
    let status = match node.health {
        NodeHealth::Healthy => row![
            icons::status_healthy(IconSize::Small),
            text("Healthy - receiving data").size(11)
        ],
        NodeHealth::Degraded => row![
            icons::status_warning(IconSize::Small),
            text("Degraded - partial data or sensor trouble").size(11)
        ],
        NodeHealth::Down => row![
            icons::status_warning(IconSize::Small),
            text("Down - all sensors report offline").size(11)
        ],
        NodeHealth::Stale => row![
            icons::status_warning(IconSize::Small),
            text("Stale - no recent data").size(11)
        ],
    }
    .spacing(5)
    .align_y(Alignment::Center);

    let mut info_items = column![header, status, rule::horizontal(1)].spacing(8);

    // Hardware vendor from the passive-asset inventory (#391).
    if let Some(ref vendor) = node.vendor {
        info_items = info_items.push(text(format!("Vendor: {vendor}")).size(11));
    }

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

        // Live rates first (#391), cumulative counters after.
        if let (Some(rx), Some(tx)) = (node.rx_rate, node.tx_rate) {
            info_items = info_items
                .push(text(format!("  ↓ {}  ↑ {}", format_rate(rx), format_rate(tx))).size(11));
        }
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
            dst_names: Vec::new(),
        }
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
        assert_eq!(state.nodes["gw"].role, NodeRole::Router);
        assert_eq!(state.nodes["hostA"].role, NodeRole::Host);
    }

    #[test]
    fn rebuild_edges_matrix_takes_precedence_over_flows() {
        let mut state = TopologyState::default();
        for id in ["a", "b", "c"] {
            state.nodes.insert(
                id.to_string(),
                Node {
                    id: id.to_string(),
                    ..Default::default()
                },
            );
        }
        let mut map = HashMap::new();
        for id in ["a", "b", "c"] {
            map.insert(id.to_string(), id.to_string());
        }

        // Flows first: a<->b with cumulative stats.
        state.apply_flow_edges(&[flow("a:1", "b:2", 4000, 40, "tcp")], &map, 1);
        assert_eq!(state.edges.len(), 1);
        assert_eq!(state.edges[0].rate, 0.0);

        // Matrix arrives: a->b rated + b->c (a pair flows never saw).
        let matrix = vec![
            zensight_common::MatrixRecord {
                src: "a".to_string(),
                dst: "b".to_string(),
                bytes_per_sec: 1234.0,
                names: Vec::new(),
            },
            zensight_common::MatrixRecord {
                src: "b".to_string(),
                dst: "c".to_string(),
                bytes_per_sec: 10.0,
                names: Vec::new(),
            },
        ];
        state.apply_matrix_edges(&matrix, &map, 2);

        assert_eq!(state.edges.len(), 2);
        let ab = state
            .edges
            .iter()
            .find(|e| ordered_pair(&e.from, &e.to) == ("a".to_string(), "b".to_string()))
            .unwrap();
        // Rated by the matrix, enriched with the flow pair's cumulative stats.
        assert_eq!(ab.rate, 1234.0);
        assert_eq!(ab.bytes, 4000);
        assert_eq!(ab.protocol.as_deref(), Some("tcp"));
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
                cpu_usage: None,
                memory_usage: None,
                network_rx: None,
                network_tx: None,
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
                cpu_usage: None,
                memory_usage: None,
                network_rx: None,
                network_tx: None,
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
            ..Default::default()
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
        state.update_from_devices(
            &devices,
            &crate::entity::EntityStore::default(),
            &HashMap::new(),
            0,
        );

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
        state.update_from_devices(
            &devices,
            &crate::entity::EntityStore::default(),
            &HashMap::new(),
            0,
        );
        assert_eq!(state.nodes.get("server01").unwrap().metric_count, 15);
        assert_eq!(state.nodes.len(), 5);
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
