//! Netlink host specialized view — interfaces + TCP socket aggregates.

use std::collections::BTreeMap;

use iced::widget::{Column, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};
use zensight_common::{SocketRecord, TelemetryValue};

use crate::message::Message;
use crate::view::components::{card, empty_state, section_header};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::netlink_detail::{
    NetlinkDetailState, NetlinkDetailTopic, SocketSort, filter_sort_sockets,
};
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
    if has_prefix(state, "tc/") {
        content = content.push(card(render_tc(state)));
    }
    if has_prefix(state, "xfrm/") || has_prefix(state, "events/ipsec/") {
        content = content.push(card(render_xfrm(state)));
    }
    if has_prefix(state, "ethtool/") {
        content = content.push(card(render_ethtool(state)));
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

    // Real-time SA/policy lifecycle counters (nlink 0.23 XFRM monitor stream),
    // folded onto the control-plane timeline as the `ipsec` family. Surfacing
    // them here ties the live signal (rekeys, soft/hard expiries, acquires) to
    // the IPsec panel — the periodic SA snapshot above misses churn between
    // poll ticks. Individual events show in the control-plane timeline.
    let ev = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let has_ev = ["added", "changed", "removed"].iter().any(|a| {
        state
            .metrics
            .contains_key(&format!("events/ipsec/{a}_total"))
    });
    if has_ev {
        col = col.push(text("lifecycle events").size(font::CAPTION).style(dim));
        for (label, action) in [
            ("  added (new SA/policy)", "added"),
            ("  changed (soft-expire/acquire)", "changed"),
            ("  removed (del/hard-expire)", "removed"),
        ] {
            col = col.push(
                row![
                    cell(label, 220),
                    cell(&ev(&format!("events/ipsec/{action}_total")), 100)
                ]
                .spacing(8),
            );
        }
    }
    col.into()
}

/// TC / QoS qdisc panel (#46): per-qdisc drops/overlimits/requeues/backlog from
/// streamed `tc/<iface>/<kind>/<stat>` metrics — the egress-congestion signal.
/// Only present when the sensor has TC collection enabled.
fn render_tc(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("TC / QoS qdiscs", None);

    // Group tc/<iface>/<kind>/<stat> by (iface, kind).
    let mut qdiscs: BTreeMap<(String, String), BTreeMap<String, &TelemetryValue>> = BTreeMap::new();
    for (metric, point) in &state.metrics {
        if let Some(rest) = metric.strip_prefix("tc/") {
            let parts: Vec<&str> = rest.splitn(3, '/').collect();
            if let [iface, kind, stat] = parts[..] {
                qdiscs
                    .entry((iface.to_string(), kind.to_string()))
                    .or_default()
                    .insert(stat.to_string(), &point.value);
            }
        }
    }

    if qdiscs.is_empty() {
        return column![title, empty_state("No TC/qdisc data", None)]
            .spacing(space::SM)
            .into();
    }

    let mut list = Column::new().spacing(4).push(
        row![
            cell("interface", 120),
            cell("qdisc", 110),
            cell("drops", 90),
            cell("overlimits", 100),
            cell("requeues", 90),
            cell("backlog pkts", 110),
            cell("backlog bytes", 120),
        ]
        .spacing(8),
    );
    for ((iface, kind), stats) in &qdiscs {
        let g = |s: &str| num(stats.get(s).copied());
        list = list.push(
            row![
                cell(iface, 120),
                cell(kind, 110),
                cell(&g("drops"), 90),
                cell(&g("overlimits"), 100),
                cell(&g("requeues"), 90),
                cell(&g("backlog_pkts"), 110),
                cell(&g("backlog_bytes"), 120),
            ]
            .spacing(8),
        );
    }

    column![title, list].spacing(8).into()
}

/// ethtool per-interface link view: negotiated speed/duplex/autoneg plus the
/// link-health signals nlink 0.23 adds — FEC mode (silent corruption on
/// marginal optics) and EEE (power-saving that can add latency). Streamed
/// `ethtool/<iface>/<stat...>` metrics; only present when ethtool collection is
/// enabled and the driver exposes the family.
fn render_ethtool(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("ethtool (link / FEC / EEE)", None);

    // Group ethtool/<iface>/<stat...> by interface (stat may contain '/').
    let mut ifaces: BTreeMap<String, BTreeMap<String, &TelemetryValue>> = BTreeMap::new();
    for (metric, point) in &state.metrics {
        if let Some(rest) = metric.strip_prefix("ethtool/")
            && let Some((iface, stat)) = rest.split_once('/')
        {
            ifaces
                .entry(iface.to_string())
                .or_default()
                .insert(stat.to_string(), &point.value);
        }
    }

    if ifaces.is_empty() {
        return column![title, empty_state("No ethtool data", None)]
            .spacing(space::SM)
            .into();
    }

    let mut list = Column::new().spacing(4).push(
        row![
            cell("interface", 110),
            cell("link", 60),
            cell("speed", 80),
            cell("duplex", 80),
            cell("autoneg", 80),
            cell("FEC", 110),
            cell("EEE", 90),
        ]
        .spacing(8),
    );
    for (iface, stats) in &ifaces {
        let g = |s: &str| num(stats.get(s).copied());
        // EEE: prefer the live "active" signal, fall back to admin "enabled".
        let eee = match (stats.get("eee/active"), stats.get("eee/enabled")) {
            (Some(TelemetryValue::Boolean(true)), _) => "active".to_string(),
            (Some(TelemetryValue::Boolean(false)), Some(TelemetryValue::Boolean(true))) => {
                "enabled".to_string()
            }
            (Some(_), _) | (_, Some(_)) => "off".to_string(),
            _ => "-".to_string(),
        };
        let fec = stats
            .get("fec/modes")
            .map(|v| num(Some(v)))
            .unwrap_or_else(|| "-".into());
        list = list.push(
            row![
                cell(iface, 110),
                cell(&g("carrier"), 60),
                cell(&g("speed_mbps"), 80),
                cell(&g("duplex"), 80),
                cell(&g("autoneg"), 80),
                cell(&fec, 110),
                cell(&eee, 90),
            ]
            .spacing(8),
        );
    }

    column![title, list].spacing(8).into()
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
        // Default-route flap counter (#111) — the #1 connectivity incident.
        line("default route flaps (v4)", "routes/default_v4_flaps_total"),
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
        fetch(NetlinkDetailTopic::Addresses, d.addresses.is_loading()),
    ]
    .spacing(space::SM);
    // The remaining queryables (events ring, full TC tree, per-SA xfrm, nft
    // inventory) — previously served but unreachable from the UI (#109).
    let buttons2 = row![
        fetch(NetlinkDetailTopic::Events, d.events.is_loading()),
        fetch(
            NetlinkDetailTopic::RouteChanges,
            d.route_changes.is_loading()
        ),
        fetch(NetlinkDetailTopic::Tc, d.tc.is_loading()),
        fetch(NetlinkDetailTopic::Xfrm, d.xfrm.is_loading()),
        fetch(NetlinkDetailTopic::Nft, d.nft.is_loading()),
    ]
    .spacing(space::SM);

    let mut col = column![title, buttons, buttons2].spacing(space::SM);

    // Sockets
    if let Some(err) = d.sockets.error() {
        col = col.push(empty_state(format!("Sockets fetch failed: {err}"), None));
    } else if let Some(socks) = d.sockets.ready() {
        // Socket explorer (#112): filter by state/port and sort by RTT/retrans to
        // surface the worst flows, driving the already-fetched record set.
        let shown = filter_sort_sockets(
            socks,
            d.socket_state_filter.as_deref(),
            &d.socket_port_filter,
            d.socket_sort,
        );
        let mut list = Column::new().spacing(3).push(
            row![
                cell("local", 190),
                cell("remote", 190),
                cell("state", 100),
                cell("rtt_us", 70),
                cell("rcv_rtt", 70),
                cell("deliv_bps", 100),
                cell("pacing_bps", 100),
                cell("retx", 50),
                cell("bret", 70),
            ]
            .spacing(8),
        );
        for s in shown.iter().take(200) {
            // Surface the enriched tcp_info (#108): delivery/pacing rate and the
            // receiver RTT turn "state counts" into "are flows delivering".
            list = list.push(
                row![
                    cell(&s.local, 190),
                    cell(&s.remote, 190),
                    cell(&s.state, 100),
                    cell(&s.rtt_us.to_string(), 70),
                    cell(&s.rcv_rtt_us.to_string(), 70),
                    cell(&s.delivery_rate.to_string(), 100),
                    cell(&s.pacing_rate.to_string(), 100),
                    cell(&s.retrans.to_string(), 50),
                    cell(&s.bytes_retrans.to_string(), 70),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(
                text(format!("Sockets ({} of {})", shown.len(), socks.len())).size(font::EMPHASIS),
            )
            .push(render_socket_controls(socks, d))
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

    // Addresses (#109)
    if let Some(err) = d.addresses.error() {
        col = col.push(empty_state(format!("Addresses fetch failed: {err}"), None));
    } else if let Some(addrs) = d.addresses.ready() {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("address", 240),
                cell("scope", 90),
                cell("label", 120),
                cell("ifindex", 70),
            ]
            .spacing(8),
        );
        for a in addrs.iter().take(200) {
            let addr = match &a.ip {
                Some(ip) => format!("{ip}/{}", a.prefix_len),
                None => "-".to_string(),
            };
            list = list.push(
                row![
                    cell(&addr, 240),
                    cell(&a.scope, 90),
                    cell(a.label.as_deref().unwrap_or("-"), 120),
                    cell(&a.ifindex.to_string(), 70),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("Addresses ({})", addrs.len())).size(font::EMPHASIS))
            .push(list);
    }

    // Control-plane change timeline (#111): the recent-events ring (#109)
    // rendered most-recent-first with a relative timestamp — link up/down,
    // address add/del, route changes, neighbor failures on one time axis.
    if let Some(err) = d.events.error() {
        col = col.push(empty_state(format!("Events fetch failed: {err}"), None));
    } else if let Some(events) = d.events.ready() {
        let mut evs: Vec<&_> = events.iter().collect();
        evs.sort_by_key(|b| std::cmp::Reverse(b.ts_unix));
        let mut list = Column::new().spacing(3).push(
            row![
                cell("when", 90),
                cell("family", 80),
                cell("action", 80),
                cell("ifindex", 70),
                cell("detail", 240),
            ]
            .spacing(8),
        );
        for e in evs.iter().take(200) {
            let when = crate::view::formatting::format_timestamp(e.ts_unix as i64 * 1000);
            list = list.push(
                row![
                    cell(&when, 90),
                    cell(&e.family, 80),
                    cell(&e.action, 80),
                    cell(&e.ifindex.map(|i| i.to_string()).unwrap_or_default(), 70),
                    cell(&e.detail, 240),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("Control-plane timeline ({})", events.len())).size(font::EMPHASIS))
            .push(list);
    }

    // Default-route flap history (#111): per-transition gateway/withdrawal ring,
    // most-recent-first — the history behind the `default_v4_flaps_total` counter.
    if let Some(err) = d.route_changes.error() {
        col = col.push(empty_state(
            format!("Route flaps fetch failed: {err}"),
            None,
        ));
    } else if let Some(changes) = d.route_changes.ready() {
        if changes.is_empty() {
            col = col.push(
                text("Default-route flaps: none observed")
                    .size(font::CAPTION)
                    .style(dim),
            );
        } else {
            let mut chs: Vec<&_> = changes.iter().collect();
            chs.sort_by_key(|c| std::cmp::Reverse(c.ts_unix));
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("when", 90),
                    cell("family", 60),
                    cell("action", 90),
                    cell("gateway", 150),
                    cell("was", 150),
                ]
                .spacing(8),
            );
            for c in chs.iter().take(200) {
                let when = crate::view::formatting::format_timestamp(c.ts_unix as i64 * 1000);
                list = list.push(
                    row![
                        cell(&when, 90),
                        cell(&c.family, 60),
                        cell(&c.action, 90),
                        cell(c.gateway.as_deref().unwrap_or("-"), 150),
                        cell(c.prev_gateway.as_deref().unwrap_or("-"), 150),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("Default-route flaps ({})", changes.len())).size(font::EMPHASIS))
                .push(list);
        }
    }

    // TC qdisc/class tree (#109)
    if let Some(err) = d.tc.error() {
        col = col.push(empty_state(format!("TC fetch failed: {err}"), None));
    } else if let Some(tc) = d.tc.ready() {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("iface", 90),
                cell("node", 70),
                cell("kind", 110),
                cell("handle", 90),
                cell("drops", 90),
                cell("backlog_b", 100),
            ]
            .spacing(8),
        );
        for t in tc.iter().take(200) {
            list = list.push(
                row![
                    cell(&t.iface, 90),
                    cell(&t.node, 70),
                    cell(t.kind.as_deref().unwrap_or("-"), 110),
                    cell(&t.handle, 90),
                    cell(&t.drops.to_string(), 90),
                    cell(&t.backlog_bytes.to_string(), 100),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("TC ({})", tc.len())).size(font::EMPHASIS))
            .push(list);
    }

    // XFRM / IPsec SAs (#109)
    if let Some(err) = d.xfrm.error() {
        col = col.push(empty_state(format!("XFRM fetch failed: {err}"), None));
    } else if let Some(sas) = d.xfrm.ready() {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("src", 150),
                cell("dst", 150),
                cell("proto", 70),
                cell("mode", 90),
                cell("spi", 100),
            ]
            .spacing(8),
        );
        for s in sas.iter().take(200) {
            list = list.push(
                row![
                    cell(s.src.as_deref().unwrap_or("-"), 150),
                    cell(s.dst.as_deref().unwrap_or("-"), 150),
                    cell(&s.proto, 70),
                    cell(&s.mode, 90),
                    cell(&format!("{:#x}", s.spi), 100),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("XFRM SAs ({})", sas.len())).size(font::EMPHASIS))
            .push(list);
    }

    // nftables rule inventory (#109)
    if let Some(err) = d.nft.error() {
        col = col.push(empty_state(format!("NFT fetch failed: {err}"), None));
    } else if let Some(rules) = d.nft.ready() {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("family", 80),
                cell("table", 140),
                cell("chain", 120),
                cell("handle", 70),
                // #115: decoded firewall hit counters (packets / bytes matched).
                cell("packets", 90),
                cell("bytes", 90),
                cell("comment", 180),
            ]
            .spacing(8),
        );
        for r in rules.iter().take(200) {
            list = list.push(
                row![
                    cell(&r.family, 80),
                    cell(&r.table, 140),
                    cell(&r.chain, 120),
                    cell(&r.handle.to_string(), 70),
                    cell(&fmt_count(r.packets), 90),
                    cell(&fmt_bytes(r.bytes), 90),
                    cell(r.comment.as_deref().unwrap_or("-"), 180),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("NFT rules ({})", rules.len())).size(font::EMPHASIS))
            .push(list);
    }

    col.into()
}

/// Socket-explorer filter/sort controls (#112): state chips (derived from the
/// states present), a port substring input, and RTT/retrans sort toggles. Drives
/// the already-fetched record set client-side via [`filter_sort_sockets`].
fn render_socket_controls<'a>(
    socks: &[SocketRecord],
    d: &'a NetlinkDetailState,
) -> Element<'a, Message> {
    // Distinct states present, sorted for stable chip order.
    let states: std::collections::BTreeSet<&str> = socks.iter().map(|s| s.state.as_str()).collect();

    // State filter: "all" + one chip per observed state. Active chip uses the
    // primary style; pressing a chip sets the filter, "all" clears it.
    let chip = |label: &str, active: bool, msg: Message| {
        let mut b = button(text(label.to_string()).size(font::CAPTION)).padding([2, 8]);
        b = b.on_press(msg);
        if active {
            b = b.style(iced::widget::button::primary);
        } else {
            b = b.style(iced::widget::button::secondary);
        }
        b
    };
    let mut state_row = row![text("state:").size(font::CAPTION)].spacing(space::XS);
    state_row = state_row.push(chip(
        "all",
        d.socket_state_filter.is_none(),
        Message::SetNetlinkSocketStateFilter(None),
    ));
    for st in states {
        let active = d.socket_state_filter.as_deref() == Some(st);
        state_row = state_row.push(chip(
            st,
            active,
            Message::SetNetlinkSocketStateFilter(Some(st.to_string())),
        ));
    }

    let port_input = row![
        text("port:").size(font::CAPTION),
        text_input("any", &d.socket_port_filter)
            .on_input(Message::SetNetlinkSocketPortFilter)
            .size(font::CAPTION)
            .width(Length::Fixed(90.0)),
    ]
    .spacing(space::XS)
    .align_y(iced::Alignment::Center);

    let sort_btn = |label: &str, which: SocketSort| {
        chip(
            label,
            d.socket_sort == which,
            Message::SetNetlinkSocketSort(which),
        )
    };
    let sort_row = row![
        text("sort:").size(font::CAPTION),
        sort_btn("default", SocketSort::Default),
        sort_btn("RTT ↓", SocketSort::Rtt),
        sort_btn("retx ↓", SocketSort::Retrans),
    ]
    .spacing(space::XS)
    .align_y(iced::Alignment::Center);

    column![state_row, row![port_input, sort_row].spacing(space::MD)]
        .spacing(space::XS)
        .into()
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

/// Compact count: `1234` → `1.2K`, `2_000_000` → `2.0M` (#115 nft hit counters).
fn fmt_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}G", n as f64 / 1e9),
    }
}

/// Human byte size (#115 nft byte counters).
fn fmt_bytes(n: u64) -> String {
    let b = n as f64;
    if b < 1024.0 {
        format!("{n} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0))
    }
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
