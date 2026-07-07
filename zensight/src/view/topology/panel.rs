//! Details-on-demand side panels for the topology view (#393).
//!
//! Replaces the old cramped info boxes with ~320 px panels where every
//! element pivots to its evidence: identity/correlator claims, vitals with
//! sparklines from the tiered store, top talkers, listen sockets, per-flow
//! process attribution (#309 reuse), and community-id copy for external
//! cross-referencing. All widgets — fully simulator-testable.

use iced::widget::{column, container, row, rule, text};
use iced::{Alignment, Element, Length};
use iced_anim::widget::button;

use super::{Edge, EdgeKind, Node, NodeHealth, Provenance, TopologyState, format_rate};
use crate::entity::EntityStore;
use crate::message::{AttributionTarget, Message};
use crate::store::MetricStore;
use crate::view::components::Sparkline;
use crate::view::icons::{self, IconSize};
use crate::view::specialized::attribution;
use crate::view::specialized::fetch::Fetch;
use crate::view::topology::graph::format_bytes;

/// Panel width (#393): wide enough for sparklines + flow rows.
const PANEL_WIDTH: f32 = 320.0;

/// How many rows each list section shows before "…and N more".
const SECTION_ROWS: usize = 6;

/// Small section title.
fn section(label: &str) -> Element<'_, Message> {
    text(label).size(12).into()
}

/// Dim single-line note.
fn note(label: String) -> Element<'static, Message> {
    text(label).size(10).into()
}

/// Generate a simple text-based progress bar.
fn progress_bar(percentage: f64, width: usize) -> String {
    let filled = ((percentage / 100.0) * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "=".repeat(filled), " ".repeat(empty))
}

/// The node detail panel (#393).
pub fn node_panel<'a>(
    state: &'a TopologyState,
    entities: &'a EntityStore,
    store: &'a MetricStore,
    node: &'a Node,
) -> Element<'a, Message> {
    let entity = entities.hosts.get(&node.id);

    // ── Header: icon, label, role/provenance, status ──
    let subtitle = match node.provenance {
        Provenance::Passive => format!("{} · wire-only", node.role.label()),
        Provenance::External => "External traffic aggregate".to_string(),
        Provenance::Monitored => node.role.label().to_string(),
    };
    let header = row![
        icons::protocol_icon(super::model::primary_protocol(node), IconSize::Large),
        column![text(&node.label).size(16), text(subtitle).size(10)].spacing(2)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

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

    let mut items = column![header, status, rule::horizontal(1)].spacing(8);

    // ── Identity & evidence (#393): what the correlator actually knows ──
    items = items.push(section("Identity"));
    if let Some(vendor) = &node.vendor {
        items = items.push(note(format!("Vendor: {vendor}")));
    }
    if let Some(e) = entity {
        if let Some(fqdn) = &e.fqdn {
            items = items.push(note(fqdn.clone()));
        }
        if !e.ips.is_empty() {
            let shown = e.ips.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            let more = e.ips.len().saturating_sub(3);
            items = items.push(note(if more > 0 {
                format!("IPs: {shown} (+{more})")
            } else {
                format!("IPs: {shown}")
            }));
        }
        if !e.macs.is_empty() {
            let shown = e
                .macs
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            items = items.push(note(format!("MACs: {shown}")));
        }
        // Passive-DNS names with provenance (#308).
        for name in e.names.iter().take(3) {
            items = items.push(note(format!("↳ {} ({})", name.name, name.provenance)));
        }
        // Member claims: the correlator's evidence, previously invisible.
        if !e.members.is_empty() {
            items = items.push(section("Correlated from"));
            for m in e.members.iter().take(SECTION_ROWS) {
                items = items.push(note(format!(
                    "{}/{} — {} ({:.0}%)",
                    m.sensor,
                    m.source,
                    m.rule,
                    m.confidence * 100.0
                )));
            }
        }
    } else if !node.ips.is_empty() {
        items = items.push(note(format!("IPs: {}", node.ips.join(", "))));
    }
    if let Some(n) = node.sensor_count {
        items = items.push(note(format!("Seen by {n} sensor(s)")));
    }

    // ── Vitals: gauges + 1 h sparkline from the tiered store ──
    let has_vitals = node.cpu_usage.is_some() || node.memory_usage.is_some();
    if has_vitals {
        items = items.push(rule::horizontal(1));
        items = items.push(section("Vitals"));
        if let Some(cpu) = node.cpu_usage {
            items = items.push(note(format!("CPU: {:.1}% {}", cpu, progress_bar(cpu, 16))));
        }
        if let Some(mem) = node.memory_usage {
            items = items.push(note(format!("Mem: {:.1}% {}", mem, progress_bar(mem, 16))));
        }
        // Sparkline over the sysinfo facet's hot ring (#393). The store keys
        // by (protocol/source), so resolve the member source.
        if let Some(values) = vitals_sparkline_values(entities, store, node) {
            items = items.push(
                row![
                    text("cpu 1h").size(9),
                    Sparkline::new(values).with_size(180.0, 22.0).view()
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
    }

    // ── Network: live rates + totals ──
    if node.rx_rate.is_some() || node.network_rx.is_some() {
        items = items.push(rule::horizontal(1));
        items = items.push(section("Network I/O"));
        if let (Some(rx), Some(tx)) = (node.rx_rate, node.tx_rate) {
            items = items.push(note(format!(
                "↓ {}  ↑ {}",
                format_rate(rx),
                format_rate(tx)
            )));
        }
        if let (Some(rx), Some(tx)) = (node.network_rx, node.network_tx) {
            items = items.push(note(format!(
                "Total: {} in, {} out",
                format_bytes(rx),
                format_bytes(tx)
            )));
        }
    }

    // ── Top talkers from the remembered traffic matrix (#393) ──
    let talkers = top_talkers(state, node);
    if !talkers.is_empty() {
        items = items.push(rule::horizontal(1));
        items = items.push(section("Top talkers"));
        for (peer, rate) in talkers.into_iter().take(5) {
            items = items.push(note(format!("{peer} — {}", format_rate(rate))));
        }
        items = items.push(
            button(text("Open flow table ↗").size(11))
                .on_press(Message::TopologyOpenFlows)
                .style(iced::widget::button::secondary)
                .width(Length::Fill),
        );
    }

    // ── Listening sockets (fetched on selection, #393) ──
    if node.protocols.contains(&zensight_common::Protocol::Netlink) {
        items = items.push(rule::horizontal(1));
        items = items.push(section("Listening"));
        match &state.panel.listen {
            Fetch::Idle => {}
            Fetch::Loading => items = items.push(note("Loading…".to_string())),
            Fetch::Error(e) => items = items.push(note(format!("Unavailable: {e}"))),
            Fetch::Ready(rows) if rows.is_empty() => {
                items = items.push(note("No listening sockets".to_string()));
            }
            Fetch::Ready(rows) => {
                let mut sorted: Vec<_> = rows.iter().collect();
                sorted.sort_by_key(|s| listen_port(&s.local));
                let total = sorted.len();
                for sock in sorted.into_iter().take(SECTION_ROWS) {
                    let process = sock.process.as_deref().unwrap_or("?");
                    items =
                        items.push(note(format!(":{} · {}", listen_port(&sock.local), process)));
                }
                if total > SECTION_ROWS {
                    items = items.push(note(format!("…and {} more", total - SECTION_ROWS)));
                }
                // Wildcard listeners can't be host-attributed on a multi-host
                // mesh — say so instead of pretending (#393).
                let netlink_hosts = state
                    .nodes
                    .values()
                    .filter(|n| n.protocols.contains(&zensight_common::Protocol::Netlink))
                    .count();
                if netlink_hosts > 1 {
                    items = items.push(note("wildcard listeners may span hosts".to_string()));
                }
            }
        }
    }

    // ── Alerts ──
    if !node.alerts.is_empty() {
        use crate::view::alerts::Severity;
        items = items.push(rule::horizontal(1));
        items = items.push(section("Alerts"));
        for a in node.alerts.iter().take(SECTION_ROWS) {
            let color = Severity::from(a.severity).color();
            items = items.push(
                text(format!("● [{}] {} — {}", a.severity, a.rule, a.summary))
                    .size(10)
                    .style(move |_: &iced::Theme| iced::widget::text::Style { color: Some(color) }),
            );
        }
    }

    // ── Actions ──
    items = items.push(rule::horizontal(1));
    if node.pinned {
        items = items.push(note("Position pinned".to_string()));
    }
    items = items.push(
        button(
            row![
                icons::arrow_right(IconSize::Small),
                text("View Device Details").size(11)
            ]
            .spacing(5)
            .align_y(Alignment::Center),
        )
        .on_press(Message::TopologyViewDeviceDetail(node.id.clone()))
        .style(iced::widget::button::primary)
        .width(Length::Fill),
    );
    items = items.push(
        button(text("Focus").size(11))
            .on_press(Message::TopologyFocusNode(node.id.clone()))
            .style(iced::widget::button::secondary)
            .width(Length::Fill),
    );
    items = items.push(
        button(text("Clear Selection").size(11))
            .on_press(Message::TopologyClearSelection)
            .style(iced::widget::button::secondary)
            .width(Length::Fill),
    );

    container(iced::widget::scrollable(items.padding(2)))
        .padding(12)
        .width(Length::Fixed(PANEL_WIDTH))
        .style(container::rounded_box)
        .into()
}

/// The edge detail panel (#393): per-direction rates, backing flows with
/// process attribution, community-id copy.
pub fn edge_panel<'a>(state: &'a TopologyState, edge: &'a Edge) -> Element<'a, Message> {
    use crate::view::formatting::format_timestamp;

    let from_label = node_label(state, &edge.from);
    let to_label = node_label(state, &edge.to);
    let header = row![
        icons::network(IconSize::Large),
        text(format!("{from_label} ⇄ {to_label}")).size(15),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let kind_label = match edge.kind {
        EdgeKind::Flow => "Observed traffic",
        EdgeKind::L2Adjacency => "L2 adjacency (ARP/NDP)",
        EdgeKind::Gateway => "Default gateway",
    };

    let mut items = column![header, rule::horizontal(1), section(kind_label)].spacing(8);

    // ── Per-direction rates (#391 data, finally on display) ──
    if edge.rate + edge.reverse_rate > 0.0 {
        items = items.push(note(format!(
            "→ {}   ← {}",
            format_rate(edge.rate),
            format_rate(edge.reverse_rate)
        )));
    }
    if edge.bytes > 0 {
        items = items.push(note(format!(
            "Total: {} · {} pkts",
            format_bytes(edge.bytes),
            edge.packets
        )));
    }
    if let Some(proto) = &edge.protocol {
        items = items.push(note(format!("Protocol: {proto}")));
    }
    items = items.push(note(format!(
        "Last seen: {}",
        format_timestamp(edge.last_seen)
    )));

    // ── Backing flows + process attribution (#309 reuse) ──
    if edge.kind == EdgeKind::Flow {
        items = items.push(rule::horizontal(1));
        items = items.push(section("Flows"));
        match &state.panel.edge_flows {
            Fetch::Idle => {}
            Fetch::Loading => items = items.push(note("Loading…".to_string())),
            Fetch::Error(e) => items = items.push(note(format!("Unavailable: {e}"))),
            Fetch::Ready(flows) if flows.is_empty() => {
                items = items.push(note("No recent flows for this pair".to_string()));
            }
            Fetch::Ready(flows) => {
                let total = flows.len();
                for flow in flows.iter().take(SECTION_ROWS) {
                    let key = attribution::flow_key(&flow.src, &flow.dst);
                    let attributed = state.panel.attribution.as_ref().filter(|(k, _)| *k == key);
                    let flow_line = format!(
                        "{} → {} · {} {}",
                        flow.src,
                        flow.dst,
                        format_bytes(flow.bytes),
                        flow.proto
                    );
                    match attributed.map(|(_, fetch)| fetch) {
                        // Attributed: show the owning process inline.
                        Some(Fetch::Ready(Some(process))) => {
                            items = items.push(note(flow_line));
                            items = items.push(note(format!("   ⚙ {}", process.display())));
                        }
                        Some(Fetch::Ready(None)) => {
                            items = items.push(note(flow_line));
                            items = items.push(note("   ⚙ no owning process found".to_string()));
                        }
                        Some(Fetch::Loading) => {
                            items = items.push(note(flow_line));
                            items = items.push(note("   ⚙ attributing…".to_string()));
                        }
                        Some(Fetch::Error(e)) => {
                            items = items.push(note(flow_line));
                            items = items.push(note(format!("   ⚙ {e}")));
                        }
                        _ => {
                            items = items.push(
                                row![
                                    text(flow_line).size(10),
                                    button(text("attr").size(9))
                                        .on_press(Message::FetchFlowAttribution {
                                            target: AttributionTarget::Topology,
                                            key,
                                            src: flow.src.clone(),
                                            dst: flow.dst.clone(),
                                        })
                                        .style(iced::widget::button::secondary)
                                ]
                                .spacing(6)
                                .align_y(Alignment::Center),
                            );
                        }
                    }
                    // Community ID: the cross-tool flow key (Zeek/Suricata).
                    if let Some(cid) = &flow.community_id {
                        items = items.push(
                            row![
                                text(format!("   {cid}")).size(9),
                                button(text("copy").size(9))
                                    .on_press(Message::TopologyCopyText(cid.clone()))
                                    .style(iced::widget::button::secondary)
                            ]
                            .spacing(6)
                            .align_y(Alignment::Center),
                        );
                    }
                }
                if total > SECTION_ROWS {
                    items = items.push(note(format!("…and {} more", total - SECTION_ROWS)));
                }
            }
        }
        items = items.push(
            button(text("Open flow table ↗").size(11))
                .on_press(Message::TopologyOpenFlows)
                .style(iced::widget::button::secondary)
                .width(Length::Fill),
        );
    }

    items = items.push(rule::horizontal(1));
    items = items.push(
        button(text("Clear Selection").size(11))
            .on_press(Message::TopologyClearSelection)
            .style(iced::widget::button::secondary)
            .width(Length::Fill),
    );

    container(iced::widget::scrollable(items.padding(2)))
        .padding(12)
        .width(Length::Fixed(PANEL_WIDTH))
        .style(container::rounded_box)
        .into()
}

/// Display label for a node id (falls back to the raw id).
fn node_label(state: &TopologyState, id: &str) -> String {
    state
        .nodes
        .get(id)
        .map(|n| n.label.clone())
        .unwrap_or_else(|| id.to_string())
}

/// The local port of a listen socket ("0.0.0.0:22" → 22).
fn listen_port(local: &str) -> u16 {
    local
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// Top peers by rate from the remembered traffic matrix (#393): rows touching
/// any of the node's IPs, labeled with the peer node when known.
fn top_talkers(state: &TopologyState, node: &Node) -> Vec<(String, f64)> {
    use std::collections::HashMap;
    let my_ips: std::collections::HashSet<&str> = node.ips.iter().map(String::as_str).collect();
    if my_ips.is_empty() {
        return Vec::new();
    }
    // ip → node label for peer resolution.
    let mut ip_label: HashMap<&str, &str> = HashMap::new();
    for n in state.nodes.values() {
        for ip in &n.ips {
            ip_label.entry(ip.as_str()).or_insert(n.label.as_str());
        }
    }
    let mut acc: HashMap<String, f64> = HashMap::new();
    for rec in state.matrix() {
        let src = super::endpoint_ip(&rec.src);
        let dst = super::endpoint_ip(&rec.dst);
        let peer = if my_ips.contains(src) && !my_ips.contains(dst) {
            dst
        } else if my_ips.contains(dst) && !my_ips.contains(src) {
            src
        } else {
            continue;
        };
        let label = ip_label.get(peer).map_or(peer, |l| *l).to_string();
        *acc.entry(label).or_insert(0.0) += rec.bytes_per_sec;
    }
    let mut talkers: Vec<(String, f64)> = acc.into_iter().collect();
    talkers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    talkers
}

/// CPU sparkline values for the node's sysinfo facet, last hour of the hot
/// ring downsampled to a drawable width.
fn vitals_sparkline_values(
    entities: &EntityStore,
    store: &MetricStore,
    node: &Node,
) -> Option<Vec<f64>> {
    // Resolve the sysinfo source: entity member first, else the node id
    // itself (source-keyed nodes).
    let source = entities
        .hosts
        .get(&node.id)
        .and_then(|e| {
            e.members
                .iter()
                .find(|m| m.sensor == "sysinfo")
                .map(|m| m.source.clone())
        })
        .unwrap_or_else(|| node.id.clone());
    let samples = store.hot_samples(&format!("sysinfo/{source}|cpu/usage"));
    if samples.len() < 2 {
        return None;
    }
    // Downsample to ~90 points so the sparkline stays cheap.
    let stride = (samples.len() / 90).max(1);
    Some(samples.iter().step_by(stride).map(|s| s.value).collect())
}
