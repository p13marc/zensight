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
    let mut content = column![
        render_header(state),
        card(render_diagnostics(state)),
        card(render_interfaces(state)),
        card(render_sockets(state)),
        card(render_neighbors(state)),
        card(render_routes(state)),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    // Conntrack + WireGuard are environment-specific (NAT gateway / VPN host),
    // so only show their cards when the host actually publishes them.
    if has_prefix(state, "conntrack/") {
        content = content.push(card(render_conntrack(state)));
    }
    if has_prefix(state, "wireguard/") {
        content = content.push(card(render_wireguard(state)));
    }
    if has_prefix(state, "xfrm/") {
        content = content.push(card(render_xfrm(state)));
    }

    content = content.push(card(render_detail(state)));

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

    // Header row. Now includes packets + errors columns (#46) — carrier-vs-admin
    // state and error counters were previously dropped at render.
    let mut list = Column::new().spacing(4).push(
        row![
            cell("interface", 130),
            cell("state", 70),
            cell("mtu", 60),
            cell("rx bytes", 110),
            cell("tx bytes", 110),
            cell("rx pkts", 100),
            cell("tx pkts", 100),
            cell("rx drop", 80),
            cell("tx drop", 80),
            cell("rx err", 80),
            cell("tx err", 80),
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
                cell(name, 130),
                cell(&st, 70),
                cell(&num(stats.get("mtu").copied()), 60),
                cell(&num(stats.get("rx_bytes").copied()), 110),
                cell(&num(stats.get("tx_bytes").copied()), 110),
                cell(&num(stats.get("rx_packets").copied()), 100),
                cell(&num(stats.get("tx_packets").copied()), 100),
                cell(&num(stats.get("rx_dropped").copied()), 80),
                cell(&num(stats.get("tx_dropped").copied()), 80),
                cell(&num(stats.get("rx_errors").copied()), 80),
                cell(&num(stats.get("tx_errors").copied()), 80),
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

    let mut col = column![
        title,
        line("established", "sockets/tcp/established"),
        line("listen", "sockets/tcp/listen"),
        line("time_wait", "sockets/tcp/time_wait"),
        line("syn_sent", "sockets/tcp/syn_sent"),
        line("close_wait", "sockets/tcp/close_wait"),
        line("retransmits (total)", "sockets/tcp/retransmits_total"),
        // RTT percentiles, not just max (#46).
        line("RTT p50 (us)", "sockets/tcp/rtt_p50_us"),
        line("RTT p95 (us)", "sockets/tcp/rtt_p95_us"),
        line("max RTT (us)", "sockets/tcp/max_rtt_us"),
        // Socket memory buffers (#46).
        line("snd buf (total)", "sockets/tcp/mem/snd_buf_total"),
        line("rcv buf (total)", "sockets/tcp/mem/rcv_buf_total"),
    ]
    .spacing(4);

    // Congestion-control algorithm distribution (#46): dynamic `by_cong/<algo>`.
    let mut congs: Vec<(String, String)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            let algo = m.strip_prefix("sockets/tcp/by_cong/")?;
            Some((algo.to_string(), num(Some(&p.value))))
        })
        .collect();
    congs.sort();
    if !congs.is_empty() {
        col = col.push(text("congestion algorithms").size(font::CAPTION).style(dim));
        for (algo, count) in congs {
            col = col.push(row![cell(&format!("  {algo}"), 180), cell(&count, 100)].spacing(8));
        }
    }

    col.into()
}

/// IPsec / xfrm SA + policy summary (#46). Only present on hosts running IPsec.
fn render_xfrm(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("IPsec / xfrm", None);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 100)].spacing(8);

    let mut col = column![
        title,
        line("SAs (total)", "xfrm/sa/total"),
        line("policies (total)", "xfrm/policy/total"),
    ]
    .spacing(4);

    // Dynamic by_proto / by_mode breakdowns.
    for (prefix, heading) in [
        ("xfrm/sa/by_proto/", "by proto"),
        ("xfrm/sa/by_mode/", "by mode"),
    ] {
        let mut items: Vec<(String, String)> = state
            .metrics
            .iter()
            .filter_map(|(m, p)| {
                let k = m.strip_prefix(prefix)?;
                Some((k.to_string(), num(Some(&p.value))))
            })
            .collect();
        items.sort();
        if !items.is_empty() {
            col = col.push(text(heading).size(font::CAPTION).style(dim));
            for (k, v) in items {
                col = col.push(row![cell(&format!("  {k}"), 180), cell(&v, 100)].spacing(8));
            }
        }
    }
    col.into()
}

/// Diagnostics summary: bottleneck score + issue counts (from the nlink scan).
fn render_diagnostics(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Diagnostics", None);
    if !state.metrics.keys().any(|k| k.starts_with("diagnostics/")) {
        return column![title, empty_state("No diagnostics data", None)]
            .spacing(space::SM)
            .into();
    }
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 120)].spacing(8);

    // Bottleneck score gets a trend sparkline (#44) — it's the headline signal.
    let bottleneck_line = row![
        cell("bottleneck score", 180),
        cell(&get("diagnostics/bottleneck_score"), 120),
        super::metric_trend_and_alert(state, "diagnostics/bottleneck_score"),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let mut col = column![
        title,
        bottleneck_line,
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
            .push(
                row![
                    cell("bottleneck", 180),
                    cell(&format!("{kind} @ {loc}"), 360)
                ]
                .spacing(8),
            )
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
    let title = section_header("On-demand Detail", None);
    let d = &state.netlink_detail;

    // A fetch button per topic; disabled (and relabelled) while that topic loads.
    let fetch = |topic: NetlinkDetailTopic, loading: bool| {
        let label = if loading {
            format!("Fetching {}…", topic.label())
        } else {
            format!("Fetch {}", topic.label())
        };
        let mut b = button(text(label).size(font::CAPTION)).padding([4, 10]);
        if !loading {
            b = b.on_press(Message::FetchNetlinkDetail(topic));
        }
        b
    };
    let buttons = row![
        fetch(NetlinkDetailTopic::Sockets, d.sockets.is_loading()),
        fetch(NetlinkDetailTopic::Routes, d.routes.is_loading()),
        fetch(NetlinkDetailTopic::Neighbors, d.neighbors.is_loading()),
    ]
    .spacing(space::SM);

    let mut col = column![title, buttons].spacing(space::SM);

    // Sockets
    if let Some(err) = d.sockets.error() {
        col = col.push(empty_state(format!("Sockets fetch failed: {err}"), None));
    } else if let Some(socks) = d.sockets.ready() {
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
            .push(text(format!("Sockets ({})", socks.len())).size(font::EMPHASIS))
            .push(list);
    }

    // Routes
    if let Some(err) = d.routes.error() {
        col = col.push(empty_state(format!("Routes fetch failed: {err}"), None));
    } else if let Some(routes) = d.routes.ready() {
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
            .push(text(format!("Routes ({})", routes.len())).size(font::EMPHASIS))
            .push(list);
    }

    // Neighbors
    if let Some(err) = d.neighbors.error() {
        col = col.push(empty_state(format!("Neighbors fetch failed: {err}"), None));
    } else if let Some(neighbors) = d.neighbors.ready() {
        let mut list = Column::new()
            .spacing(3)
            .push(row![cell("ip", 200), cell("mac", 200), cell("state", 120)].spacing(8));
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
            .push(text(format!("Neighbors ({})", neighbors.len())).size(font::EMPHASIS))
            .push(list);
    }

    col.into()
}

/// Conntrack (NAT/flow-table health) section.
fn render_conntrack(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Conntrack", None);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 120)].spacing(8);

    let mut col = column![
        title,
        line("entries", "conntrack/entries"),
        line("tcp", "conntrack/by_proto/tcp"),
        line("udp", "conntrack/by_proto/udp"),
        line("icmp", "conntrack/by_proto/icmp"),
        line("other", "conntrack/by_proto/other"),
        line("max", "conntrack/max"),
    ]
    .spacing(4);

    // Utilization is a 0..1 ratio — render as a percentage (num() would floor it).
    if let Some(TelemetryValue::Gauge(u)) =
        state.metrics.get("conntrack/utilization").map(|p| &p.value)
    {
        col = col.push(
            row![
                cell("utilization", 180),
                cell(&format!("{:.1}%", u * 100.0), 120)
            ]
            .spacing(8),
        );
    }
    col.into()
}

/// WireGuard peers section: one sub-table per WG interface.
fn render_wireguard(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut col = column![section_header("WireGuard", None)].spacing(space::SM);
    for (iface, peers) in wireguard(state) {
        let count = num(state
            .metrics
            .get(&format!("wireguard/{iface}/peers"))
            .map(|p| &p.value));
        col = col.push(text(format!("{iface} — {count} peers")).size(font::EMPHASIS));
        let mut list = Column::new().spacing(3).push(
            row![
                cell("peer", 110),
                cell("endpoint", 190),
                cell("handshake", 110),
                cell("rx", 90),
                cell("tx", 90),
                cell("up", 50),
            ]
            .spacing(8),
        );
        for (peer, stats) in peers {
            let endpoint = stats
                .get("rx_bytes")
                .and_then(|p| p.labels.get("endpoint"))
                .cloned()
                .unwrap_or_else(|| "-".into());
            let g = |s: &str| num(stats.get(s).map(|p| &p.value));
            let handshake = match stats.get("last_handshake_age_s").map(|p| &p.value) {
                Some(TelemetryValue::Gauge(a)) => format!("{a:.0}s ago"),
                _ => "never".into(),
            };
            let up = matches!(
                stats.get("up").map(|p| &p.value),
                Some(TelemetryValue::Boolean(true))
            );
            list = list.push(
                row![
                    cell(&peer, 110),
                    cell(&endpoint, 190),
                    cell(&handshake, 110),
                    cell(&g("rx_bytes"), 90),
                    cell(&g("tx_bytes"), 90),
                    cell(if up { "yes" } else { "no" }, 50),
                ]
                .spacing(8),
            );
        }
        col = col.push(list);
    }
    col.into()
}

/// Group `wireguard/<iface>/<peer>/<stat>` metrics by interface then peer.
type WgPeers<'a> = std::collections::BTreeMap<
    String,
    std::collections::BTreeMap<String, &'a zensight_common::TelemetryPoint>,
>;
fn wireguard(state: &DeviceDetailState) -> std::collections::BTreeMap<String, WgPeers<'_>> {
    let mut map: std::collections::BTreeMap<String, WgPeers<'_>> = Default::default();
    for (metric, point) in &state.metrics {
        let Some(rest) = metric.strip_prefix("wireguard/") else {
            continue;
        };
        // `<iface>/peers` is the count (no peer segment) — skip here.
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if let [iface, peer, stat] = parts.as_slice() {
            map.entry(iface.to_string())
                .or_default()
                .entry(peer.to_string())
                .or_default()
                .insert(stat.to_string(), point);
        }
    }
    map
}

fn has_prefix(state: &DeviceDetailState, prefix: &str) -> bool {
    state.metrics.keys().any(|k| k.starts_with(prefix))
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
