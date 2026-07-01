//! Netlink host specialized view — a tabbed, chart-driven, drill-down surface
//! (Overview · Interfaces · Sockets · Routing & Neighbors · QoS · Firewall &
//! IPsec · Events · WireGuard) over the sensor's streamed metrics and
//! `@/query/*` channels (#258, epic #270).

use std::collections::BTreeMap;

use iced::widget::{Column, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};
use zensight_common::{NeighborRecord, RouteRecord, SocketRecord, TelemetryValue};

use crate::message::Message;
use crate::view::components::{
    Column as DataColumn, DataTable, Gauge, SortKey, TabItem, badge, card, empty_state,
    section_header, tabbed_view,
};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::SpecializedTab;
use crate::view::specialized::fetch::Fetch;
use crate::view::specialized::netlink_detail::{
    AddressRecord, EventRecord, NetlinkDetailState, NetlinkDetailTopic, NetlinkTable,
    NftRuleRecord, RouteChangeRecord, SocketSort, TcRecord, XfrmSaRecord, filter_sort_sockets,
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
        Interfaces => render_interfaces_tab(state),
        Sockets => column![
            card(render_sockets(state)),
            card(render_sockets_explorer(state)),
        ]
        .spacing(space::MD),
        RoutingNeighbors => render_routing_tab(state),
        Qos => render_qos_tab(state),
        FirewallIpsec => render_firewall_tab(state),
        Events => render_events_tab(state),
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

/// Interfaces tab (#260): one card per interface — rx/tx throughput trend charts,
/// drops/errors, MTU/state, ethtool link health (speed/duplex/autoneg/FEC/EEE),
/// and a drill-down to the interface's sockets.
fn render_interfaces_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let ifaces = interfaces(state);
    if ifaces.is_empty() {
        return column![card(
            column![
                section_header("Interfaces", None),
                empty_state("No interface data", None),
            ]
            .spacing(space::SM)
        )]
        .spacing(space::MD);
    }
    let eth = ethtool_by_iface(state);
    let mut col = column![].spacing(space::MD);
    for (name, stats) in &ifaces {
        col = col.push(card(render_iface_card(state, name, stats, eth.get(name))));
    }
    col
}

/// A single interface card: header (name/state/MTU + sockets pivot), rx/tx
/// throughput trend tiles, drops/errors, and inline ethtool link health.
fn render_iface_card<'a>(
    state: &'a DeviceDetailState,
    name: &str,
    stats: &BTreeMap<String, &TelemetryValue>,
    eth: Option<&BTreeMap<String, &'a TelemetryValue>>,
) -> Element<'a, Message> {
    let header = row![
        text(name.to_string()).size(font::EMPHASIS),
        iface_chip(name, iface_up(stats)),
        text(format!("MTU {}", num(stats.get("mtu").copied())))
            .size(font::CAPTION)
            .style(dim),
        container(text("")).width(Length::Fill),
        button(text("View sockets →").size(font::CAPTION))
            .padding([2, 8])
            .on_press(Message::SelectSpecializedTab(
                state.device_id.clone(),
                SpecializedTab::Sockets,
            )),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let throughput = row![
        tput_tile(state, "rx ↓", &format!("iface/{name}/rx_bytes")),
        tput_tile(state, "tx ↑", &format!("iface/{name}/tx_bytes")),
    ]
    .spacing(space::LG);

    let counters = row![
        text(format!(
            "drops rx {} / tx {}",
            num(stats.get("rx_dropped").copied()),
            num(stats.get("tx_dropped").copied()),
        ))
        .size(font::CAPTION)
        .style(dim),
        text(format!(
            "errors rx {} / tx {}",
            num(stats.get("rx_errors").copied()),
            num(stats.get("tx_errors").copied()),
        ))
        .size(font::CAPTION)
        .style(dim),
    ]
    .spacing(space::LG);

    let mut col = column![header, throughput, counters].spacing(space::SM);

    // ethtool link health, inline (#260).
    if let Some(e) = eth {
        let g = |s: &str| {
            e.get(s)
                .copied()
                .map(|v| num(Some(v)))
                .unwrap_or_else(|| "-".into())
        };
        let eee = match (e.get("eee/active").copied(), e.get("eee/enabled").copied()) {
            (Some(TelemetryValue::Boolean(true)), _) => "active".to_string(),
            (Some(TelemetryValue::Boolean(false)), Some(TelemetryValue::Boolean(true))) => {
                "enabled".to_string()
            }
            (Some(_), _) | (_, Some(_)) => "off".to_string(),
            _ => "-".to_string(),
        };
        let fec = e
            .get("fec/modes")
            .copied()
            .map(|v| num(Some(v)))
            .unwrap_or_else(|| "-".into());
        let ethline = row![
            text(format!("link {}", g("carrier"))).size(font::CAPTION),
            text(format!("{} Mb/s", g("speed_mbps"))).size(font::CAPTION),
            text(format!("{} duplex", g("duplex"))).size(font::CAPTION),
            text(format!("autoneg {}", g("autoneg"))).size(font::CAPTION),
            text(format!("FEC {fec}")).size(font::CAPTION),
            text(format!("EEE {eee}")).size(font::CAPTION),
        ]
        .spacing(space::MD);
        col = col.push(ethline);
    }

    col.into()
}

/// A throughput trend tile: per-second rate (from history) + counter sparkline.
fn tput_tile<'a>(state: &'a DeviceDetailState, label: &str, key: &str) -> Element<'a, Message> {
    let rate = counter_rate(state, key)
        .map(|r| format!("{}/s", fmt_bytes(r.max(0.0) as u64)))
        .unwrap_or_else(|| num(state.metrics.get(key).map(|p| &p.value)));
    column![
        row![
            text(label.to_string()).size(font::CAPTION).style(dim),
            text(rate).size(font::EMPHASIS),
        ]
        .spacing(space::XS)
        .align_y(iced::Alignment::Center),
        super::metric_sparkline(state, key),
    ]
    .spacing(space::XS)
    .into()
}

/// Group `ethtool/<iface>/<stat...>` metrics by interface (stat may contain '/').
fn ethtool_by_iface(
    state: &DeviceDetailState,
) -> BTreeMap<String, BTreeMap<String, &TelemetryValue>> {
    let mut m: BTreeMap<String, BTreeMap<String, &TelemetryValue>> = BTreeMap::new();
    for (metric, point) in &state.metrics {
        if let Some(rest) = metric.strip_prefix("ethtool/")
            && let Some((iface, stat)) = rest.split_once('/')
        {
            m.entry(iface.to_string())
                .or_default()
                .insert(stat.to_string(), &point.value);
        }
    }
    m
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

/// QoS / Queues tab (#263): per-(iface, qdisc) health chips + AQM class, backlog
/// trend sparklines, drops/overlimits/requeues, and the full qdisc/class tree
/// (`@/query/tc`) as a DataTable. From streamed `tc/<iface>/<kind>/<stat>` +
/// iface-level `tc/<iface>/aqm_class`.
fn render_qos_tab(state: &DeviceDetailState) -> Column<'_, Message> {
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

    let mut col = column![].spacing(space::MD);
    if qdiscs.is_empty() {
        col = col.push(card(
            column![
                section_header("TC / QoS qdiscs", None),
                empty_state("No TC/qdisc data", None),
            ]
            .spacing(space::SM),
        ));
    }
    for ((iface, kind), stats) in &qdiscs {
        col = col.push(card(render_qdisc_card(state, iface, kind, stats)));
    }

    // Full qdisc/class tree (@/query/tc).
    let d = &state.netlink_detail;
    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Tc,
        NetlinkDetailTopic::Tc,
        &d.tc,
        "Qdisc / class tree",
        "nodes",
        tc_columns(),
        |t: &TcRecord| format!("{} {}", t.iface, t.kind.as_deref().unwrap_or("")),
    )));

    col
}

/// One qdisc card: iface/kind header + health chip + AQM class + backlog trend
/// sparklines + drops/overlimits/requeues.
fn render_qdisc_card<'a>(
    state: &'a DeviceDetailState,
    iface: &str,
    kind: &str,
    stats: &BTreeMap<String, &TelemetryValue>,
) -> Element<'a, Message> {
    let g = |s: &str| num(stats.get(s).copied());
    let health = stats.get("health_score").and_then(|v| tv_num(v));
    let health_chip = match health {
        Some(h) => {
            let color = if h >= 0.7 {
                theme::STATUS_ONLINE
            } else if h >= 0.4 {
                theme::STATUS_DEGRADED
            } else {
                theme::STATUS_OFFLINE
            };
            badge(color, format!("health {h:.2}"))
        }
        None => badge(theme::STATUS_UNKNOWN, "health -".to_string()),
    };
    let aqm = num(state
        .metrics
        .get(&format!("tc/{iface}/aqm_class"))
        .map(|p| &p.value));

    let header = row![
        text(format!("{iface} / {kind}")).size(font::EMPHASIS),
        health_chip,
        badge(theme::SEVERITY_INFO, format!("AQM {aqm}")),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let backlog = row![
        tput_tile(
            state,
            "backlog bytes",
            &format!("tc/{iface}/{kind}/backlog_bytes")
        ),
        tput_tile(
            state,
            "backlog pkts",
            &format!("tc/{iface}/{kind}/backlog_pkts")
        ),
    ]
    .spacing(space::LG);

    let counters = text(format!(
        "drops {} · overlimits {} · requeues {}",
        g("drops"),
        g("overlimits"),
        g("requeues"),
    ))
    .size(font::CAPTION)
    .style(dim);

    column![header, backlog, counters].spacing(space::SM).into()
}

fn tc_columns<'a>() -> Vec<DataColumn<'a, TcRecord, Message>> {
    vec![
        DataColumn::fill("iface", 2, |t: &TcRecord| {
            text(t.iface.clone()).size(font::CAPTION).into()
        })
        .sortable(|t: &TcRecord| SortKey::Text(t.iface.clone())),
        DataColumn::fixed("node", 70.0, |t: &TcRecord| {
            text(t.node.clone()).size(font::CAPTION).into()
        }),
        DataColumn::fill("kind", 2, |t: &TcRecord| {
            text(t.kind.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|t: &TcRecord| SortKey::Text(t.kind.clone().unwrap_or_default())),
        DataColumn::fixed("handle", 90.0, |t: &TcRecord| {
            text(t.handle.clone()).size(font::CAPTION).into()
        }),
        DataColumn::fixed("drops", 90.0, |t: &TcRecord| {
            text(fmt_count(t.drops)).size(font::CAPTION).into()
        })
        .sortable(|t: &TcRecord| SortKey::Num(t.drops as f64)),
        DataColumn::fixed("backlog_b", 100.0, |t: &TcRecord| {
            text(fmt_bytes(t.backlog_bytes)).size(font::CAPTION).into()
        })
        .sortable(|t: &TcRecord| SortKey::Num(t.backlog_bytes as f64)),
    ]
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

/// Routing & Neighbors tab (#262): route + neighbor summaries, a default-route
/// flap section (count tile + flap timeline table), a neighbor-state breakdown
/// donut, and DataTable views of routes / neighbors / addresses.
fn render_routing_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.netlink_detail;
    let mut col = column![card(render_routes(state))].spacing(space::MD);

    // Default-route flap section: count tile + flap timeline table.
    col = col.push(card(render_flap_section(state)));

    // Neighbor summary + state-breakdown donut.
    col = col.push(card(render_neighbors(state)));
    if state
        .metrics
        .keys()
        .any(|k| k.starts_with("neighbors/by_state/"))
    {
        col = col.push(card(
            column![
                section_header("Neighbor states", None),
                crate::view::chart::donut(&neighbor_state_mix(state), 120.0),
            ]
            .spacing(space::XS),
        ));
    }

    // Record tables (DataTable, #244 — sortable/filterable, no silent cutoff).
    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Routes,
        NetlinkDetailTopic::Routes,
        &d.routes,
        "Routes",
        "routes",
        routes_columns(),
        |r: &RouteRecord| format!("{} {}", r.dst, r.gateway.as_deref().unwrap_or("")),
    )));
    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Neighbors,
        NetlinkDetailTopic::Neighbors,
        &d.neighbors,
        "Neighbors",
        "neighbors",
        neighbors_columns(),
        |n: &NeighborRecord| {
            format!(
                "{} {} {}",
                n.ip.as_deref().unwrap_or(""),
                n.mac.as_deref().unwrap_or(""),
                n.state
            )
        },
    )));
    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Addresses,
        NetlinkDetailTopic::Addresses,
        &d.addresses,
        "Addresses",
        "addresses",
        addresses_columns(),
        |a: &AddressRecord| a.ip.clone().unwrap_or_default(),
    )));

    col
}

/// Default-route flap count tile + the flap timeline record table (#262).
fn render_flap_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let d = &state.netlink_detail;
    let tile = row![metric_tile(
        state,
        "flaps (total)",
        "routes/default_v4_flaps_total",
        None,
    )]
    .spacing(space::MD);
    let table = detail_datatable(
        d,
        NetlinkTable::RouteChanges,
        NetlinkDetailTopic::RouteChanges,
        &d.route_changes,
        "Flap timeline",
        "flaps",
        route_changes_columns(),
        |c: &RouteChangeRecord| format!("{} {}", c.family, c.action),
    );
    column![section_header("Default-route flaps", None), tile, table]
        .spacing(space::SM)
        .into()
}

/// Per-neighbor-state counts (`neighbors/by_state/*`) for the breakdown donut.
fn neighbor_state_mix(state: &DeviceDetailState) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            let st = m.strip_prefix("neighbors/by_state/")?;
            Some((st.to_string(), tv_num(&p.value)?))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// A `TelemetryValue`'s numeric projection (counter/gauge/bool→0|1).
fn tv_num(v: &TelemetryValue) -> Option<f64> {
    match v {
        TelemetryValue::Counter(c) => Some(*c as f64),
        TelemetryValue::Gauge(g) => Some(*g),
        TelemetryValue::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Generic detail-table renderer (#244): a fetch/refresh affordance + the
/// fetched records as a sortable/filterable [`DataTable`] addressed by `which`.
/// Reused across the Routing, QoS, and Firewall tabs.
#[allow(clippy::too_many_arguments)]
fn detail_datatable<'a, T>(
    d: &'a NetlinkDetailState,
    which: NetlinkTable,
    topic: NetlinkDetailTopic,
    fetch: &'a Fetch<Vec<T>>,
    title: &'static str,
    noun: &'static str,
    columns: Vec<DataColumn<'a, T, Message>>,
    searchable: impl Fn(&T) -> String + 'a,
) -> Element<'a, Message> {
    let loading = fetch.is_loading();
    let label = if loading {
        format!("Fetching {title}…")
    } else {
        format!("Fetch {title}")
    };
    let mut refresh = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        refresh = refresh.on_press(Message::FetchNetlinkDetail(topic));
    }
    let head = row![section_header(title, None), refresh]
        .spacing(space::MD)
        .align_y(iced::Alignment::Center);
    let body: Element<'a, Message> = if let Some(err) = fetch.error() {
        empty_state(format!("{title} fetch failed: {err}"), None)
    } else if let Some(rows) = fetch.ready() {
        DataTable::new(columns)
            .searchable(searchable)
            .on_sort(move |c| Message::NetlinkTableSort(which, c))
            .on_filter(move |f| Message::NetlinkTableFilter(which, f))
            .on_more(Message::NetlinkTableMore(which))
            .noun(noun)
            .view(rows, d.table(which))
    } else {
        empty_state(format!("Fetch {title} to load"), None)
    };
    column![head, body].spacing(space::SM).into()
}

fn routes_columns<'a>() -> Vec<DataColumn<'a, RouteRecord, Message>> {
    vec![
        DataColumn::fill("destination", 3, |r: &RouteRecord| {
            text(r.dst.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &RouteRecord| SortKey::Text(r.dst.clone())),
        DataColumn::fill("gateway", 2, |r: &RouteRecord| {
            text(r.gateway.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &RouteRecord| SortKey::Text(r.gateway.clone().unwrap_or_default())),
        DataColumn::fixed("proto", 90.0, |r: &RouteRecord| {
            text(r.protocol.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &RouteRecord| SortKey::Text(r.protocol.clone())),
        DataColumn::fixed("scope", 90.0, |r: &RouteRecord| {
            text(r.scope.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &RouteRecord| SortKey::Text(r.scope.clone())),
    ]
}

fn neighbors_columns<'a>() -> Vec<DataColumn<'a, NeighborRecord, Message>> {
    vec![
        DataColumn::fill("ip", 3, |n: &NeighborRecord| {
            text(n.ip.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|n: &NeighborRecord| SortKey::Text(n.ip.clone().unwrap_or_default())),
        DataColumn::fill("mac", 3, |n: &NeighborRecord| {
            text(n.mac.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        }),
        DataColumn::fixed("state", 120.0, |n: &NeighborRecord| {
            text(n.state.clone()).size(font::CAPTION).into()
        })
        .sortable(|n: &NeighborRecord| SortKey::Text(n.state.clone())),
    ]
}

fn addresses_columns<'a>() -> Vec<DataColumn<'a, AddressRecord, Message>> {
    vec![
        DataColumn::fill("address", 3, |a: &AddressRecord| {
            let s = match &a.ip {
                Some(ip) => format!("{ip}/{}", a.prefix_len),
                None => "-".to_string(),
            };
            text(s).size(font::CAPTION).into()
        })
        .sortable(|a: &AddressRecord| SortKey::Text(a.ip.clone().unwrap_or_default())),
        DataColumn::fixed("scope", 90.0, |a: &AddressRecord| {
            text(a.scope.clone()).size(font::CAPTION).into()
        })
        .sortable(|a: &AddressRecord| SortKey::Text(a.scope.clone())),
        DataColumn::fill("label", 2, |a: &AddressRecord| {
            text(a.label.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        }),
        DataColumn::fixed("ifindex", 70.0, |a: &AddressRecord| {
            text(a.ifindex.to_string()).size(font::CAPTION).into()
        })
        .sortable(|a: &AddressRecord| SortKey::Num(a.ifindex as f64)),
    ]
}

fn route_changes_columns<'a>() -> Vec<DataColumn<'a, RouteChangeRecord, Message>> {
    vec![
        DataColumn::fixed("when", 160.0, |c: &RouteChangeRecord| {
            text(crate::view::formatting::format_timestamp(
                c.ts_unix as i64 * 1000,
            ))
            .size(font::CAPTION)
            .into()
        })
        .sortable(|c: &RouteChangeRecord| SortKey::Num(c.ts_unix as f64)),
        DataColumn::fixed("family", 70.0, |c: &RouteChangeRecord| {
            text(c.family.clone()).size(font::CAPTION).into()
        }),
        DataColumn::fixed("action", 90.0, |c: &RouteChangeRecord| {
            text(c.action.clone()).size(font::CAPTION).into()
        })
        .sortable(|c: &RouteChangeRecord| SortKey::Text(c.action.clone())),
        DataColumn::fill("gateway", 2, |c: &RouteChangeRecord| {
            text(c.gateway.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        }),
        DataColumn::fill("was", 2, |c: &RouteChangeRecord| {
            text(c.prev_gateway.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        }),
    ]
}

/// Events tab (#265): a structured, filterable control-plane timeline
/// (`@/query/events`, newest-first) across link/addr/route/neighbor/ipsec, with
/// per-family event counters as a context chart. Family/action are their own
/// columns so the DataTable filter box filters by either; `detail` is the
/// sensor's already-humanized field (iface name / ip / route dest), not raw JSON.
fn render_events_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.netlink_detail;
    let mut col = column![].spacing(space::MD);

    let fam = event_family_totals(state);
    if !fam.is_empty() {
        col = col.push(card(
            column![
                section_header("Event families", None),
                crate::view::chart::ranked_bar(&fam, |v| format!("{}", v as u64), 8),
            ]
            .spacing(space::XS),
        ));
    }

    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Events,
        NetlinkDetailTopic::Events,
        &d.events,
        "Event timeline",
        "events",
        events_columns(),
        |e: &EventRecord| format!("{} {} {}", e.family, e.action, e.detail),
    )));

    col
}

fn events_columns<'a>() -> Vec<DataColumn<'a, EventRecord, Message>> {
    vec![
        DataColumn::fixed("when", 160.0, |e: &EventRecord| {
            text(crate::view::formatting::format_timestamp(
                e.ts_unix as i64 * 1000,
            ))
            .size(font::CAPTION)
            .into()
        })
        .sortable(|e: &EventRecord| SortKey::Num(e.ts_unix as f64)),
        DataColumn::fixed("family", 80.0, |e: &EventRecord| {
            badge(family_color(&e.family), e.family.clone())
        })
        .sortable(|e: &EventRecord| SortKey::Text(e.family.clone())),
        DataColumn::fixed("action", 90.0, |e: &EventRecord| {
            text(e.action.clone()).size(font::CAPTION).into()
        })
        .sortable(|e: &EventRecord| SortKey::Text(e.action.clone())),
        DataColumn::fixed("iface", 60.0, |e: &EventRecord| {
            text(
                e.ifindex
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "-".into()),
            )
            .size(font::CAPTION)
            .into()
        }),
        DataColumn::fill("detail", 3, |e: &EventRecord| {
            text(e.detail.clone()).size(font::CAPTION).into()
        }),
    ]
}

/// Per-family event totals (`events/<family>/*_total`) for the context chart.
fn event_family_totals(state: &DeviceDetailState) -> Vec<(String, f64)> {
    let mut map: BTreeMap<String, f64> = BTreeMap::new();
    for (m, p) in &state.metrics {
        if let Some(rest) = m.strip_prefix("events/")
            && let Some((fam, _)) = rest.split_once('/')
            && let Some(v) = tv_num(&p.value)
        {
            *map.entry(fam.to_string()).or_default() += v;
        }
    }
    let mut v: Vec<(String, f64)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

/// Distinct colors per control-plane event family (for the timeline badges).
fn family_color(family: &str) -> iced::Color {
    match family {
        "link" => theme::SEVERITY_INFO,
        "addr" | "address" => theme::STATUS_ONLINE,
        "route" => theme::ACCENT_GOLD,
        "neigh" | "neighbor" => theme::STATUS_DEGRADED,
        "ipsec" | "xfrm" => theme::SEVERITY_CRITICAL,
        _ => theme::STATUS_UNKNOWN,
    }
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
/// Firewall & IPsec tab (#264): conntrack utilization gauge + per-proto donut,
/// nft per-rule hit-rate table, and the xfrm/IPsec SA inventory + lifecycle.
fn render_firewall_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.netlink_detail;
    let mut col = column![].spacing(space::MD);

    if has_prefix(state, "conntrack/") {
        col = col.push(card(render_conntrack(state)));
    }
    // nft per-rule hit-rate (@/query/nft) with decoded packet/byte counters.
    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Nft,
        NetlinkDetailTopic::Nft,
        &d.nft,
        "nftables rules",
        "rules",
        nft_columns(),
        |r: &NftRuleRecord| format!("{} {} {}", r.family, r.table, r.chain),
    )));
    // IPsec / xfrm summary + lifecycle counters, then the SA inventory.
    if has_prefix(state, "xfrm/") || has_prefix(state, "events/ipsec/") {
        col = col.push(card(render_xfrm(state)));
    }
    col = col.push(card(detail_datatable(
        d,
        NetlinkTable::Xfrm,
        NetlinkDetailTopic::Xfrm,
        &d.xfrm,
        "IPsec SAs",
        "SAs",
        xfrm_columns(),
        |s: &XfrmSaRecord| {
            format!(
                "{} {} {}",
                s.src.as_deref().unwrap_or(""),
                s.dst.as_deref().unwrap_or(""),
                s.proto
            )
        },
    )));

    col
}

fn render_conntrack(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Conntrack", None);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 120)].spacing(8);

    let mut col = column![
        title,
        line("entries", "conntrack/entries"),
        line("max", "conntrack/max"),
    ]
    .spacing(4);

    // Utilization is a 0..1 ratio → a proper gauge (kills the inline *100 hack).
    if let Some(u) = fval(state, "conntrack/utilization") {
        col = col.push(
            Gauge::percentage(u * 100.0, "utilization")
                .with_thresholds(0.75, 0.9)
                .with_width(200.0)
                .view(),
        );
    }

    // Per-protocol breakdown donut.
    let mix = conntrack_proto_mix(state);
    if !mix.is_empty() {
        col = col.push(text("by protocol").size(font::CAPTION).style(dim));
        col = col.push(crate::view::chart::donut(&mix, 120.0));
    }
    col.into()
}

/// Conntrack entries per protocol (`conntrack/by_proto/*`) for the donut.
fn conntrack_proto_mix(state: &DeviceDetailState) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for p in ["tcp", "udp", "icmp", "other"] {
        if let Some(v) = fval(state, &format!("conntrack/by_proto/{p}"))
            && v > 0.0
        {
            out.push((p.to_string(), v));
        }
    }
    out
}

fn nft_columns<'a>() -> Vec<DataColumn<'a, NftRuleRecord, Message>> {
    vec![
        DataColumn::fixed("family", 70.0, |r: &NftRuleRecord| {
            text(r.family.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &NftRuleRecord| SortKey::Text(r.family.clone())),
        DataColumn::fill("table", 2, |r: &NftRuleRecord| {
            text(r.table.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &NftRuleRecord| SortKey::Text(r.table.clone())),
        DataColumn::fill("chain", 2, |r: &NftRuleRecord| {
            text(r.chain.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &NftRuleRecord| SortKey::Text(r.chain.clone())),
        DataColumn::fixed("packets", 90.0, |r: &NftRuleRecord| {
            text(fmt_count(r.packets)).size(font::CAPTION).into()
        })
        .sortable(|r: &NftRuleRecord| SortKey::Num(r.packets as f64)),
        DataColumn::fixed("bytes", 90.0, |r: &NftRuleRecord| {
            text(fmt_bytes(r.bytes)).size(font::CAPTION).into()
        })
        .sortable(|r: &NftRuleRecord| SortKey::Num(r.bytes as f64)),
        DataColumn::fill("comment", 2, |r: &NftRuleRecord| {
            text(r.comment.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        }),
    ]
}

fn xfrm_columns<'a>() -> Vec<DataColumn<'a, XfrmSaRecord, Message>> {
    vec![
        DataColumn::fill("src", 3, |s: &XfrmSaRecord| {
            text(s.src.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|s: &XfrmSaRecord| SortKey::Text(s.src.clone().unwrap_or_default())),
        DataColumn::fill("dst", 3, |s: &XfrmSaRecord| {
            text(s.dst.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|s: &XfrmSaRecord| SortKey::Text(s.dst.clone().unwrap_or_default())),
        DataColumn::fixed("proto", 70.0, |s: &XfrmSaRecord| {
            text(s.proto.clone()).size(font::CAPTION).into()
        }),
        DataColumn::fixed("mode", 90.0, |s: &XfrmSaRecord| {
            text(s.mode.clone()).size(font::CAPTION).into()
        }),
        DataColumn::fixed("spi", 100.0, |s: &XfrmSaRecord| {
            text(format!("{:#x}", s.spi)).size(font::CAPTION).into()
        }),
        DataColumn::fixed("bytes", 90.0, |s: &XfrmSaRecord| {
            text(fmt_bytes(s.bytes)).size(font::CAPTION).into()
        })
        .sortable(|s: &XfrmSaRecord| SortKey::Num(s.bytes as f64)),
    ]
}

/// WireGuard peers section: one sub-table per WG interface.
/// WireGuard tab (#266): a summary line (interfaces / peers / active) over
/// per-peer cards with handshake-age chips, up/stale status, endpoint, and
/// rx/tx throughput trend tiles.
fn render_wireguard(state: &DeviceDetailState) -> Element<'_, Message> {
    let wg = wireguard(state);
    let n_ifaces = wg.len();
    let mut n_peers = 0usize;
    let mut n_active = 0usize;
    for peers in wg.values() {
        for stats in peers.values() {
            n_peers += 1;
            if matches!(
                stats.get("up").map(|p| &p.value),
                Some(TelemetryValue::Boolean(true))
            ) {
                n_active += 1;
            }
        }
    }
    let summary = text(format!(
        "{n_ifaces} interfaces · {n_peers} peers · {n_active} active"
    ))
    .size(font::CAPTION)
    .style(dim);
    let mut col = column![section_header("WireGuard", None), summary].spacing(space::SM);

    for (iface, peers) in &wg {
        let count = num(state
            .metrics
            .get(&format!("wireguard/{iface}/peers"))
            .map(|p| &p.value));
        col = col.push(text(format!("{iface} — {count} peers")).size(font::EMPHASIS));
        for (peer, stats) in peers {
            col = col.push(card(render_wg_peer(state, iface, peer, stats)));
        }
    }
    col.into()
}

/// One WireGuard peer card: id + up/stale + handshake-age chips + endpoint, and
/// rx/tx throughput trend tiles.
fn render_wg_peer<'a>(
    state: &'a DeviceDetailState,
    iface: &str,
    peer: &str,
    stats: &BTreeMap<String, &zensight_common::TelemetryPoint>,
) -> Element<'a, Message> {
    let endpoint = stats
        .get("rx_bytes")
        .and_then(|p| p.labels.get("endpoint"))
        .cloned()
        .unwrap_or_else(|| "-".into());
    let up = matches!(
        stats.get("up").map(|p| &p.value),
        Some(TelemetryValue::Boolean(true))
    );
    let up_chip = badge(
        if up {
            theme::STATUS_ONLINE
        } else {
            theme::STATUS_OFFLINE
        },
        if up { "up" } else { "stale" }.to_string(),
    );
    // Handshake-age chip: fresh (green) < 180s, aging (amber) < 900s, else red.
    let hs_chip = match stats.get("last_handshake_age_s").map(|p| &p.value) {
        Some(TelemetryValue::Gauge(a)) => {
            let color = if *a < 180.0 {
                theme::STATUS_ONLINE
            } else if *a < 900.0 {
                theme::STATUS_DEGRADED
            } else {
                theme::STATUS_OFFLINE
            };
            badge(color, format!("handshake {a:.0}s ago"))
        }
        _ => badge(theme::STATUS_UNKNOWN, "handshake never".to_string()),
    };
    // Prefer the wg-quick AllowedIPs label (#268) as a readable name; else the
    // short public key (keys are long).
    let name = stats
        .get("rx_bytes")
        .and_then(|p| p.labels.get("allowed_ips"))
        .cloned()
        .unwrap_or_else(|| {
            if peer.len() > 14 {
                format!("{}…", &peer[..14])
            } else {
                peer.to_string()
            }
        });

    let header = row![
        text(name).size(font::EMPHASIS),
        up_chip,
        hs_chip,
        text(endpoint).size(font::CAPTION).style(dim),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let throughput = row![
        tput_tile(state, "rx ↓", &format!("wireguard/{iface}/{peer}/rx_bytes")),
        tput_tile(state, "tx ↑", &format!("wireguard/{iface}/{peer}/tx_bytes")),
    ]
    .spacing(space::LG);

    column![header, throughput].spacing(space::SM).into()
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
