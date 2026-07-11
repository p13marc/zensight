//! Network topology visualization.
//!
//! Displays interconnections between VMs/hosts as an interactive graph,
//! showing network bandwidth between each link.

pub mod graph;
pub mod layout;
pub mod model;
pub mod panel;
pub mod tiered;

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
pub use layout::{
    LayoutConfig, arrange_circle, center_layout, grid_positions, layout_step, seed_position,
};
pub use model::{
    Edge, EdgeKind, EdgeLabelMode, FocusState, GroupingMode, INTERNET_NODE_ID, LayoutMode, Lens,
    Node, NodeAlert, NodeHealth, NodeId, NodeRole, Provenance, RenderEdge, RenderGraph, RenderNode,
    RenderSource, TintSource, TopoFilters, TopoPrefs, counter_rate, edges_from_flows,
    edges_from_gateways, edges_from_matrix, edges_from_neighbors, endpoint_ip,
    external_edges_from_matrix, format_rate, gateway_from_metrics, is_public_ip, merge_flow_stats,
    node_health, render_node_position, roles_from_assets,
};

use model::{entity_node_label, is_node_protocol, ordered_pair, primary_protocol};
pub use tiered::{PositionTween, TierBand, TieredLayout, tiered_layout, tween_at};

/// Flow edges slower than this (bytes/sec) don't animate (#394).
pub const ANIMATED_EDGE_MIN_RATE: f64 = 1_000.0;

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
    /// d3-style cooling alpha (#441): scales the force simulation's applied
    /// motion and decays each iteration, so the settle eases out and always
    /// converges. Reset to 1.0 by [`Self::wake_layout`].
    pub layout_alpha: f32,
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
    /// Presentation preferences (#392): lens, edge labels, grouping, filters,
    /// focus. Mutate through the setters so the render graph stays fresh.
    pub prefs: TopoPrefs,
    /// Device-group label per node (#392), resolved by the app from the
    /// settings' group assignments; drives GroupingMode::DeviceGroup.
    pub group_labels: HashMap<NodeId, String>,
    /// The resolved graph the canvas draws (#392): filters/search/focus (and
    /// later grouping/lens) applied. Rebuilt by [`Self::invalidate`] on
    /// structural or pref changes — never per frame.
    pub render: RenderGraph,
    /// Details-on-demand data for the side panel (#393), fetched on selection
    /// (never on tick) and reset when the selection changes.
    pub panel: PanelData,
    /// Persisted node positions (#394), applied when a node (re)appears so a
    /// manual arrangement survives restarts.
    pub saved_positions: HashMap<NodeId, (f32, f32)>,
    /// Persisted pinned-node set (#394).
    pub saved_pins: std::collections::HashSet<NodeId>,
    /// Hovered node id (#394): its 1-hop neighborhood stays bright, the rest
    /// dims. Session-transient.
    pub hovered: Option<NodeId>,
    /// The hovered node's neighborhood (hovered + direct peers).
    pub hover_set: std::collections::HashSet<NodeId>,
    /// Dash-march offset for animated flow edges (#394); advanced by the
    /// gated animation tick, drawn in the uncached overlay layer.
    pub anim_offset: u32,
    /// Whether the lens legend is shown (#394). Session-transient.
    pub show_legend: bool,
    /// The tier/subnet bands of the current tiered layout (#442), for the
    /// canvas captions. Empty outside Tiered mode.
    pub tier_bands: Vec<TierBand>,
    /// In-flight animated transition to freshly computed tiered positions
    /// (#442). Stepped by the layout-frame subscription; `None` when parked.
    tween: Option<PositionTween>,
    /// The last computed tiered layout (#442): the change gate — recomputing
    /// an identical layout is a no-op, so the 1 Hz refresh stays free.
    last_tiered: TieredLayout,
}

/// One topology data-refresh reply set (#440): whichever of the four
/// queryables answered. `None` keeps the remembered input for that source.
#[derive(Debug, Clone, Default)]
pub struct TopologyBatch {
    pub flows: Option<Vec<zensight_common::FlowRecord>>,
    pub neighbors: Option<Vec<zensight_common::NeighborRecord>>,
    pub matrix: Option<Vec<zensight_common::MatrixRecord>>,
    pub assets: Option<Vec<zensight_common::AssetRecord>>,
}

/// On-demand side-panel data (#393).
#[derive(Debug, Default)]
pub struct PanelData {
    /// Listen sockets for the selected node.
    pub listen: crate::view::specialized::fetch::Fetch<Vec<zensight_common::SocketRecord>>,
    /// Recent flows between the selected edge's endpoints.
    pub edge_flows: crate::view::specialized::fetch::Fetch<Vec<zensight_common::FlowRecord>>,
    /// One in-flight flow→process join for the edge panel (#309 reuse):
    /// `(flow key, fetched attribution)`.
    pub attribution: Option<(
        String,
        crate::view::specialized::fetch::Fetch<
            Option<crate::view::specialized::attribution::AttributedProcess>,
        >,
    )>,
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
            layout_alpha: 1.0,
            last_flows: Vec::new(),
            last_neighbors: Vec::new(),
            last_matrix: Vec::new(),
            pending_gateways: HashMap::new(),
            last_gateways: HashMap::new(),
            asset_roles: HashMap::new(),
            prefs: TopoPrefs::default(),
            group_labels: HashMap::new(),
            render: RenderGraph::default(),
            panel: PanelData::default(),
            saved_positions: HashMap::new(),
            saved_pins: std::collections::HashSet::new(),
            hovered: None,
            hover_set: std::collections::HashSet::new(),
            anim_offset: 0,
            show_legend: false,
            tier_bands: Vec::new(),
            tween: None,
            last_tiered: TieredLayout::default(),
        }
    }
}

impl TopologyState {
    /// The remembered traffic matrix (#393): read by the side panel's
    /// top-talkers section.
    pub(crate) fn matrix(&self) -> &[zensight_common::MatrixRecord] {
        &self.last_matrix
    }

    /// Recompute the render graph (#392). Call after any change that affects
    /// what is drawn: node/edge structure, alerts, rates, search, prefs.
    /// Selection/zoom/pan/drag only need `cache.clear()` — they resolve at
    /// draw time.
    ///
    /// Change-gated (#440): this runs on the 1 Hz tick via
    /// [`Self::update_from_devices`], so the expensive part — the canvas
    /// cache clear and full re-tessellation — happens only when the rebuilt
    /// graph actually differs. The timestamp is quantized so the `hide_idle`
    /// cutoff can't make every rebuild compare unequal.
    pub(crate) fn invalidate(&mut self) {
        const NOW_QUANTUM_MS: i64 = 30_000;
        let now_ms = zensight_common::current_timestamp_millis() / NOW_QUANTUM_MS * NOW_QUANTUM_MS;
        let render = model::build_render_graph(
            &self.nodes,
            &self.edges,
            &self.prefs,
            &self.group_labels,
            &self.search_query,
            now_ms,
        );
        if render != self.render {
            self.render = render;
            self.cache.clear();
        }
    }

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
                // Create new node — a persisted position/pin (#394) wins over
                // the deterministic ring seeding (#440).
                self.nodes.insert(
                    node_id.clone(),
                    Node {
                        id: node_id.clone(),
                        label: label.clone(),
                        position: self
                            .saved_positions
                            .get(&node_id)
                            .copied()
                            .unwrap_or_else(|| seed_position(&node_id)),
                        pinned: self.saved_pins.contains(&node_id),
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
                match entities.hosts.get(&node_id) {
                    Some(e) => {
                        node.ips = e.ips.clone();
                        node.macs = e.macs.clone();
                    }
                    None if node_id.parse::<std::net::IpAddr>().is_ok() => {
                        node.ips = vec![node_id.clone()];
                    }
                    None => node.ips = Vec::new(),
                }
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

        // If new nodes were added, wake the layout so it absorbs just the
        // newcomers — the old whole-graph `arrange_in_circle` reseed made
        // every unpinned node jump whenever anything appeared (#440).
        if self.nodes.len() > initial_count {
            self.wake_layout();
        }
        self.invalidate();
        // Newcomers/identity changes may need a fresh tier slot (#442);
        // change-gated, so the 1 Hz refresh stays free.
        self.apply_tiered_layout(true);

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
                node.macs = entity.macs.clone();
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
                        macs: entity.macs.clone(),
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
        self.invalidate();
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
    /// nodes (#391). Change-gated: this runs on the 1 Hz tick, and rates
    /// fluctuate constantly on an active network, so a change patches the
    /// already-built render graph in place (#440) — a redraw, never a rebuild.
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
            self.refresh_render_rates();
        }
    }

    /// Refresh live rx/tx rates on the already-built render graph (#440):
    /// rates change every tick, structure doesn't, so patch the rendered
    /// nodes and redraw without a full [`Self::invalidate`] rebuild.
    fn refresh_render_rates(&mut self) {
        for rnode in &mut self.render.nodes {
            if let RenderSource::Node(id) = &rnode.source
                && let Some(node) = self.nodes.get(id)
            {
                rnode.rx_rate = node.rx_rate;
                rnode.tx_rate = node.tx_rate;
            }
        }
        self.cache.clear();
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

    /// Apply one topology data-refresh batch (#440): store every reply that
    /// arrived, then rebuild the edge set exactly **once** — the batch used
    /// to land as four separate messages, each triggering its own full
    /// rebuild + canvas cache clear back-to-back.
    pub fn apply_batch(
        &mut self,
        batch: TopologyBatch,
        mac_to_node: &HashMap<String, NodeId>,
        ip_to_node: &HashMap<String, NodeId>,
        now_ms: i64,
    ) {
        if let Some(flows) = batch.flows {
            self.last_flows = flows;
        }
        if let Some(neighbors) = batch.neighbors {
            self.last_neighbors = neighbors;
        }
        if let Some(matrix) = batch.matrix {
            self.last_matrix = matrix;
        }
        if let Some(assets) = batch.assets {
            self.asset_roles = roles_from_assets(&assets, mac_to_node, ip_to_node);
        }
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
                        position: seed_position(&gw_ip),
                        ..Default::default()
                    },
                );
                self.wake_layout();
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
                        position: seed_position(INTERNET_NODE_ID),
                        ..Default::default()
                    },
                );
                self.wake_layout();
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
        self.invalidate();
        // Structure/roles may have moved nodes between tiers (#442);
        // change-gated, so identical layouts are free.
        self.apply_tiered_layout(true);
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

    /// Select a node by ID. Panel data resets; the app triggers the
    /// on-demand fetches (#393).
    pub fn select_node(&mut self, node_id: NodeId) {
        if self.selected_node.as_ref() != Some(&node_id) {
            self.panel = PanelData::default();
        }
        self.selected_node = Some(node_id);
        self.selected_edge = None;
        self.cache.clear();
    }

    /// Select an edge by index. Panel data resets; the app triggers the
    /// on-demand fetches (#393).
    pub fn select_edge(&mut self, edge_index: usize) {
        if self.selected_edge != Some(edge_index) {
            self.panel = PanelData::default();
        }
        self.selected_edge = Some(edge_index);
        self.selected_node = None;
        self.cache.clear();
    }

    /// Clear selection.
    pub fn clear_selection(&mut self) {
        self.selected_node = None;
        self.selected_edge = None;
        self.panel = PanelData::default();
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
        // The drag wins over an in-flight tween (#442): drop the node from
        // both endpoints so the animation can't fight the pointer.
        if let Some(tween) = &mut self.tween {
            tween.from.remove(node_id);
            tween.to.remove(node_id);
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

    /// Set search query (#392): plain text / `find:` highlights, `hide:`
    /// removes — so the render graph must rebuild.
    pub fn set_search(&mut self, query: String) {
        self.search_query = query;
        self.invalidate();
    }

    /// Switch the presentation lens (#392).
    pub fn set_lens(&mut self, lens: Lens) {
        if self.prefs.lens != lens {
            self.prefs.lens = lens;
            self.invalidate();
        }
    }

    /// Switch what edge labels show (#392). The label mode is read at draw
    /// time (not baked into the render graph), so force a redraw even though
    /// the change-gated [`Self::invalidate`] sees an identical graph (#440).
    pub fn set_edge_label(&mut self, mode: EdgeLabelMode) {
        if self.prefs.edge_label != mode {
            self.prefs.edge_label = mode;
            self.cache.clear();
        }
    }

    /// Switch the grouping mode (#392); switching resets expansions.
    pub fn set_grouping(&mut self, mode: GroupingMode) {
        if self.prefs.grouping != mode {
            self.prefs.grouping = mode;
            self.prefs.expanded_groups.clear();
            self.invalidate();
        }
    }

    /// Expand a collapsed group (#392) — clicking a meta-node.
    pub fn expand_group(&mut self, group_id: String) {
        if self.prefs.expanded_groups.insert(group_id) {
            self.invalidate();
        }
    }

    /// Re-collapse every expanded group (#392).
    pub fn regroup(&mut self) {
        if !self.prefs.expanded_groups.is_empty() {
            self.prefs.expanded_groups.clear();
            self.invalidate();
        }
    }

    /// Enter focus mode on a node (#392), 1 hop by default.
    pub fn focus_node(&mut self, root: NodeId) {
        self.prefs.focus = Some(FocusState { root, hops: 1 });
        self.invalidate();
    }

    /// Change the focus radius (#392); clamped to 1..=3.
    pub fn set_focus_hops(&mut self, hops: u8) {
        if let Some(ref mut focus) = self.prefs.focus {
            let hops = hops.clamp(1, 3);
            if focus.hops != hops {
                focus.hops = hops;
                self.invalidate();
            }
        }
    }

    /// Leave focus mode (#392).
    pub fn exit_focus(&mut self) {
        if self.prefs.focus.take().is_some() {
            self.invalidate();
        }
    }

    /// Toggle the idle-edge filter (#392).
    pub fn toggle_hide_idle(&mut self) {
        self.prefs.filters.hide_idle = !self.prefs.filters.hide_idle;
        self.invalidate();
    }

    /// Toggle the passive-node filter (#392).
    pub fn toggle_hide_passive(&mut self) {
        self.prefs.filters.hide_passive = !self.prefs.filters.hide_passive;
        self.invalidate();
    }

    /// Toggle the external-aggregate filter (#392).
    pub fn toggle_hide_external(&mut self) {
        self.prefs.filters.hide_external = !self.prefs.filters.hide_external;
        self.invalidate();
    }

    /// Cap the number of flow edges shown (`0` = unlimited, #392).
    pub fn set_top_n(&mut self, top_n: usize) {
        if self.prefs.filters.top_n != top_n {
            self.prefs.filters.top_n = top_n;
            self.invalidate();
        }
    }

    /// Switch the layout mode (#394): Tiered tweens to the hierarchy (#442),
    /// Force re-animates, Grid/Circular apply a static arrangement (pinned
    /// nodes stay put throughout).
    pub fn set_layout(&mut self, mode: LayoutMode) {
        if self.prefs.layout == mode {
            return;
        }
        self.prefs.layout = mode;
        // Leaving/entering Tiered always resets its state: the change gate
        // must not skip a re-apply after another layout moved the nodes.
        self.tween = None;
        self.tier_bands.clear();
        self.last_tiered = TieredLayout::default();
        match mode {
            LayoutMode::Tiered => {
                self.apply_tiered_layout(true);
            }
            LayoutMode::Force => {
                self.wake_layout();
            }
            LayoutMode::Grid => {
                grid_positions(self);
                self.layout_stable = true;
            }
            LayoutMode::Circular => {
                arrange_circle(self, 400.0);
                self.layout_stable = true;
            }
        }
        self.cache.clear();
    }

    /// Toggle a node's pin (#394): pinned nodes ignore the layout.
    pub fn toggle_pin(&mut self, node_id: &NodeId) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.pinned = !node.pinned;
            if !node.pinned && self.prefs.layout == LayoutMode::Force {
                self.wake_layout();
            }
            self.cache.clear();
        }
    }

    /// Apply a computed fit (#394): zoom/pan so the whole graph is visible.
    pub fn apply_fit(&mut self, zoom: f32, pan: (f32, f32)) {
        self.zoom = zoom.clamp(0.3, 3.0);
        self.pan = pan;
        self.cache.clear();
    }

    /// The persisted-layout payload (#394): every pinned node's position.
    pub fn pinned_positions(&self) -> (Vec<NodeId>, HashMap<NodeId, (f32, f32)>) {
        let pins: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| n.pinned)
            .map(|n| n.id.clone())
            .collect();
        let positions = self
            .nodes
            .values()
            .filter(|n| n.pinned)
            .map(|n| (n.id.clone(), n.position))
            .collect();
        (pins, positions)
    }

    /// Set the hovered node (#394): its 1-hop neighborhood stays bright.
    /// No-op when unchanged — the canvas emits only on transitions, and the
    /// cache clears only here.
    pub fn set_hover(&mut self, node_id: Option<NodeId>) {
        if self.hovered == node_id {
            return;
        }
        self.hover_set = match &node_id {
            Some(id) => model::focus_neighborhood(&self.edges, id, 1),
            None => std::collections::HashSet::new(),
        };
        self.hovered = node_id;
        self.cache.clear();
    }

    /// Advance the flow-dash animation (#394). Does NOT clear the canvas
    /// cache — animated dashes draw in the uncached overlay layer.
    pub fn advance_animation(&mut self) {
        self.anim_offset = self.anim_offset.wrapping_add(2);
    }

    /// Whether any rendered edge is worth animating (#394): gates the
    /// animation subscription so idle networks burn no frames.
    pub fn has_animated_edges(&self) -> bool {
        self.render
            .edges
            .iter()
            .any(|e| e.rate + e.reverse_rate >= ANIMATED_EDGE_MIN_RATE)
    }

    /// Toggle the lens legend (#394).
    pub fn toggle_legend(&mut self) {
        self.show_legend = !self.show_legend;
    }

    /// Toggle auto-layout.
    pub fn toggle_auto_layout(&mut self) {
        self.auto_layout = !self.auto_layout;
        if self.auto_layout {
            // Re-enabling re-animates from a fresh cooling alpha.
            self.wake_layout();
        }
    }

    /// Wake the force simulation (#441): full cooling alpha, not stable.
    /// Call whenever the graph structure changes under Force layout.
    pub fn wake_layout(&mut self) {
        self.layout_alpha = 1.0;
        self.layout_stable = false;
    }

    /// Recompute the tiered layout (#442) and move unpinned nodes there —
    /// tweened when `animate`, else immediately. Change-gated: identical
    /// targets are a no-op, so calling this on every data refresh is free.
    pub fn apply_tiered_layout(&mut self, animate: bool) {
        if self.prefs.layout != LayoutMode::Tiered {
            return;
        }
        let layout = tiered_layout(&self.nodes, &self.edges);
        if layout == self.last_tiered {
            return;
        }
        self.tier_bands = layout.bands.clone();
        let targets: HashMap<NodeId, (f32, f32)> = layout
            .positions
            .iter()
            .filter(|(id, _)| self.nodes.get(*id).is_some_and(|n| !n.pinned))
            .map(|(id, pos)| (id.clone(), *pos))
            .collect();
        if animate && !targets.is_empty() {
            // A fresh tween replaces any in-flight one, starting from the
            // current (possibly mid-animation) positions — no rubber-banding.
            let from = targets
                .keys()
                .filter_map(|id| self.nodes.get(id).map(|n| (id.clone(), n.position)))
                .collect();
            self.tween = Some(PositionTween {
                from,
                to: targets,
                started_ms: zensight_common::current_timestamp_millis(),
                duration_ms: 400,
            });
        } else {
            for (id, pos) in &targets {
                if let Some(node) = self.nodes.get_mut(id) {
                    node.position = *pos;
                    node.velocity = (0.0, 0.0);
                }
            }
            self.cache.clear();
        }
        self.last_tiered = layout;
        self.layout_stable = true;
    }

    /// Whether a position tween is in flight (#442): gates the layout-frame
    /// subscription alongside the force simulation.
    pub fn tween_active(&self) -> bool {
        self.tween.is_some()
    }

    /// Advance the in-flight position tween (#442): writes the interpolation
    /// into the live node positions — the canvas resolves them at draw time,
    /// so this is a cheap redraw, never a render-graph rebuild.
    pub fn step_tween(&mut self, now_ms: i64) {
        let Some(tween) = &self.tween else { return };
        let (positions, finished) = tween_at(tween, now_ms);
        for (id, pos) in positions {
            if let Some(node) = self.nodes.get_mut(&id)
                && !node.pinned
            {
                node.position = pos;
                node.velocity = (0.0, 0.0);
            }
        }
        if finished {
            self.tween = None;
        }
        self.cache.clear();
    }

    /// Run layout iterations for smooth convergence. Driven by the gated
    /// ~30 fps `TopologyLayoutFrame` subscription while unstable (#441) —
    /// a few iterations per frame reads as continuous motion, and the time
    /// budget caps the cost on unexpectedly large graphs.
    /// Returns true if the layout is stable.
    pub fn run_layout_step(&mut self) -> bool {
        // Static layout modes (#394) never animate.
        if self.prefs.layout != LayoutMode::Force {
            self.layout_stable = true;
            return true;
        }
        let config = self.layout_config.clone();
        let start = std::time::Instant::now();
        for _ in 0..3 {
            self.layout_stable = layout_step(self, &config);
            if self.layout_stable || start.elapsed() >= std::time::Duration::from_millis(4) {
                break;
            }
        }
        self.layout_stable
    }

    /// Arrange nodes in a circle (useful for initial layout).
    pub fn arrange_in_circle(&mut self, radius: f32) {
        arrange_circle(self, radius);
        self.wake_layout();
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

/// Render the topology view. `entities` and `store` feed the side panel's
/// identity/evidence and sparkline sections (#393).
pub fn topology_view<'a>(
    state: &'a TopologyState,
    entities: &'a crate::entity::EntityStore,
    store: &'a crate::store::MetricStore,
    theme: AppTheme,
) -> Element<'a, Message> {
    let is_dark = matches!(theme, AppTheme::Dark);
    let header = render_header(state);
    let graph = TopologyGraph::view(state, is_dark);

    // Show the node panel, or the edge detail panel (#25/#393), beside the graph.
    let main_content: Element<'a, Message> = if let Some(ref node_id) = state.selected_node {
        if let Some(node) = state.nodes.get(node_id) {
            let panel = panel::node_panel(state, entities, store, node);
            row![graph, panel].spacing(10).into()
        } else {
            graph
        }
    } else if let Some(edge) = state.selected_edge.and_then(|i| state.edges.get(i)) {
        let panel = panel::edge_panel(state, edge);
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

    // Show layout status. "Manual"/"Adjusting" only describe the force
    // simulation; the deterministic layouts are simply what they are (#442).
    let layout_status = if state.tween_active() {
        text("Layout: animating…").size(10)
    } else if state.prefs.layout != LayoutMode::Force {
        text("Layout: Stable").size(10)
    } else if !state.auto_layout {
        text("Layout: Manual").size(10)
    } else if state.layout_stable {
        text("Layout: Stable").size(10)
    } else {
        text("Layout: Adjusting...").size(10)
    };

    // Show search match count if searching (#392): highlight mode counts
    // matches in the rendered graph; hide mode reports what's shown.
    let search_matches = if !state.search_query.is_empty() {
        let highlighted = state.render.nodes.iter().filter(|n| n.highlighted).count();
        let label = match model::parse_search(&state.search_query) {
            model::SearchAction::Hide(_) => format!("{} shown", state.render.nodes.len()),
            _ => format!("{} matches", highlighted),
        };
        Some(text(label).size(10))
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

    // Auto-layout only governs the force simulation (#442): hidden for the
    // deterministic layouts, where it would be a no-op.
    let auto_layout_btn = (state.prefs.layout == LayoutMode::Force).then(|| {
        button(
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
        })
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
        .push(reset_btn);
    if let Some(btn) = auto_layout_btn {
        header = header.push(btn);
    }

    // Second control row (#392): lens selector + edge-label mode.
    let mut lens_row = row![text("Lens:").size(12)]
        .spacing(8)
        .align_y(Alignment::Center);
    for lens in Lens::ALL {
        let btn = button(text(lens.label()).size(12)).style(if state.prefs.lens == lens {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        });
        let btn = if state.prefs.lens == lens {
            btn
        } else {
            btn.on_press(Message::TopologySetLens(lens))
        };
        lens_row = lens_row.push(btn);
    }

    let label_picker = iced::widget::pick_list(
        EdgeLabelMode::ALL,
        Some(state.prefs.edge_label),
        Message::TopologySetEdgeLabel,
    )
    .text_size(12)
    .padding(4);
    lens_row = lens_row
        .push(text("Edge labels:").size(12))
        .push(label_picker);
    lens_row = lens_row.push(
        button(text("Legend").size(12))
            .on_press(Message::TopologyToggleLegend)
            .style(if state.show_legend {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            }),
    );

    // Grouping + filters (#392).
    let grouping_picker = iced::widget::pick_list(
        GroupingMode::ALL,
        Some(state.prefs.grouping),
        Message::TopologySetGrouping,
    )
    .text_size(12)
    .padding(4);
    lens_row = lens_row.push(grouping_picker);
    let layout_picker = iced::widget::pick_list(
        LayoutMode::ALL,
        Some(state.prefs.layout),
        Message::TopologySetLayout,
    )
    .text_size(12)
    .padding(4);
    lens_row = lens_row.push(layout_picker);
    if !state.prefs.expanded_groups.is_empty() {
        lens_row = lens_row.push(
            button(text("Regroup").size(12))
                .on_press(Message::TopologyRegroup)
                .style(iced::widget::button::secondary),
        );
    }

    lens_row = lens_row
        .push(
            iced::widget::checkbox(state.prefs.filters.hide_idle)
                .label("Hide idle")
                .on_toggle(|_| Message::TopologyToggleHideIdle)
                .size(14)
                .text_size(12),
        )
        .push(
            iced::widget::checkbox(state.prefs.filters.hide_passive)
                .label("Hide passive")
                .on_toggle(|_| Message::TopologyToggleHidePassive)
                .size(14)
                .text_size(12),
        )
        .push(
            iced::widget::checkbox(state.prefs.filters.hide_external)
                .label("Hide external")
                .on_toggle(|_| Message::TopologyToggleHideExternal)
                .size(14)
                .text_size(12),
        );

    // Flow-edge cap with an honest truncation label (#392): never silently
    // pretend the capped view is the whole picture.
    let top_n_picker = iced::widget::pick_list(
        TopNChoice::ALL,
        Some(TopNChoice::from_value(state.prefs.filters.top_n)),
        |choice: TopNChoice| Message::TopologySetTopN(choice.value()),
    )
    .text_size(12)
    .padding(4);
    lens_row = lens_row.push(text("Flows:").size(12)).push(top_n_picker);
    let flows_shown = state
        .render
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Flow)
        .count();
    if flows_shown < state.render.total_flow_edges {
        lens_row = lens_row.push(
            text(format!(
                "showing top {flows_shown} of {} flows",
                state.render.total_flow_edges
            ))
            .size(10),
        );
    }

    let mut rows = column![header, lens_row].spacing(8);

    // Lens legend (#394): what the current lens encodes.
    if state.show_legend {
        rows = rows.push(render_legend(state.prefs.lens));
    }

    // Focus breadcrumb (#392).
    if let Some(ref focus) = state.prefs.focus {
        let root_label = state
            .nodes
            .get(&focus.root)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| focus.root.clone());
        let mut focus_row = row![
            text(format!("Focus: {root_label}")).size(12),
            text("hops:").size(11),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        for hops in 1..=3u8 {
            let btn = button(text(format!("{hops}")).size(11)).style(if focus.hops == hops {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            });
            let btn = if focus.hops == hops {
                btn
            } else {
                btn.on_press(Message::TopologySetFocusHops(hops))
            };
            focus_row = focus_row.push(btn);
        }
        focus_row = focus_row.push(
            button(text("Exit focus").size(11))
                .on_press(Message::TopologyExitFocus)
                .style(iced::widget::button::secondary),
        );
        rows = rows.push(focus_row);
    }

    rows.into()
}

/// The lens legend (#394): a compact stack of lines explaining the active
/// lens's encodings.
fn render_legend(lens: Lens) -> Element<'static, Message> {
    let entries: &[&str] = match lens {
        Lens::Traffic => &[
            "fill = role (blue host · green router · amber switch/AP · grey other)",
            "ring = health · arrows = observed direction · width = rate",
            "solid = traffic · dotted = L2 · dashed = gateway",
        ],
        Lens::Security => &[
            "fill = alert severity (red critical · amber warning · blue info)",
            "bright = alerting or wire-only unknown · faded = quiet",
        ],
        Lens::L2 => &["labels = vendor/MAC · dotted = ARP/NDP adjacency · dashed = gateway"],
        Lens::Health => &["fill = health (blue healthy · amber degraded · red down · grey stale)"],
    };
    let mut legend = iced::widget::Column::new().spacing(2);
    for entry in entries {
        legend = legend.push(text(*entry).size(10));
    }
    container(legend).padding(8).into()
}

/// Flow-cap choices for the pick list (#392).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopNChoice {
    N25,
    N50,
    N100,
    All,
}

impl TopNChoice {
    const ALL: [TopNChoice; 4] = [
        TopNChoice::N25,
        TopNChoice::N50,
        TopNChoice::N100,
        TopNChoice::All,
    ];

    fn value(self) -> usize {
        match self {
            TopNChoice::N25 => 25,
            TopNChoice::N50 => 50,
            TopNChoice::N100 => 100,
            TopNChoice::All => 0,
        }
    }

    fn from_value(v: usize) -> Self {
        match v {
            25 => TopNChoice::N25,
            100 => TopNChoice::N100,
            0 => TopNChoice::All,
            _ => TopNChoice::N50,
        }
    }
}

impl std::fmt::Display for TopNChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TopNChoice::N25 => "top 25",
            TopNChoice::N50 => "top 50",
            TopNChoice::N100 => "top 100",
            TopNChoice::All => "all",
        })
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
    fn hover_tracks_neighborhood_and_change_gates() {
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
        state.edges.push(Edge {
            from: "a".to_string(),
            to: "b".to_string(),
            ..Default::default()
        });

        state.set_hover(Some("a".to_string()));
        assert!(state.hover_set.contains("a"));
        assert!(state.hover_set.contains("b"));
        assert!(!state.hover_set.contains("c"));

        state.set_hover(None);
        assert!(state.hover_set.is_empty());
        assert_eq!(state.hovered, None);
    }

    #[test]
    fn apply_batch_rebuilds_once_and_matches_sequential_applies() {
        let mut map = HashMap::new();
        for id in ["a", "b", "gw"] {
            map.insert(id.to_string(), id.to_string());
        }
        let make_state = || {
            let mut state = TopologyState::default();
            for id in ["a", "b", "gw"] {
                let mut node = Node {
                    id: id.to_string(),
                    ..Default::default()
                };
                if id == "a" {
                    // The neighbor table is attributed to netlink hosts.
                    node.protocols.insert(zensight_common::Protocol::Netlink);
                }
                state.nodes.insert(id.to_string(), node);
            }
            state
        };
        let flows = vec![flow("a:1", "b:2", 1000, 10, "tcp")];
        let neighbors = vec![neighbor("gw", true)];
        let matrix = vec![zensight_common::MatrixRecord {
            src: "a".to_string(),
            dst: "b".to_string(),
            bytes_per_sec: 500.0,
            names: Vec::new(),
        }];

        let mut sequential = make_state();
        sequential.apply_flow_edges(&flows, &map, 1);
        sequential.apply_neighbor_edges(&neighbors, &map, 1);
        sequential.apply_matrix_edges(&matrix, &map, 1);

        let mut batched = make_state();
        batched.apply_batch(
            TopologyBatch {
                flows: Some(flows),
                neighbors: Some(neighbors),
                matrix: Some(matrix),
                assets: None,
            },
            &HashMap::new(),
            &map,
            1,
        );

        assert_eq!(batched.edges.len(), sequential.edges.len());
        for edge in &sequential.edges {
            assert!(
                batched.edges.iter().any(|e| ordered_pair(&e.from, &e.to)
                    == ordered_pair(&edge.from, &edge.to)
                    && e.rate == edge.rate
                    && e.bytes == edge.bytes),
                "batched edge set missing {:?}→{:?}",
                edge.from,
                edge.to
            );
        }
        assert_eq!(batched.nodes["gw"].role, NodeRole::Router);
        // A partial batch keeps the remembered inputs: dropping the matrix
        // reply must not wipe the rated edges.
        batched.apply_batch(TopologyBatch::default(), &HashMap::new(), &map, 2);
        assert!(batched.edges.iter().any(|e| e.rate == 500.0));
    }

    #[test]
    fn new_node_does_not_reshuffle_existing_layout() {
        use zensight_common::Protocol;

        let device = |source: &str| {
            let id = DeviceId::new(Protocol::Sysinfo, source);
            (id.clone(), DeviceState::new(id))
        };
        let mut devices: HashMap<DeviceId, DeviceState> = [device("host-a")].into();

        let mut state = TopologyState::default();
        // Force mode: this test pins down the force-layout contract (the
        // tiered default assigns positions itself, #442).
        state.prefs.layout = LayoutMode::Force;
        let entities = crate::entity::EntityStore::default();
        state.update_from_devices(&devices, &entities, &HashMap::new(), 0);
        let seeded = state.nodes["host-a"].position;
        assert_ne!(seeded, (0.0, 0.0), "new nodes seed off-origin");

        // Simulate the settled layout, then a second host appearing.
        state.nodes.get_mut("host-a").unwrap().position = (123.0, -45.0);
        let (id, dev) = device("host-b");
        devices.insert(id, dev);
        state.update_from_devices(&devices, &entities, &HashMap::new(), 0);

        // The existing (unpinned) node stays put; only the newcomer is seeded.
        assert_eq!(state.nodes["host-a"].position, (123.0, -45.0));
        assert_ne!(state.nodes["host-b"].position, (0.0, 0.0));
        assert!(!state.layout_stable, "layout wakes to absorb the newcomer");
    }

    #[test]
    fn tiered_default_applies_positions_and_change_gates() {
        assert_eq!(LayoutMode::default(), LayoutMode::Tiered);

        let mut state = TopologyState::default();
        for (id, role, ip) in [
            ("gw", NodeRole::Router, "10.0.0.1"),
            ("host-a", NodeRole::Host, "10.0.0.2"),
            ("host-b", NodeRole::Host, "10.0.0.3"),
        ] {
            state.nodes.insert(
                id.to_string(),
                Node {
                    id: id.to_string(),
                    label: id.to_string(),
                    role,
                    ips: vec![ip.to_string()],
                    ..Default::default()
                },
            );
        }
        state.nodes.get_mut("host-b").unwrap().pinned = true;
        state.nodes.get_mut("host-b").unwrap().position = (7.0, 7.0);

        // Immediate application: unpinned nodes take their tier slots,
        // pinned ones stay put, bands are published.
        state.apply_tiered_layout(false);
        assert!(!state.tier_bands.is_empty());
        assert!(state.layout_stable);
        let gw_y = state.nodes["gw"].position.1;
        let host_y = state.nodes["host-a"].position.1;
        assert!(gw_y < host_y, "router tier above hosts tier");
        assert_eq!(state.nodes["host-b"].position, (7.0, 7.0), "pin wins");

        // Identical recompute is a no-op: no tween spawned.
        state.apply_tiered_layout(true);
        assert!(!state.tween_active(), "change gate skips identical layouts");

        // A structural change (new node) starts a tween; stepping it past
        // its duration lands the node and parks the tween.
        state.nodes.insert(
            "host-c".to_string(),
            Node {
                id: "host-c".to_string(),
                label: "host-c".to_string(),
                ips: vec!["10.0.0.4".to_string()],
                ..Default::default()
            },
        );
        state.apply_tiered_layout(true);
        assert!(state.tween_active());
        let now = zensight_common::current_timestamp_millis();
        state.step_tween(now + 10_000);
        assert!(!state.tween_active(), "tween finishes and parks");
        let c = state.nodes["host-c"].position;
        assert!(c != (0.0, 0.0), "tween landed the newcomer on its slot");
        assert!(c.1 > gw_y, "newcomer landed below the router tier");
    }

    #[test]
    fn apply_rates_patches_render_without_rebuild() {
        let mut state = TopologyState::default();
        state.nodes.insert(
            "a".to_string(),
            Node {
                id: "a".to_string(),
                ..Default::default()
            },
        );
        state.invalidate();
        let before = state.render.clone();

        let mut rates = HashMap::new();
        rates.insert("a".to_string(), (100.0, 200.0));
        state.apply_rates(&rates);

        assert_eq!(state.render.nodes[0].rx_rate, Some(100.0));
        assert_eq!(state.render.nodes[0].tx_rate, Some(200.0));
        // Everything except the patched rates is identical — no rebuild ran.
        assert_eq!(state.render.nodes.len(), before.nodes.len());
        assert_eq!(state.render.edges, before.edges);

        // Unchanged rates are a no-op (change gate).
        let snapshot = state.render.clone();
        state.apply_rates(&rates);
        assert_eq!(state.render, snapshot);
    }

    #[test]
    fn render_graph_rebuild_is_deterministic() {
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
        state.edges.push(Edge {
            from: "a".to_string(),
            to: "b".to_string(),
            rate: 42.0,
            ..Default::default()
        });
        state.invalidate();
        let first = state.render.clone();
        state.invalidate();
        // Same inputs ⇒ identical render graph — the PartialEq change gate
        // in `invalidate` is meaningful.
        assert_eq!(state.render, first);
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
