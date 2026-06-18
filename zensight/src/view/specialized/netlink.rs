//! Netlink host specialized view — interfaces + TCP socket aggregates.

use std::collections::BTreeMap;

use iced::widget::{Column, button, column, container, row, scrollable, text};
use iced::{Element, Length, Theme};
use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::components::{card, empty_state, section_header};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::netlink_detail::NetlinkDetailTopic;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the netlink host specialized view.
pub fn netlink_host_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let content = column![
        render_header(state),
        card(render_diagnostics(state)),
        card(render_interfaces(state)),
        card(render_sockets(state)),
        card(render_neighbors(state)),
        card(render_routes(state)),
        card(render_detail(state)),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    row![
        text(format!("Netlink: {}", state.device_id.source)).size(font::TITLE),
        text(format!("({} metrics)", state.metrics.len()))
            .size(font::CAPTION)
            .style(dim),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center)
    .into()
}

/// Group `iface/<name>/<stat>` metrics by interface name.
fn interfaces(state: &DeviceDetailState) -> BTreeMap<String, BTreeMap<String, &TelemetryValue>> {
    let mut map: BTreeMap<String, BTreeMap<String, &TelemetryValue>> = BTreeMap::new();
    for (metric, point) in &state.metrics {
        if let Some(rest) = metric.strip_prefix("iface/")
            && let Some((name, stat)) = rest.split_once('/')
        {
            map.entry(name.to_string())
                .or_default()
                .insert(stat.to_string(), &point.value);
        }
    }
    map
}

fn render_interfaces(state: &DeviceDetailState) -> Element<'_, Message> {
    let ifaces = interfaces(state);
    let title = section_header(format!("Interfaces ({})", ifaces.len()), None);

    if ifaces.is_empty() {
        return column![title, empty_state("No interface data", None)]
            .spacing(space::SM)
            .into();
    }

    // Header row.
    let mut list = Column::new().spacing(4).push(
        row![
            cell("interface", 140),
            cell("state", 80),
            cell("mtu", 70),
            cell("rx bytes", 120),
            cell("tx bytes", 120),
            cell("rx drop", 90),
            cell("tx drop", 90),
        ]
        .spacing(8),
    );

    for (name, stats) in &ifaces {
        let st = stats
            .get("oper_state")
            .and_then(text_val)
            .unwrap_or_else(|| match stats.get("up").and_then(bool_val) {
                Some(true) => "up".to_string(),
                Some(false) => "down".to_string(),
                None => "-".into(),
            });
        list = list.push(
            row![
                cell(name, 140),
                cell(&st, 80),
                cell(&num(stats.get("mtu").copied()), 70),
                cell(&num(stats.get("rx_bytes").copied()), 120),
                cell(&num(stats.get("tx_bytes").copied()), 120),
                cell(&num(stats.get("rx_dropped").copied()), 90),
                cell(&num(stats.get("tx_dropped").copied()), 90),
            ]
            .spacing(8),
        );
    }

    column![title, list].spacing(8).into()
}

fn render_sockets(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("TCP Sockets", None);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 100)].spacing(8);

    if !state.metrics.keys().any(|k| k.starts_with("sockets/tcp/")) {
        return column![title, empty_state("No socket data", None)]
            .spacing(space::SM)
            .into();
    }

    column![
        title,
        line("established", "sockets/tcp/established"),
        line("listen", "sockets/tcp/listen"),
        line("time_wait", "sockets/tcp/time_wait"),
        line("syn_sent", "sockets/tcp/syn_sent"),
        line("close_wait", "sockets/tcp/close_wait"),
        line("retransmits (total)", "sockets/tcp/retransmits_total"),
        line("max RTT (us)", "sockets/tcp/max_rtt_us"),
    ]
    .spacing(4)
    .into()
}

/// Diagnostics summary: bottleneck score + issue counts (from the nlink scan).
fn render_diagnostics(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Diagnostics", None);
    if !state
        .metrics
        .keys()
        .any(|k| k.starts_with("diagnostics/"))
    {
        return column![title, empty_state("No diagnostics data", None)]
            .spacing(space::SM)
            .into();
    }
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 120)].spacing(8);

    let mut col = column![
        title,
        line("bottleneck score", "diagnostics/bottleneck_score"),
        line("issues (total)", "diagnostics/issues/total"),
        line("  critical", "diagnostics/issues/critical"),
        line("  error", "diagnostics/issues/error"),
        line("  warning", "diagnostics/issues/warning"),
        line("  info", "diagnostics/issues/info"),
    ]
    .spacing(4);

    // The worst bottleneck (if any) carries its location/recommendation as labels.
    if let Some(point) = state.metrics.get("diagnostics/bottleneck") {
        let kind = match &point.value {
            TelemetryValue::Text(s) => s.clone(),
            _ => "-".into(),
        };
        let loc = point.labels.get("location").cloned().unwrap_or_default();
        let rec = point
            .labels
            .get("recommendation")
            .cloned()
            .unwrap_or_default();
        col = col
            .push(row![cell("bottleneck", 180), cell(&format!("{kind} @ {loc}"), 360)].spacing(8))
            .push(row![cell("  recommendation", 180), cell(&rec, 360)].spacing(8));
    }
    col.into()
}

/// ARP/NDP neighbor state summary.
fn render_neighbors(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Neighbors (ARP/NDP)", None);
    if !state.metrics.keys().any(|k| k.starts_with("neighbors/")) {
        return column![title, empty_state("No neighbor data", None)]
            .spacing(space::SM)
            .into();
    }
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 100)].spacing(8);
    column![
        title,
        line("total", "neighbors/total"),
        line("reachable", "neighbors/by_state/reachable"),
        line("stale", "neighbors/by_state/stale"),
        line("failed", "neighbors/by_state/failed"),
        line("incomplete", "neighbors/by_state/incomplete"),
        line("permanent", "neighbors/by_state/permanent"),
    ]
    .spacing(4)
    .into()
}

/// Routing-table summary.
fn render_routes(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Routes", None);
    if !state.metrics.keys().any(|k| k.starts_with("routes/")) {
        return column![title, empty_state("No route data", None)]
            .spacing(space::SM)
            .into();
    }
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 160)].spacing(8);
    column![
        title,
        line("IPv4 routes", "routes/ipv4_count"),
        line("IPv6 routes", "routes/ipv6_count"),
        line("default route (v4)", "routes/default_v4_present"),
        line("default gateway (v4)", "routes/default_v4_gw"),
    ]
    .spacing(4)
    .into()
}

/// On-demand detail: fetch buttons + the fetched full tables (P2 — pulled from
/// the sensor's `@/query/*` channels only when the user asks, never streamed).
fn render_detail(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = text("On-demand Detail").size(18);
    let fetch = |topic: NetlinkDetailTopic| {
        button(text(format!("Fetch {}", topic.label())).size(12))
            .on_press(Message::FetchNetlinkDetail(topic))
            .padding([4, 10])
    };
    let buttons = row![
        fetch(NetlinkDetailTopic::Sockets),
        fetch(NetlinkDetailTopic::Routes),
        fetch(NetlinkDetailTopic::Neighbors),
    ]
    .spacing(8);

    let mut col = column![title, buttons].spacing(10);
    let d = &state.netlink_detail;

    if let Some(socks) = &d.sockets {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("local", 200),
                cell("remote", 200),
                cell("state", 110),
                cell("rtt_us", 80),
                cell("retx", 60),
            ]
            .spacing(8),
        );
        for s in socks.iter().take(200) {
            list = list.push(
                row![
                    cell(&s.local, 200),
                    cell(&s.remote, 200),
                    cell(&s.state, 110),
                    cell(&s.rtt_us.to_string(), 80),
                    cell(&s.retrans.to_string(), 60),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("Sockets ({})", socks.len())).size(15))
            .push(list);
    }

    if let Some(routes) = &d.routes {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("destination", 220),
                cell("gateway", 160),
                cell("proto", 90),
                cell("scope", 90),
            ]
            .spacing(8),
        );
        for r in routes.iter().take(200) {
            list = list.push(
                row![
                    cell(&r.dst, 220),
                    cell(r.gateway.as_deref().unwrap_or("-"), 160),
                    cell(&r.protocol, 90),
                    cell(&r.scope, 90),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("Routes ({})", routes.len())).size(15))
            .push(list);
    }

    if let Some(neighbors) = &d.neighbors {
        let mut list = Column::new().spacing(3).push(
            row![cell("ip", 200), cell("mac", 200), cell("state", 120)].spacing(8),
        );
        for n in neighbors.iter().take(200) {
            list = list.push(
                row![
                    cell(n.ip.as_deref().unwrap_or("-"), 200),
                    cell(n.mac.as_deref().unwrap_or("-"), 200),
                    cell(&n.state, 120),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("Neighbors ({})", neighbors.len())).size(15))
            .push(list);
    }

    col.into()
}

// ---- small helpers ----------------------------------------------------------

fn cell<'a>(s: &str, width: u16) -> Element<'a, Message> {
    text(s.to_string())
        .size(12)
        .width(Length::Fixed(width as f32))
        .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

fn text_val(v: &&TelemetryValue) -> Option<String> {
    match v {
        TelemetryValue::Text(s) => Some(s.clone()),
        _ => None,
    }
}

fn bool_val(v: &&TelemetryValue) -> Option<bool> {
    match v {
        TelemetryValue::Boolean(b) => Some(*b),
        _ => None,
    }
}

fn num(v: Option<&TelemetryValue>) -> String {
    match v {
        Some(TelemetryValue::Counter(c)) => c.to_string(),
        Some(TelemetryValue::Gauge(g)) => format!("{g:.0}"),
        Some(TelemetryValue::Text(s)) => s.clone(),
        Some(TelemetryValue::Boolean(b)) => b.to_string(),
        _ => "-".into(),
    }
}
