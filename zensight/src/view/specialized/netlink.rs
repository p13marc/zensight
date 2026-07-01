//! Netlink host specialized view — a tabbed, chart-driven, drill-down surface
//! (Overview · Interfaces · Sockets · Routing & Neighbors · QoS · Firewall &
//! IPsec · Events · WireGuard) over the sensor's streamed metrics and
//! `@/query/*` channels (#258, epic #270).

use std::collections::BTreeMap;

use iced::widget::{Column, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};
use zensight_common::{SocketRecord, TelemetryValue};

use crate::message::Message;
use crate::view::components::{
    Gauge, TabItem, badge, card, empty_state, section_header, tabbed_view,
};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::SpecializedTab;
use crate::view::specialized::netlink_detail::{
    NetlinkDetailState, NetlinkDetailTopic, SocketSort, filter_sort_sockets,
};
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the netlink host specialized view: a header + the tabbed container
/// over the active tab's content (#258).
pub fn netlink_host_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let tabs = netlink_tabs(state);
    let active = if tabs
        .iter()
        .any(|t| t.visible && t.id == state.specialized_tab)
    {
        state.specialized_tab
    } else {
        SpecializedTab::Overview
    };
    let device_id = state.device_id.clone();
    let content = netlink_tab_content(state, active);
    column![
        render_header(state),
        tabbed_view(&tabs, active, content, move |t| {
            Message::SelectSpecializedTab(device_id.clone(), t)
        }),
    ]
    .spacing(space::SM)
    .padding(space::LG)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The netlink tab strip, capability-aware: environment-specific tabs (QoS,
/// Firewall/IPsec, WireGuard) show only when the host publishes their data.
fn netlink_tabs(state: &DeviceDetailState) -> Vec<TabItem<SpecializedTab>> {
    use SpecializedTab::*;
    let has_fw = has_prefix(state, "conntrack/")
        || has_prefix(state, "xfrm/")
        || has_prefix(state, "nft/")
        || has_prefix(state, "events/ipsec/");
    vec![
        TabItem::new(Overview, "Overview"),
        TabItem::new(Interfaces, "Interfaces"),
        TabItem::new(Sockets, "Sockets"),
        TabItem::new(RoutingNeighbors, "Routing & Neighbors"),
        TabItem::new(Qos, "QoS / Queues").visible(has_prefix(state, "tc/")),
        TabItem::new(FirewallIpsec, "Firewall & IPsec").visible(has_fw),
        TabItem::new(Events, "Events"),
        TabItem::new(WireGuard, "WireGuard").visible(has_prefix(state, "wireguard/")),
    ]
}

/// Build the scrollable content for a netlink tab by composing the existing
/// per-section cards + on-demand detail tables. No data regression: every
/// section reachable in the old single-scroll view lives in exactly one tab.
fn netlink_tab_content(state: &DeviceDetailState, tab: SpecializedTab) -> Element<'_, Message> {
    use SpecializedTab::*;
    let inner: Column<'_, Message> = match tab {
        Overview => render_overview(state),
        Interfaces => {
            let mut c = column![card(render_interfaces(state))].spacing(space::MD);
            if has_prefix(state, "ethtool/") {
                c = c.push(card(render_ethtool(state)));
            }
            c
        }
        Sockets => column![
            card(render_sockets(state)),
            card(render_sockets_explorer(state)),
        ]
        .spacing(space::MD),
        RoutingNeighbors => column![
            card(render_routes(state)),
            card(render_neighbors(state)),
            card(render_detail(
                state,
                &[
                    NetlinkDetailTopic::Routes,
                    NetlinkDetailTopic::Neighbors,
                    NetlinkDetailTopic::Addresses,
                    NetlinkDetailTopic::RouteChanges,
                ],
            )),
        ]
        .spacing(space::MD),
        Qos => column![
            card(render_tc(state)),
            card(render_detail(state, &[NetlinkDetailTopic::Tc])),
        ]
        .spacing(space::MD),
        FirewallIpsec => {
            let mut c = column![].spacing(space::MD);
            if has_prefix(state, "conntrack/") {
                c = c.push(card(render_conntrack(state)));
            }
            if has_prefix(state, "xfrm/") || has_prefix(state, "events/ipsec/") {
                c = c.push(card(render_xfrm(state)));
            }
            c = c.push(card(render_detail(
                state,
                &[NetlinkDetailTopic::Xfrm, NetlinkDetailTopic::Nft],
            )));
            c
        }
        Events => {
            column![card(render_detail(state, &[NetlinkDetailTopic::Events]))].spacing(space::MD)
        }
        WireGuard => column![card(render_wireguard(state))].spacing(space::MD),
        // netring tabs never reach a netlink view (falls back to Overview).
        _ => column![card(render_diagnostics(state))].spacing(space::MD),
    };
    scrollable(inner.width(Length::Fill))
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

/// First-class TCP socket explorer (#261): a refresh affordance, the state/port/
/// sort controls, RTT-distribution + congestion-mix charts derived from the
/// fetched set, and a paginated table with an explicit "N of M" footer (no more
/// silent `.take(200)` cutoff). Surfaces the enriched tcp_info columns
/// (delivery/pacing rate, bytes_retrans, rcv_rtt, lost, reord, cong).
fn render_sockets_explorer(state: &DeviceDetailState) -> Element<'_, Message> {
    let d = &state.netlink_detail;
    let title = section_header("Socket Explorer", None);
    let loading = d.sockets.is_loading();
    let refresh_label = if loading {
        "Fetching Sockets…".to_string()
    } else {
        "Fetch Sockets".to_string()
    };
    let mut refresh = button(text(refresh_label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        refresh = refresh.on_press(Message::FetchNetlinkDetail(NetlinkDetailTopic::Sockets));
    }
    let header = row![title, refresh]
        .spacing(space::MD)
        .align_y(iced::Alignment::Center);
    let mut col = column![header].spacing(space::SM);

    if let Some(err) = d.sockets.error() {
        return col
            .push(empty_state(format!("Sockets fetch failed: {err}"), None))
            .into();
    }
    let Some(socks) = d.sockets.ready() else {
        return col.push(empty_state("Loading sockets…", None)).into();
    };
    if socks.is_empty() {
        return col.push(empty_state("No sockets", None)).into();
    }

    // Distribution charts from the fetched set: RTT histogram + congestion mix.
    let charts = row![
        container(card(
            column![
                text("RTT distribution").size(font::CAPTION).style(dim),
                crate::view::chart::ranked_bar(
                    &rtt_histogram(socks),
                    |v| format!("{}", v as u64),
                    8
                ),
            ]
            .spacing(space::XS)
        ))
        .width(Length::FillPortion(1)),
        container(card(
            column![
                text("Congestion control").size(font::CAPTION).style(dim),
                crate::view::chart::donut(&cong_mix(socks), 120.0),
            ]
            .spacing(space::XS)
        ))
        .width(Length::FillPortion(1)),
    ]
    .spacing(space::MD);
    col = col.push(charts);

    // State/port/sort controls, then the paginated record table.
    col = col.push(render_socket_controls(socks, d));
    let shown = filter_sort_sockets(
        socks,
        d.socket_state_filter.as_deref(),
        &d.socket_port_filter,
        d.socket_sort,
    );
    let total = shown.len();
    let limit = d.sockets_table.limit;
    let mut list = Column::new().spacing(3).push(
        row![
            cell("local", 180),
            cell("remote", 180),
            cell("state", 90),
            cell("rtt_us", 70),
            cell("rcv_rtt", 70),
            cell("deliv_bps", 90),
            cell("pacing_bps", 90),
            cell("retx", 50),
            cell("bret", 70),
            cell("lost", 50),
            cell("reord", 55),
            cell("cong", 70),
        ]
        .spacing(8),
    );
    for s in shown.iter().take(limit) {
        list = list.push(
            row![
                cell(&s.local, 180),
                cell(&s.remote, 180),
                cell(&s.state, 90),
                cell(&s.rtt_us.to_string(), 70),
                cell(&s.rcv_rtt_us.to_string(), 70),
                cell(&s.delivery_rate.to_string(), 90),
                cell(&s.pacing_rate.to_string(), 90),
                cell(&s.retrans.to_string(), 50),
                cell(&s.bytes_retrans.to_string(), 70),
                cell(&s.lost.to_string(), 50),
                cell(&s.reord_seen.to_string(), 55),
                cell(s.congestion.as_deref().unwrap_or("-"), 70),
            ]
            .spacing(8),
        );
    }
    let shown_n = total.min(limit);
    let mut footer = row![
        text(format!("showing {shown_n} of {total} sockets"))
            .size(font::CAPTION)
            .style(dim)
    ]
    .spacing(space::MD)
    .align_y(iced::Alignment::Center);
    if shown_n < total {
        footer = footer.push(
            button(text("Show more").size(font::CAPTION))
                .padding([2, 8])
                .on_press(Message::NetlinkSocketsMore),
        );
    }
    col.push(list).push(footer).into()
}

/// Bucket sockets by smoothed RTT (µs) into human ranges for the histogram (#261).
fn rtt_histogram(socks: &[SocketRecord]) -> Vec<(String, f64)> {
    let buckets: [(&str, u32, u32); 6] = [
        ("<1ms", 0, 1_000),
        ("1–5ms", 1_000, 5_000),
        ("5–20ms", 5_000, 20_000),
        ("20–100ms", 20_000, 100_000),
        ("100–300ms", 100_000, 300_000),
        ("300ms+", 300_000, u32::MAX),
    ];
    buckets
        .iter()
        .map(|(label, lo, hi)| {
            let n = socks
                .iter()
                .filter(|s| s.rtt_us >= *lo && s.rtt_us < *hi)
                .count();
            (label.to_string(), n as f64)
        })
        .collect()
}

/// Count sockets per congestion-control algorithm for the donut (#261).
fn cong_mix(socks: &[SocketRecord]) -> Vec<(String, f64)> {
    let mut map: BTreeMap<String, f64> = BTreeMap::new();
    for s in socks {
        let algo = s.congestion.clone().unwrap_or_else(|| "unknown".into());
        *map.entry(algo).or_default() += 1.0;
    }
    map.into_iter().collect()
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

/// Overview health hero (#259): a bottleneck gauge + issue badges, an interface
/// status strip, TCP-health tiles (sparklined; retransmits as a rate not a raw
/// counter), and default-route + neighbor health chips.
fn render_overview(state: &DeviceDetailState) -> Column<'_, Message> {
    let mut col = column![card(render_health_hero(state))].spacing(space::MD);

    // Interface status strip.
    let ifaces = interfaces(state);
    if !ifaces.is_empty() {
        let mut strip = row![].spacing(space::SM);
        for (name, stats) in &ifaces {
            strip = strip.push(iface_chip(name, iface_up(stats)));
        }
        col = col.push(card(
            column![
                section_header("Interfaces", None),
                scrollable(strip).direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::hidden(),
                )),
            ]
            .spacing(space::SM),
        ));
    }

    // TCP-health tiles.
    if state.metrics.keys().any(|k| k.starts_with("sockets/tcp/")) {
        let tiles = row![
            metric_tile(state, "established", "sockets/tcp/established", None),
            metric_tile(
                state,
                "retransmits/s",
                "sockets/tcp/retransmits_total",
                Some(rate_str(state, "sockets/tcp/retransmits_total")),
            ),
            metric_tile(state, "RTT p50 (µs)", "sockets/tcp/rtt_p50_us", None),
            metric_tile(state, "RTT p95 (µs)", "sockets/tcp/rtt_p95_us", None),
        ]
        .spacing(space::MD);
        col = col.push(card(
            column![section_header("TCP health", None), tiles].spacing(space::SM),
        ));
    }

    // Default-route + neighbor health chips.
    if let Some(chips) = render_route_neighbor_chips(state) {
        col = col.push(card(chips));
    }

    col
}

/// Health hero: bottleneck gauge + severity issue badges + worst-bottleneck
/// location/recommendation, with the promote-to-alert affordance retained.
fn render_health_hero(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Health", None);
    let score = fval(state, "diagnostics/bottleneck_score").unwrap_or(0.0);
    let gauge = Gauge::percentage(score * 100.0, "bottleneck")
        .with_thresholds(0.5, 0.8)
        .with_width(200.0)
        .view();

    let issue = |label: &str, metric: &str, color| {
        let n = fval(state, metric).unwrap_or(0.0) as u64;
        badge(color, format!("{label}: {n}"))
    };
    let issues = row![
        issue(
            "critical",
            "diagnostics/issues/critical",
            theme::SEVERITY_CRITICAL
        ),
        issue(
            "error",
            "diagnostics/issues/error",
            theme::SEVERITY_CRITICAL
        ),
        issue(
            "warning",
            "diagnostics/issues/warning",
            theme::SEVERITY_WARNING
        ),
        issue("info", "diagnostics/issues/info", theme::SEVERITY_INFO),
    ]
    .spacing(space::SM);

    let hero = row![
        gauge,
        column![
            issues,
            super::metric_trend_and_alert(state, "diagnostics/bottleneck_score")
        ]
        .spacing(space::XS),
    ]
    .spacing(space::LG)
    .align_y(iced::Alignment::Center);
    let mut col = column![title, hero].spacing(space::SM);

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
        col = col.push(text(format!("{kind} @ {loc}")).size(font::EMPHASIS));
        if !rec.is_empty() {
            col = col.push(text(rec).size(font::CAPTION).style(dim));
        }
    }
    col.into()
}

/// A compact metric tile: big value + label + trend sparkline (#259).
fn metric_tile<'a>(
    state: &'a DeviceDetailState,
    label: &str,
    metric: &str,
    value_override: Option<String>,
) -> Element<'a, Message> {
    let value = value_override.unwrap_or_else(|| num(state.metrics.get(metric).map(|p| &p.value)));
    container(card(
        column![
            text(value).size(font::SECTION),
            text(label.to_string()).size(font::CAPTION).style(dim),
            super::metric_sparkline(state, metric),
        ]
        .spacing(space::XS),
    ))
    .width(Length::FillPortion(1))
    .into()
}

/// Format a counter's per-second rate from the last two history points (#259),
/// falling back to the raw value when there isn't enough history yet.
fn rate_str(state: &DeviceDetailState, metric: &str) -> String {
    match counter_rate(state, metric) {
        Some(r) => format!("{r:.1}"),
        None => num(state.metrics.get(metric).map(|p| &p.value)),
    }
}

/// Per-second rate of a monotonic counter from its two most-recent history
/// points; `None` on insufficient history, zero dt, or a counter reset.
fn counter_rate(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    let hist = state.history.get(metric)?;
    if hist.len() < 2 {
        return None;
    }
    let last = hist.back()?;
    let prev = &hist[hist.len() - 2];
    let dt = (last.timestamp - prev.timestamp) as f64 / 1000.0;
    if dt <= 0.0 {
        return None;
    }
    let v = |p: &zensight_common::TelemetryPoint| match &p.value {
        TelemetryValue::Counter(c) => Some(*c as f64),
        TelemetryValue::Gauge(g) => Some(*g),
        _ => None,
    };
    let (a, b) = (v(last)?, v(prev)?);
    if a < b {
        return None; // counter reset
    }
    Some((a - b) / dt)
}

/// The raw numeric value of a metric (counter/gauge/bool→0|1), if present.
fn fval(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    match state.metrics.get(metric).map(|p| &p.value) {
        Some(TelemetryValue::Counter(c)) => Some(*c as f64),
        Some(TelemetryValue::Gauge(g)) => Some(*g),
        Some(TelemetryValue::Boolean(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Interface up/down (from `oper_state` text, else the `up` boolean).
fn iface_up(stats: &BTreeMap<String, &TelemetryValue>) -> Option<bool> {
    if let Some(s) = stats.get("oper_state").and_then(text_val) {
        return Some(s.eq_ignore_ascii_case("up"));
    }
    stats.get("up").and_then(bool_val)
}

/// A colored interface chip for the Overview status strip.
fn iface_chip<'a>(name: &str, up: Option<bool>) -> Element<'a, Message> {
    let color = match up {
        Some(true) => theme::STATUS_ONLINE,
        Some(false) => theme::STATUS_OFFLINE,
        None => theme::STATUS_UNKNOWN,
    };
    badge(color, name.to_string())
}

/// Default-route + neighbor health chips (#259).
fn render_route_neighbor_chips(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    let has_routes = state.metrics.keys().any(|k| k.starts_with("routes/"));
    let has_neigh = state.metrics.keys().any(|k| k.starts_with("neighbors/"));
    if !has_routes && !has_neigh {
        return None;
    }
    let mut r = row![].spacing(space::SM);
    if has_routes {
        let present = matches!(
            state
                .metrics
                .get("routes/default_v4_present")
                .map(|p| &p.value),
            Some(TelemetryValue::Boolean(true))
        );
        let gw = state
            .metrics
            .get("routes/default_v4_gw")
            .and_then(|p| text_val(&&p.value));
        let color = if present {
            theme::STATUS_ONLINE
        } else {
            theme::STATUS_OFFLINE
        };
        let label = match gw {
            Some(g) if present => format!("default → {g}"),
            _ if present => "default route".to_string(),
            _ => "no default route".to_string(),
        };
        r = r.push(badge(color, label));
        if let Some(f) = fval(state, "routes/default_v4_flaps_total")
            && f > 0.0
        {
            r = r.push(badge(theme::STATUS_DEGRADED, format!("{} flaps", f as u64)));
        }
    }
    if has_neigh {
        let total = fval(state, "neighbors/total").unwrap_or(0.0) as u64;
        let failed = fval(state, "neighbors/by_state/failed").unwrap_or(0.0) as u64;
        let color = if failed > 0 {
            theme::STATUS_DEGRADED
        } else {
            theme::STATUS_ONLINE
        };
        r = r.push(badge(
            color,
            format!("neighbors: {total} ({failed} failed)"),
        ));
    }
    Some(
        column![section_header("Routing & neighbors", None), r]
            .spacing(space::SM)
            .into(),
    )
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

/// On-demand detail tables for the requested `topics` (#258): fetch buttons plus
/// the fetched full tables (pulled from the sensor's `@/query/*` channels), so
/// each tab shows only its own drill-downs. `loading(topic)` relabels/disables
/// the button while a fetch is in flight.
fn render_detail<'a>(
    state: &'a DeviceDetailState,
    topics: &[NetlinkDetailTopic],
) -> Element<'a, Message> {
    let title = section_header("On-demand Detail", None);
    let d = &state.netlink_detail;
    let want = |t: NetlinkDetailTopic| topics.contains(&t);

    // A fetch button per requested topic; disabled/relabelled while loading.
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
    let loading_of = |t: NetlinkDetailTopic| match t {
        NetlinkDetailTopic::Sockets => d.sockets.is_loading(),
        NetlinkDetailTopic::Routes => d.routes.is_loading(),
        NetlinkDetailTopic::Neighbors => d.neighbors.is_loading(),
        NetlinkDetailTopic::Addresses => d.addresses.is_loading(),
        NetlinkDetailTopic::Events => d.events.is_loading(),
        NetlinkDetailTopic::RouteChanges => d.route_changes.is_loading(),
        NetlinkDetailTopic::Tc => d.tc.is_loading(),
        NetlinkDetailTopic::Xfrm => d.xfrm.is_loading(),
        NetlinkDetailTopic::Nft => d.nft.is_loading(),
    };
    let mut buttons = row![].spacing(space::SM);
    for t in topics {
        buttons = buttons.push(fetch(*t, loading_of(*t)));
    }

    let mut col = column![title, buttons].spacing(space::SM);

    // Sockets are rendered by the first-class explorer (`render_sockets_explorer`,
    // #261), not here — the Sockets topic isn't routed through `render_detail`.

    // Routes
    if want(NetlinkDetailTopic::Routes)
        && let Some(err) = d.routes.error()
    {
        col = col.push(empty_state(format!("Routes fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Routes)
        && let Some(routes) = d.routes.ready()
    {
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
    if want(NetlinkDetailTopic::Neighbors)
        && let Some(err) = d.neighbors.error()
    {
        col = col.push(empty_state(format!("Neighbors fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Neighbors)
        && let Some(neighbors) = d.neighbors.ready()
    {
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
    if want(NetlinkDetailTopic::Addresses)
        && let Some(err) = d.addresses.error()
    {
        col = col.push(empty_state(format!("Addresses fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Addresses)
        && let Some(addrs) = d.addresses.ready()
    {
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
    if want(NetlinkDetailTopic::Events)
        && let Some(err) = d.events.error()
    {
        col = col.push(empty_state(format!("Events fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Events)
        && let Some(events) = d.events.ready()
    {
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
    if want(NetlinkDetailTopic::RouteChanges)
        && let Some(err) = d.route_changes.error()
    {
        col = col.push(empty_state(
            format!("Route flaps fetch failed: {err}"),
            None,
        ));
    } else if want(NetlinkDetailTopic::RouteChanges)
        && let Some(changes) = d.route_changes.ready()
    {
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
    if want(NetlinkDetailTopic::Tc)
        && let Some(err) = d.tc.error()
    {
        col = col.push(empty_state(format!("TC fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Tc)
        && let Some(tc) = d.tc.ready()
    {
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
    if want(NetlinkDetailTopic::Xfrm)
        && let Some(err) = d.xfrm.error()
    {
        col = col.push(empty_state(format!("XFRM fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Xfrm)
        && let Some(sas) = d.xfrm.ready()
    {
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
    if want(NetlinkDetailTopic::Nft)
        && let Some(err) = d.nft.error()
    {
        col = col.push(empty_state(format!("NFT fetch failed: {err}"), None));
    } else if want(NetlinkDetailTopic::Nft)
        && let Some(rules) = d.nft.ready()
    {
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
