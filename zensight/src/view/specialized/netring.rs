//! Netring sensor specialized view — a tabbed, chart-driven, drill-down surface
//! (Overview · Flows · Talkers & Matrix · DNS · HTTP/TLS · Bandwidth · Assets ·
//! Security · Capture) over the sensor's streamed metrics and `@/query/*`
//! channels (#247, epic #257).

use iced::Element;
use iced::widget::{Column, button, column, container, row, scrollable, text};
use iced::{Length, Theme};
use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::chart;
use crate::view::components::{
    Column as TableColumn, DataTable, SortKey, TabItem, badge, card, empty_state, section_header,
    tabbed_view,
};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::{format_bytes, format_count, format_rate};
use crate::view::specialized::SpecializedTab;
use crate::view::specialized::fetch::Fetch;
use crate::view::specialized::netring_detail::NetringTable;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the netring sensor specialized view: a header + the tabbed container
/// over the active tab's content (#247).
pub fn netring_sensor_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let tabs = netring_tabs(state);
    // Fall back to Overview if the remembered tab is currently hidden (e.g. the
    // DNS tab after the sensor stopped publishing `dns/`).
    let active = if tabs
        .iter()
        .any(|t| t.visible && t.id == state.specialized_tab)
    {
        state.specialized_tab
    } else {
        SpecializedTab::Overview
    };
    let device_id = state.device_id.clone();
    let content = netring_tab_content(state, active);
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

/// The netring tab strip, capability-aware: tabs render only when the sensor
/// publishes the data (or a fetch was attempted). Overview / Flows / Talkers &
/// Matrix / HTTP-TLS / Bandwidth are always available (streamed or on-demand).
fn netring_tabs(state: &DeviceDetailState) -> Vec<TabItem<SpecializedTab>> {
    use SpecializedTab::*;
    vec![
        TabItem::new(Overview, "Overview"),
        TabItem::new(Flows, "Flows"),
        TabItem::new(TalkersMatrix, "Talkers & Matrix"),
        TabItem::new(Dns, "DNS").visible(has_prefix(state, "dns/")),
        TabItem::new(HttpTls, "HTTP/TLS"),
        TabItem::new(Bandwidth, "Bandwidth"),
        TabItem::new(Assets, "Assets").visible(
            has_prefix(state, "assets/") || !matches!(state.netring_detail.assets, Fetch::Idle),
        ),
        TabItem::new(Security, "Security")
            .visible(!state.netring_detail.anomalies.is_empty())
            .badge(state.netring_detail.anomalies.len()),
        TabItem::new(Capture, "Capture")
            .visible(state.metrics.keys().any(|k| k.starts_with("capture/"))),
    ]
}

/// Build the scrollable content for a netring tab by composing the existing
/// per-section cards. No data regression: every card in the old single-scroll
/// view is reachable from exactly one tab.
fn netring_tab_content(state: &DeviceDetailState, tab: SpecializedTab) -> Element<'_, Message> {
    use SpecializedTab::*;
    let inner: Column<'_, Message> = match tab {
        Overview => {
            let mut c = column![].spacing(space::MD);
            // Live anomaly strip: a compact rollup of firing detectors that
            // click-throughs to the Security tab (#253).
            if let Some(strip) = anomaly_strip(state) {
                c = c.push(strip);
            }
            c = c
                .push(card(render_flows(state)))
                .push(card(render_tcp_health(state)));
            if has_prefix(state, "flow/by_l4/") {
                c = c.push(card(render_per_l4(state)));
            }
            c
        }
        Flows => column![
            card(render_flow_detail(state)),
            card(render_elephants(state))
        ]
        .spacing(space::MD),
        TalkersMatrix => {
            column![card(render_talkers(state)), card(render_matrix(state))].spacing(space::MD)
        }
        Dns => column![card(render_dns(state))].spacing(space::MD),
        HttpTls => {
            let mut c = column![].spacing(space::MD);
            if has_prefix(state, "http/") {
                c = c.push(card(render_http(state)));
            }
            c = c.push(card(render_tls(state)));
            if has_prefix(state, "quic/") || !matches!(state.netring_detail.quic, Fetch::Idle) {
                c = c.push(card(render_quic(state)));
            }
            if has_prefix(state, "ssh/") || !matches!(state.netring_detail.ssh, Fetch::Idle) {
                c = c.push(card(render_ssh(state)));
            }
            c
        }
        Bandwidth => column![card(render_bandwidth(state))].spacing(space::MD),
        Assets => column![card(render_assets(state))].spacing(space::MD),
        Capture => column![card(render_capture(state))].spacing(space::MD),
        Security => column![card(render_netring_security(state))].spacing(space::MD),
    };
    scrollable(inner.width(Length::Fill))
        .height(Length::Fill)
        .into()
}

/// TLS section: streamed handshake aggregates + an on-demand fingerprint
/// inventory (SNI / JA4) fetched from `@/query/tls`.
fn render_tls(state: &DeviceDetailState) -> Element<'_, Message> {
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let loading = state.netring_detail.tls.is_loading();
    let label = if loading {
        "Fetching…"
    } else {
        "Fetch inventory"
    };
    let mut fetch = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringTls);
    }

    let mut col = column![
        section_header("TLS", Some(fetch.into())),
        row![
            cell("handshakes (total)", 180),
            cell(&get("tls/handshakes_total"), 100)
        ]
        .spacing(8),
        row![
            cell("distinct fingerprints", 180),
            cell(&get("tls/distinct_fingerprints"), 100)
        ]
        .spacing(8),
    ]
    .spacing(space::SM);

    if let Some(err) = state.netring_detail.tls.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.tls.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No TLS handshakes observed", None));
        } else {
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("sni", 220),
                    cell("ja4", 220),
                    cell("ja3", 220),
                    cell("alpn", 90),
                    cell("count", 60)
                ]
                .spacing(8),
            );
            for r in records.iter().take(200) {
                list = list.push(
                    row![
                        cell(r.sni.as_deref().unwrap_or("-"), 220),
                        cell(r.ja4.as_deref().unwrap_or("-"), 220),
                        // JA3 was fetched but never rendered before (#45).
                        cell(r.ja3.as_deref().unwrap_or("-"), 220),
                        cell(r.alpn.as_deref().unwrap_or("-"), 90),
                        cell(&r.count.to_string(), 60),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("{} fingerprints", records.len())).size(font::EMPHASIS))
                .push(list);
        }
    }
    col.into()
}

/// Drop-rate fraction at/above which the capture-health card flags an overload
/// badge — mirrors the netring `OverloadDetector` default enter threshold (5%),
/// so the GUI's local signal lines up with the `capture-overload` alert (#71).
const OVERLOAD_DROP_RATE: f64 = 0.05;

/// QUIC section (#72): streamed distinct-SNI count + an on-demand SNI/ALPN/version
/// inventory fetched from `@/query/quic` — the QUIC analogue of the TLS card.
fn render_quic(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.quic.is_loading();
    let label = if loading { "Fetching…" } else { "Fetch QUIC" };
    let mut fetch = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringQuic);
    }

    let count = num(state.metrics.get("quic/distinct_sni").map(|p| &p.value));
    let mut col = column![
        section_header("QUIC (SNI / ALPN)", Some(fetch.into())),
        row![cell("distinct SNI", 180), cell(&count, 100)].spacing(8),
    ]
    .spacing(space::SM);

    if let Some(err) = state.netring_detail.quic.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.quic.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No QUIC Initials observed", None));
        } else {
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("sni", 280),
                    cell("alpn", 120),
                    cell("version", 90),
                    cell("count", 60),
                ]
                .spacing(8),
            );
            for r in records.iter().take(200) {
                list = list.push(
                    row![
                        cell(r.sni.as_deref().unwrap_or("-"), 280),
                        cell(&join_or_dash(&r.alpn), 120),
                        cell(&r.version, 90),
                        cell(&r.count.to_string(), 60),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("{} SNI/version pairs", records.len())).size(font::EMPHASIS))
                .push(list);
        }
    }
    col.into()
}

/// SSH section (#72): streamed distinct-HASSH count + an on-demand HASSH
/// inventory (fingerprint · role · banner) fetched from `@/query/ssh`.
fn render_ssh(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.ssh.is_loading();
    let label = if loading { "Fetching…" } else { "Fetch SSH" };
    let mut fetch = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringSsh);
    }

    let count = num(state.metrics.get("ssh/distinct_hassh").map(|p| &p.value));
    let mut col = column![
        section_header("SSH (HASSH)", Some(fetch.into())),
        row![cell("distinct HASSH", 180), cell(&count, 100)].spacing(8),
    ]
    .spacing(space::SM);

    if let Some(err) = state.netring_detail.ssh.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.ssh.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No SSH handshakes observed", None));
        } else {
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("hassh", 260),
                    cell("role", 70),
                    cell("banner", 220),
                    cell("count", 60),
                ]
                .spacing(8),
            );
            for r in records.iter().take(200) {
                list = list.push(
                    row![
                        cell(&r.hassh, 260),
                        cell(&r.role, 70),
                        cell(r.banner.as_deref().unwrap_or("-"), 220),
                        cell(&r.count.to_string(), 60),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("{} fingerprints", records.len())).size(font::EMPHASIS))
                .push(list);
        }
    }
    col.into()
}

/// Passive asset-inventory section (#70): a streamed discovered-count plus an
/// on-demand table (MAC · IP · hostname · platform · capabilities · seen-via)
/// fetched from `@/query/assets`. Surfaces hosts seen on the wire that emit no
/// telemetry of their own — the discovery the topology/devices views lack.
fn render_assets(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.assets.is_loading();
    let label = if loading {
        "Fetching…"
    } else {
        "Fetch assets"
    };
    let mut fetch = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringAssets);
    }

    let discovered = num(state.metrics.get("assets/discovered").map(|p| &p.value));
    let mut col = column![
        section_header("Assets (passive discovery)", Some(fetch.into())),
        row![cell("discovered", 180), cell(&discovered, 100)].spacing(8),
    ]
    .spacing(space::SM);

    if let Some(err) = state.netring_detail.assets.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.assets.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No assets discovered yet", None));
        } else {
            col = col.push(assets_table(records, state));
        }
    }
    col.into()
}

/// First IPv4 (else first IPv6) of an asset, or `"-"`.
fn asset_ip(r: &zensight_common::AssetRecord) -> &str {
    r.ipv4
        .first()
        .or_else(|| r.ipv6.first())
        .map(String::as_str)
        .unwrap_or("-")
}

/// Assets tab table (#252): first-class, filterable/sortable inventory. The IP
/// column is a drill-down pivot to the asset's flows.
fn assets_table<'a>(
    records: &'a [zensight_common::AssetRecord],
    state: &'a DeviceDetailState,
) -> Element<'a, Message> {
    use zensight_common::AssetRecord;
    let columns = vec![
        TableColumn::fill("mac", 3, |r: &AssetRecord| {
            text(r.mac.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &AssetRecord| SortKey::Text(r.mac.clone())),
        // IP → flows pivot (asset drill-down, #246/#252).
        TableColumn::fill("ip", 3, |r: &AssetRecord| {
            let ip = asset_ip(r);
            if ip == "-" {
                text("-").size(font::CAPTION).into()
            } else {
                pivot_button(state, ip, ip)
            }
        })
        .sortable(|r: &AssetRecord| SortKey::Text(asset_ip(r).to_string())),
        TableColumn::fill("hostname", 3, |r: &AssetRecord| {
            text(r.hostname.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &AssetRecord| SortKey::Text(r.hostname.clone().unwrap_or_default())),
        // vendor was collected (DHCP opt 60 / LLDP / SSDP) but never rendered (#120).
        TableColumn::fill("vendor", 3, |r: &AssetRecord| {
            text(r.vendor.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &AssetRecord| SortKey::Text(r.vendor.clone().unwrap_or_default())),
        TableColumn::fill("platform", 3, |r: &AssetRecord| {
            text(r.platform.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        }),
        TableColumn::fill("caps", 3, |r: &AssetRecord| {
            text(join_or_dash(&r.capabilities))
                .size(font::CAPTION)
                .into()
        }),
        TableColumn::fill("seen via", 2, |r: &AssetRecord| {
            text(join_or_dash(&r.seen_via)).size(font::CAPTION).into()
        }),
    ];
    DataTable::new(columns)
        .searchable(|r: &AssetRecord| {
            format!(
                "{} {} {} {}",
                r.mac,
                asset_ip(r),
                r.hostname.clone().unwrap_or_default(),
                r.vendor.clone().unwrap_or_default(),
            )
        })
        .on_sort(|c| Message::NetringTableSort(NetringTable::Assets, c))
        .on_filter(|q| Message::NetringTableFilter(NetringTable::Assets, q))
        .on_more(Message::NetringTableMore(NetringTable::Assets))
        .noun("assets")
        .view(records, state.netring_detail.table(NetringTable::Assets))
}

/// Join a slug list with commas, or `"-"` when empty.
fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

/// Capture self-health section (#71): packets/drops/drop_rate per source, the
/// honest drop breakdown (AF_PACKET freezes / AF_XDP ring + descriptor causes),
/// and an "OVERLOAD" badge when a source is shedding ≥5% of packets — the trust
/// signal that the sensor's *other* telemetry is currently incomplete.
fn render_capture(state: &DeviceDetailState) -> Element<'_, Message> {
    // Group capture/<src>/<stat>; `stat` may itself be `xdp/<cause>`.
    let mut sources: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, &TelemetryValue>,
    > = Default::default();
    for (metric, point) in &state.metrics {
        if let Some(rest) = metric.strip_prefix("capture/")
            && let Some((src, stat)) = rest.split_once('/')
            // `capture/focus/*` is the reloadable-filter counter, not a NIC leg —
            // surfaced separately below, so keep it out of the per-source table.
            && src != "focus"
        {
            sources
                .entry(src.to_string())
                .or_default()
                .insert(stat.to_string(), &point.value);
        }
    }

    // Resolved capture backend (#227): af_packet / af_xdp / pcap-replay.
    let backend = match state.metrics.get("capture/backend") {
        Some(p) => match &p.value {
            TelemetryValue::Text(s) => Some(s.clone()),
            _ => None,
        },
        None => None,
    };

    // Deliberate load-shedding (#224): a source is sampling when its `shed/active`
    // gauge is set. Sum the cumulative shed counters across shedding sources.
    let mut shed_dropped: u64 = 0;
    let mut shedding = false;
    for stats in sources.values() {
        if matches!(stats.get("shed/active"), Some(TelemetryValue::Gauge(g)) if *g >= 1.0) {
            shedding = true;
            for leaf in ["shed/new_flows_total", "shed/sampled_total"] {
                if let Some(TelemetryValue::Counter(c)) = stats.get(leaf) {
                    shed_dropped += *c;
                }
            }
        }
    }

    // Overload if any source's windowed drop_rate is at/above the threshold.
    let overloaded = sources.values().any(|stats| {
        matches!(stats.get("drop_rate"), Some(TelemetryValue::Gauge(r)) if *r >= OVERLOAD_DROP_RATE)
    });
    let badge: Option<Element<'_, Message>> = overloaded.then(|| {
        text("⚠ OVERLOAD — losing packets")
            .size(font::CAPTION)
            .style(danger)
            .into()
    });

    let mut col = column![section_header("Capture Health", badge)].spacing(space::SM);

    // Resolved-backend badge — what's actually live (AF_PACKET / AF_XDP / replay).
    if let Some(b) = &backend {
        col = col.push(text(format!("backend: {b}")).size(font::CAPTION).style(dim));
    }

    // Unmistakable shedding banner: the sensor is *deliberately* dropping new
    // flows, so the rest of the telemetry is a sample — say so plainly (#224).
    if shedding {
        col = col.push(
            row![
                text("⚠ SHEDDING — data is sampled")
                    .size(font::EMPHASIS)
                    .style(warn),
                text(format!(
                    "({} flows deliberately dropped)",
                    format_count(shed_dropped)
                ))
                .size(font::CAPTION)
                .style(dim),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }

    let mut list = Column::new().spacing(3).push(
        row![
            cell("source", 90),
            cell("packets", 120),
            cell("drops", 100),
            cell("drop rate", 100),
            cell("freezes", 90),
        ]
        .spacing(8),
    );
    for (src, stats) in &sources {
        // Counters read large; scale them (1.2M) but keep small values exact.
        let g = |s: &str| match stats.get(s).copied() {
            Some(TelemetryValue::Counter(c)) => format_count(*c),
            other => num(other),
        };
        let dr = match stats.get("drop_rate") {
            Some(TelemetryValue::Gauge(r)) => Some(*r),
            _ => None,
        };
        let drop_rate = dr
            .map(|r| format!("{:.2}%", r * 100.0))
            .unwrap_or_else(|| "-".into());
        // Tint the drop-rate per source: danger at/above the overload threshold,
        // warning once it's non-trivial, so the lossy source stands out in the row.
        let drop_cell = match dr {
            Some(r) if r >= OVERLOAD_DROP_RATE => cell_styled(&drop_rate, 100, danger),
            Some(r) if r >= 0.01 => cell_styled(&drop_rate, 100, warn),
            _ => cell(&drop_rate, 100),
        };
        list = list.push(
            row![
                cell(src, 90),
                cell(&g("packets"), 120),
                cell(&g("drops"), 100),
                drop_cell,
                cell(&g("freezes"), 90),
            ]
            .spacing(8),
        );
        // AF_XDP per-cause breakdown (only present on XDP sources).
        let xdp: Vec<(String, String)> = stats
            .iter()
            .filter_map(|(stat, v)| {
                let cause = stat.strip_prefix("xdp/")?;
                let formatted = match **v {
                    TelemetryValue::Counter(c) => format_count(c),
                    _ => num(Some(*v)),
                };
                Some((cause.to_string(), formatted))
            })
            .collect();
        for (cause, v) in xdp {
            // Indent via a spacer cell (not leading spaces) so the label text
            // node stays exactly findable.
            list = list.push(
                row![
                    cell("", 16),
                    cell(&format!("xdp/{cause}"), 264),
                    cell(&v, 120)
                ]
                .spacing(8),
            );
        }
    }
    col = col.push(list);
    col.into()
}

fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    row![
        text(format!("Netring: {}", state.device_id.source)).size(font::TITLE),
        text(format!("({} metrics)", state.metrics.len()))
            .size(font::CAPTION)
            .style(dim),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center)
    .into()
}

fn render_flows(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Flows", None);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let get_bytes = |m: &str| {
        metric_f64(state, m)
            .map(format_bytes)
            .unwrap_or_else(|| "-".into())
    };
    let get_count = |m: &str| {
        metric_f64(state, m)
            .map(|v| format_count(v as u64))
            .unwrap_or_else(|| "-".into())
    };
    column![
        title,
        row![
            cell("started (total)", 160),
            cell(&get("flow/started_total"), 100)
        ]
        .spacing(8),
        row![
            cell("ended (total)", 160),
            cell(&get("flow/ended_total"), 100)
        ]
        .spacing(8),
        row![cell("active", 160), cell(&get("flow/active"), 100)].spacing(8),
        row![
            cell("bytes (total)", 160),
            cell(&get_bytes("flow/bytes_total"), 100)
        ]
        .spacing(8),
        row![
            cell("packets (total)", 160),
            cell(&get_count("flow/packets_total"), 100)
        ]
        .spacing(8),
        row![
            cell("retransmits (total)", 160),
            cell(&get_count("flow/retransmits_total"), 100)
        ]
        .spacing(8),
    ]
    .spacing(4)
    .into()
}

/// Read a metric as `f64` (Counter or Gauge), `None` if absent or non-numeric.
fn metric_f64(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    match state.metrics.get(metric).map(|p| &p.value) {
        Some(TelemetryValue::Counter(c)) => Some(*c as f64),
        Some(TelemetryValue::Gauge(g)) => Some(*g),
        _ => None,
    }
}

/// TCP health: reset / connection-refused counters.
fn render_tcp_health(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("TCP Health", None);
    if !state.metrics.keys().any(|k| k.starts_with("tcp/")) {
        return column![title, empty_state("No TCP reset data", None)]
            .spacing(space::SM)
            .into();
    }
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    column![
        title,
        row![
            cell("resets (total)", 160),
            cell(&get("tcp/resets_total"), 100)
        ]
        .spacing(8),
        row![
            cell("refused (total)", 160),
            cell(&get("tcp/refused_total"), 100)
        ]
        .spacing(8),
        // Close-reason breakdown (#45).
        row![
            cell("closed fin (total)", 160),
            cell(&get("tcp/closed_fin_total"), 100)
        ]
        .spacing(8),
        row![
            cell("closed rst (total)", 160),
            cell(&get("tcp/closed_rst_total"), 100)
        ]
        .spacing(8),
        row![
            cell("closed idle (total)", 160),
            cell(&get("tcp/closed_idle_total"), 100)
        ]
        .spacing(8),
    ]
    .spacing(4)
    .into()
}

/// Whether any metric key starts with `prefix` (#45).
fn has_prefix(state: &DeviceDetailState, prefix: &str) -> bool {
    state.metrics.keys().any(|k| k.starts_with(prefix))
}

/// DNS tab (#250): RED tiles (rate / unanswered / RTT percentiles) + an rcode
/// bar chart + an on-demand top-SLD table with an NXDOMAIN callout.
fn render_dns(state: &DeviceDetailState) -> Element<'_, Message> {
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));

    // RED tiles.
    let tiles = row![
        metric_tile("queries", get("dns/queries_total")),
        metric_tile("unanswered", get("dns/unanswered_total")),
        metric_tile("RTT p50 (ms)", get("dns/query_rtt_p50_ms")),
        metric_tile("RTT p95 (ms)", get("dns/query_rtt_p95_ms")),
        metric_tile("RTT p99 (ms)", get("dns/query_rtt_p99_ms")),
    ]
    .spacing(space::SM);

    let mut col = column![section_header("DNS (RED)", None), tiles].spacing(space::SM);

    // Response-code breakdown (`dns/responses_by_rcode/<rcode>_total`) as a
    // ranked bar chart instead of a text list.
    let mut rcodes: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            let r = m.strip_prefix("dns/responses_by_rcode/")?;
            Some((
                r.trim_end_matches("_total").to_string(),
                value_f64(&p.value),
            ))
        })
        .collect();
    rcodes.sort_by(|a, b| b.1.total_cmp(&a.1));
    if !rcodes.is_empty() {
        col = col
            .push(text("by rcode").size(font::CAPTION).style(dim))
            .push(chart::ranked_bar(&rcodes, |v| format_count(v as u64), 8));
    }

    // On-demand top-SLD / top-NXDOMAIN drill-down via `@/query/dns`.
    let loading = state.netring_detail.dns.is_loading();
    let mut fetch = button(
        text(if loading {
            "Fetching…"
        } else {
            "Fetch top domains"
        })
        .size(font::CAPTION),
    )
    .padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringDns);
    }
    col = col.push(fetch);
    if let Some(err) = state.netring_detail.dns.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.dns.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No DNS detail", None));
        } else {
            // NXDOMAIN callout: how many SLDs returned NXDOMAIN (a DGA / NOD
            // signal that pivots to the Security tab).
            let nx = records.iter().filter(|r| r.nxdomain > 0).count();
            if nx > 0 {
                col = col.push(
                    text(format!("⚠ {nx} domain(s) returned NXDOMAIN"))
                        .size(font::CAPTION)
                        .style(warn),
                );
            }
            let columns = vec![
                TableColumn::fill("domain", 4, |r: &zensight_common::DnsRecord| {
                    text(r.domain.clone()).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::DnsRecord| SortKey::Text(r.domain.clone())),
                TableColumn::fixed("queries", 100.0, |r: &zensight_common::DnsRecord| {
                    text(r.queries.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::DnsRecord| SortKey::Num(r.queries as f64)),
                TableColumn::fixed("nxdomain", 100.0, |r: &zensight_common::DnsRecord| {
                    let t = text(r.nxdomain.to_string()).size(font::CAPTION);
                    if r.nxdomain > 0 { t.style(warn) } else { t }.into()
                })
                .sortable(|r: &zensight_common::DnsRecord| SortKey::Num(r.nxdomain as f64)),
            ];
            let table = DataTable::new(columns)
                .searchable(|r: &zensight_common::DnsRecord| r.domain.clone())
                .on_sort(|c| Message::NetringTableSort(NetringTable::Dns, c))
                .on_filter(|q| Message::NetringTableFilter(NetringTable::Dns, q))
                .on_more(Message::NetringTableMore(NetringTable::Dns))
                .noun("domains")
                .view(records, state.netring_detail.table(NetringTable::Dns));
            col = col.push(table);
        }
    }
    col.into()
}

/// HTTP RED card (#45): requests, status-class breakdown, latency, methods.
fn render_http(state: &DeviceDetailState) -> Element<'_, Message> {
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 200), cell(&get(metric), 120)].spacing(8);

    let mut col = column![
        section_header("HTTP (RED)", None),
        line("requests (total)", "http/requests_total"),
        line("2xx", "http/status_2xx_total"),
        line("3xx", "http/status_3xx_total"),
        line("4xx", "http/status_4xx_total"),
        line("5xx", "http/status_5xx_total"),
        line("latency p50 (ms)", "http/latency_p50_ms"),
        line("latency p95 (ms)", "http/latency_p95_ms"),
    ]
    .spacing(4);

    let mut methods: Vec<(String, String)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            let meth = m.strip_prefix("http/methods/")?.strip_suffix("_total")?;
            Some((meth.to_string(), num(Some(&p.value))))
        })
        .collect();
    methods.sort();
    if !methods.is_empty() {
        col = col.push(text("by method").size(font::CAPTION).style(dim));
        for (meth, v) in methods {
            col = col.push(row![cell(&format!("  {meth}"), 200), cell(&v, 120)].spacing(8));
        }
    }

    // On-demand top-hosts / error-hosts drill-down via `@/query/http` (#45).
    let loading = state.netring_detail.http.is_loading();
    let mut fetch = button(
        text(if loading {
            "Fetching…"
        } else {
            "Fetch top hosts"
        })
        .size(font::CAPTION),
    )
    .padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringHttp);
    }
    col = col.push(fetch);
    if let Some(err) = state.netring_detail.http.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.http.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No HTTP detail", None));
        } else {
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("host", 280),
                    cell("requests", 100),
                    cell("errors", 100),
                ]
                .spacing(8),
            );
            for r in records.iter().take(200) {
                list = list.push(
                    row![
                        cell(&r.host, 280),
                        cell(&r.requests.to_string(), 100),
                        cell(&r.errors.to_string(), 100),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("{} hosts", records.len())).size(font::EMPHASIS))
                .push(list);
        }
    }
    col.into()
}

/// Top-talker drill-down (#45): the per-destination histogram the sensor serves
/// on `@/query/talkers` — distinct from the per-app bandwidth card. "Who are the
/// major backends?" by bytes/packets/flows.
fn render_talkers(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.talkers.is_loading();
    let title = section_header("Top Talkers (on demand)", None);
    let mut fetch = button(
        text(if loading {
            "Fetching…"
        } else {
            "Fetch Talkers"
        })
        .size(font::CAPTION),
    )
    .padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringTalkers);
    }
    let mut col = column![title, fetch].spacing(space::SM);
    if let Some(err) = state.netring_detail.talkers.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.talkers.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No talkers", None));
        } else {
            // Ranked bar chart of the heaviest destinations by bytes.
            let bars: Vec<(String, f64)> = records
                .iter()
                .take(RANKED_BAR_ROWS)
                .map(|r| (r.dst.clone(), r.bytes as f64))
                .collect();
            col = col.push(chart::ranked_bar(&bars, format_bytes, RANKED_BAR_ROWS));

            let columns = vec![
                TableColumn::fill("destination", 4, |r: &zensight_common::TalkerRecord| {
                    pivot_button(state, &r.dst, &r.dst)
                })
                .sortable(|r: &zensight_common::TalkerRecord| SortKey::Text(r.dst.clone())),
                TableColumn::fixed("bytes", 120.0, |r: &zensight_common::TalkerRecord| {
                    text(format_bytes(r.bytes as f64)).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::TalkerRecord| SortKey::Num(r.bytes as f64)),
                TableColumn::fixed("packets", 100.0, |r: &zensight_common::TalkerRecord| {
                    text(format_count(r.packets)).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::TalkerRecord| SortKey::Num(r.packets as f64)),
                TableColumn::fixed("flows", 80.0, |r: &zensight_common::TalkerRecord| {
                    text(r.flows.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::TalkerRecord| SortKey::Num(r.flows as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &zensight_common::TalkerRecord| r.dst.clone())
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Talkers, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Talkers, q))
                    .on_more(Message::NetringTableMore(NetringTable::Talkers))
                    .noun("talkers")
                    .view(records, state.netring_detail.table(NetringTable::Talkers)),
            );
        }
    }
    col.into()
}

/// Traffic-matrix / service-map drill-down (#122): the heaviest `src → dst` pairs
/// by byte volume, served on `@/query/matrix`. "Who talks to whom?" — the service
/// map behind the per-destination Top Talkers card.
fn render_matrix(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.matrix.is_loading();
    let title = section_header("Service Map · Traffic Matrix (on demand)", None);
    let mut fetch = button(
        text(if loading {
            "Fetching…"
        } else {
            "Fetch Matrix"
        })
        .size(font::CAPTION),
    )
    .padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringMatrix);
    }
    let mut col = column![title, fetch].spacing(space::SM);
    if let Some(err) = state.netring_detail.matrix.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.matrix.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No traffic matrix yet", None));
        } else {
            // Heatmap: src (rows) × dst (cols), cell intensity = bytes. Capped so
            // the canvas stays bounded; the table below carries the full detail.
            if let Some(hm) = matrix_heatmap(records) {
                col = col.push(hm);
            }
            let columns = vec![
                TableColumn::fill("source", 4, |r: &zensight_common::MatrixRecord| {
                    text(r.src.clone()).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::MatrixRecord| SortKey::Text(r.src.clone())),
                TableColumn::fill("destination", 4, |r: &zensight_common::MatrixRecord| {
                    pivot_button(state, &r.dst, &r.dst)
                })
                .sortable(|r: &zensight_common::MatrixRecord| SortKey::Text(r.dst.clone())),
                TableColumn::fixed("bytes", 120.0, |r: &zensight_common::MatrixRecord| {
                    text(format_bytes(r.bytes as f64)).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::MatrixRecord| SortKey::Num(r.bytes as f64)),
                TableColumn::fixed("packets", 100.0, |r: &zensight_common::MatrixRecord| {
                    text(format_count(r.packets)).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::MatrixRecord| SortKey::Num(r.packets as f64)),
                TableColumn::fixed("flows", 80.0, |r: &zensight_common::MatrixRecord| {
                    text(r.flows.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &zensight_common::MatrixRecord| SortKey::Num(r.flows as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &zensight_common::MatrixRecord| {
                        format!("{} {}", r.src, r.dst)
                    })
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Matrix, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Matrix, q))
                    .on_more(Message::NetringTableMore(NetringTable::Matrix))
                    .noun("src→dst pairs")
                    .view(records, state.netring_detail.table(NetringTable::Matrix)),
            );
        }
    }
    col.into()
}

/// Largest square of the traffic matrix rendered as a heatmap (src rows × dst
/// cols, cell = bytes). `None` when there's nothing to plot. Capped at
/// [`MATRIX_HEATMAP_DIM`] rows/cols so the canvas stays bounded.
fn matrix_heatmap<'a>(records: &[zensight_common::MatrixRecord]) -> Option<Element<'a, Message>> {
    use std::collections::HashMap;
    let mut src_idx: HashMap<&str, usize> = HashMap::new();
    let mut dst_idx: HashMap<&str, usize> = HashMap::new();
    for r in records {
        let sn = src_idx.len();
        if sn < MATRIX_HEATMAP_DIM {
            src_idx.entry(r.src.as_str()).or_insert(sn);
        }
        let dn = dst_idx.len();
        if dn < MATRIX_HEATMAP_DIM {
            dst_idx.entry(r.dst.as_str()).or_insert(dn);
        }
    }
    if src_idx.is_empty() || dst_idx.is_empty() {
        return None;
    }
    let mut grid = vec![vec![0.0_f64; dst_idx.len()]; src_idx.len()];
    for r in records {
        if let (Some(&s), Some(&d)) = (src_idx.get(r.src.as_str()), dst_idx.get(r.dst.as_str())) {
            grid[s][d] += r.bytes as f64;
        }
    }
    Some(chart::heatmap(&grid, 16.0))
}

/// Elephant-flow drill-down (#45): the biggest recently-ended flows, served on
/// `@/query/elephant_flows`. "What were the biggest transfers?"
fn render_elephants(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.elephants.is_loading();
    let title = section_header("Elephant Flows (on demand)", None);
    let mut fetch = button(
        text(if loading {
            "Fetching…"
        } else {
            "Fetch Elephants"
        })
        .size(font::CAPTION),
    )
    .padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringElephants);
    }
    let mut col = column![title, fetch].spacing(space::SM);
    if let Some(err) = state.netring_detail.elephants.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.elephants.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No elephant flows", None));
        } else {
            use zensight_common::ElephantRecord;
            let columns = vec![
                TableColumn::fill("src", 4, |r: &ElephantRecord| {
                    pivot_button(state, &r.src, &r.src)
                })
                .sortable(|r: &ElephantRecord| SortKey::Text(r.src.clone())),
                TableColumn::fill("dst", 4, |r: &ElephantRecord| {
                    pivot_button(state, &r.dst, &r.dst)
                })
                .sortable(|r: &ElephantRecord| SortKey::Text(r.dst.clone())),
                TableColumn::fixed("proto", 60.0, |r: &ElephantRecord| {
                    text(r.proto.clone()).size(font::CAPTION).into()
                }),
                TableColumn::fixed("bytes", 110.0, |r: &ElephantRecord| {
                    text(format_bytes(r.bytes as f64)).size(font::CAPTION).into()
                })
                .sortable(|r: &ElephantRecord| SortKey::Num(r.bytes as f64)),
                TableColumn::fixed("packets", 90.0, |r: &ElephantRecord| {
                    text(format_count(r.packets)).size(font::CAPTION).into()
                })
                .sortable(|r: &ElephantRecord| SortKey::Num(r.packets as f64)),
                TableColumn::fixed("dur_ms", 80.0, |r: &ElephantRecord| {
                    text(r.duration_ms.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &ElephantRecord| SortKey::Num(r.duration_ms as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &ElephantRecord| format!("{} {} {}", r.src, r.dst, r.proto))
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Elephants, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Elephants, q))
                    .on_more(Message::NetringTableMore(NetringTable::Elephants))
                    .noun("flows")
                    .view(records, state.netring_detail.table(NetringTable::Elephants)),
            );
        }
    }
    col.into()
}

/// Per-L4 (tcp/udp/icmp) flow + byte split (#45).
fn render_per_l4(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut col = column![
        section_header("Per-protocol (L4)", None),
        row![cell("proto", 120), cell("flows", 120), cell("bytes", 140)].spacing(8),
    ]
    .spacing(4);
    for proto in ["tcp", "udp", "icmp"] {
        let flows = metric_f64(state, &format!("flow/by_l4/{proto}/flows_total"))
            .map(|v| format_count(v as u64))
            .unwrap_or_else(|| "-".into());
        let bytes = metric_f64(state, &format!("flow/by_l4/{proto}/bytes_total"))
            .map(format_bytes)
            .unwrap_or_else(|| "-".into());
        col = col.push(row![cell(proto, 120), cell(&flows, 120), cell(&bytes, 140)].spacing(8));
    }
    col.into()
}

/// On-demand recent-flow detail: a fetch button + the fetched flow table (P2 —
/// pulled from the sensor's `@/query/flows` channel, never streamed).
fn render_flow_detail(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.flows.is_loading();
    let title = section_header("Recent Flows (on demand)", None);

    // The button is disabled (no on_press) while a fetch is in flight.
    let label = if loading {
        "Fetching…"
    } else {
        "Fetch Flows"
    };
    let mut fetch = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringFlows);
    }
    let mut col = column![title, fetch].spacing(space::SM);

    if let Some(err) = state.netring_detail.flows.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(flows) = state.netring_detail.flows.ready() {
        if flows.is_empty() {
            col = col.push(empty_state("No recent flows", None));
        } else {
            col = col.push(flows_table(flows, state));
        }
    }
    col.into()
}

/// The Recent-Flows table, rendered through the shared [`DataTable`] (#244) —
/// sortable/filterable columns, responsive widths, and an explicit "N of M"
/// footer instead of a silent `.take(200)`.
fn flows_table<'a>(
    flows: &'a [zensight_common::FlowRecord],
    state: &'a DeviceDetailState,
) -> Element<'a, Message> {
    let columns = vec![
        TableColumn::fill("initiator", 3, |f: &zensight_common::FlowRecord| {
            text(f.src.clone()).size(font::CAPTION).into()
        })
        .sortable(|f: &zensight_common::FlowRecord| SortKey::Text(f.src.clone())),
        // Directedness glyph: authoritative initiator→responder (TCP, SYN-resolved)
        // renders "→"; UDP / handshake-less flows are undirected ("↔").
        TableColumn::fixed("dir", 26.0, |f: &zensight_common::FlowRecord| {
            text(dir_glyph(f.directed))
                .size(font::CAPTION)
                .style(if f.directed { dim } else { warn })
                .into()
        }),
        TableColumn::fill("responder", 3, |f: &zensight_common::FlowRecord| {
            text(f.dst.clone()).size(font::CAPTION).into()
        })
        .sortable(|f: &zensight_common::FlowRecord| SortKey::Text(f.dst.clone())),
        TableColumn::fixed("proto", 55.0, |f: &zensight_common::FlowRecord| {
            text(f.proto.clone()).size(font::CAPTION).into()
        })
        .sortable(|f: &zensight_common::FlowRecord| SortKey::Text(f.proto.clone())),
        TableColumn::fixed("bytes", 85.0, |f: &zensight_common::FlowRecord| {
            text(format_bytes(f.bytes as f64))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|f: &zensight_common::FlowRecord| SortKey::Num(f.bytes as f64)),
        TableColumn::fixed(
            "out↑ / in↓",
            150.0,
            |f: &zensight_common::FlowRecord| {
                text(dir_split(f.bytes_initiator, f.bytes_responder))
                    .size(font::CAPTION)
                    .into()
            },
        ),
        TableColumn::fixed("dur_ms", 70.0, |f: &zensight_common::FlowRecord| {
            text(f.duration_ms.to_string()).size(font::CAPTION).into()
        })
        .sortable(|f: &zensight_common::FlowRecord| SortKey::Num(f.duration_ms as f64)),
        TableColumn::fixed("reason", 80.0, |f: &zensight_common::FlowRecord| {
            text(f.reason.clone()).size(font::CAPTION).into()
        }),
    ];
    DataTable::new(columns)
        .searchable(|f: &zensight_common::FlowRecord| format!("{} {} {}", f.src, f.dst, f.proto))
        .on_sort(|col| Message::NetringTableSort(NetringTable::Flows, col))
        .on_filter(|q| Message::NetringTableFilter(NetringTable::Flows, q))
        .on_more(Message::NetringTableMore(NetringTable::Flows))
        .noun("flows")
        .view(flows, state.netring_detail.table(NetringTable::Flows))
}

/// Number of apps shown in the Bandwidth tab before the "N of M" footer.
const BANDWIDTH_TOP_N: usize = 20;

/// Rows in a ranked-bar chart (talkers, bandwidth) before it truncates.
const RANKED_BAR_ROWS: usize = 15;

/// Max rows/cols of the traffic-matrix heatmap (keeps the canvas bounded).
const MATRIX_HEATMAP_DIM: usize = 24;

/// Bandwidth-by-app tab (#251): a ranked bar chart of current per-app throughput
/// plus a table (app → flows pivot · throughput · trend sparkline) with a top-N
/// "N of M" footer. Distinct from the per-destination talker histogram.
fn render_bandwidth(state: &DeviceDetailState) -> Element<'_, Message> {
    // Collect `bandwidth/<app>/bytes_per_sec` and sort by value desc.
    let mut rows: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(metric, point)| {
            let app = metric
                .strip_prefix("bandwidth/")?
                .strip_suffix("/bytes_per_sec")?;
            Some((app.to_string(), value_f64(&point.value)))
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    let title = section_header(format!("Per-app bandwidth ({})", rows.len()), None);
    if rows.is_empty() {
        return column![title, empty_state("No bandwidth data", None)]
            .spacing(space::SM)
            .into();
    }

    // Ranked bar chart of current throughput (top-N).
    let bars = chart::ranked_bar(&rows, format_rate, BANDWIDTH_TOP_N);

    // Per-app table: clickable app (→ flows pivot), throughput, trend sparkline.
    let total = rows.len();
    let shown = total.min(BANDWIDTH_TOP_N);
    let mut list = Column::new().spacing(4).push(
        row![
            cell("application", 200),
            cell("throughput", 140),
            cell("trend", 80)
        ]
        .spacing(8),
    );
    for (app, bps) in rows.iter().take(BANDWIDTH_TOP_N) {
        let metric = format!("bandwidth/{app}/bytes_per_sec");
        list = list.push(
            row![
                pivot_cell(state, app, 200),
                cell(&format_rate(*bps), 140),
                super::metric_sparkline(state, &metric),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }
    let footer = text(format!("showing {shown} of {total} apps"))
        .size(font::CAPTION)
        .style(dim);
    column![title, bars, list, footer].spacing(space::SM).into()
}

/// Rank an alert severity for ordering / "highest severity" rollups.
fn sev_rank(s: zensight_common::AlertSeverity) -> u8 {
    use zensight_common::AlertSeverity::*;
    match s {
        Critical => 3,
        Warning => 2,
        Info => 1,
    }
}

/// Human label for a severity.
fn sev_label(s: zensight_common::AlertSeverity) -> &'static str {
    use zensight_common::AlertSeverity::*;
    match s {
        Critical => "critical",
        Warning => "warning",
        Info => "info",
    }
}

/// Design-system color for a severity (D2 severity palette).
fn sev_color(s: zensight_common::AlertSeverity) -> iced::Color {
    use zensight_common::AlertSeverity::*;
    match s {
        Critical => theme::SEVERITY_CRITICAL,
        Warning => theme::SEVERITY_WARNING,
        Info => theme::SEVERITY_INFO,
    }
}

/// The ATT&CK technique tagged on an anomaly, if any (#117).
fn anomaly_technique(a: &zensight_common::Alert) -> Option<&str> {
    a.labels.get("technique").map(String::as_str)
}

/// Overview anomaly strip (#253): a one-line rollup of firing netring detectors
/// that click-throughs to the Security tab. `None` when there are no anomalies.
fn anomaly_strip(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    let anoms = &state.netring_detail.anomalies;
    if anoms.is_empty() {
        return None;
    }
    let highest = anoms
        .iter()
        .map(|a| a.severity)
        .max_by_key(|s| sev_rank(*s))?;
    let tech = anoms.iter().find_map(anomaly_technique).unwrap_or("");
    let n = anoms.len();
    let plural = if n == 1 { "y" } else { "ies" };
    let label = if tech.is_empty() {
        format!("⚠ {n} anomal{plural} · highest {}", sev_label(highest))
    } else {
        format!(
            "⚠ {n} anomal{plural} · highest {} · {tech}",
            sev_label(highest)
        )
    };
    Some(
        button(
            text(label)
                .size(font::CAPTION)
                .style(move |_: &Theme| text::Style {
                    color: Some(sev_color(highest)),
                }),
        )
        .padding([space::XS as u16, space::SM as u16])
        .style(iced::widget::button::text)
        .on_press(Message::SelectSpecializedTab(
            state.device_id.clone(),
            SpecializedTab::Security,
        ))
        .into(),
    )
}

/// Security tab (#253): an in-view rollup of this sensor's firing anomalies by
/// detector, scoped to this source, that deep-links to the global Security view
/// and pivots each anomaly to its offending flows. Deliberately compact — it
/// does not duplicate the full Security view.
fn render_netring_security(state: &DeviceDetailState) -> Element<'_, Message> {
    let anoms = &state.netring_detail.anomalies;
    let open = button(text("Open Security view").size(font::CAPTION))
        .padding([4, 10])
        .on_press(Message::OpenSecurity);
    let mut col = column![section_header(
        format!("Anomalies ({})", anoms.len()),
        Some(open.into())
    )]
    .spacing(space::SM);

    if anoms.is_empty() {
        return col
            .push(empty_state("No anomalies for this sensor", None))
            .into();
    }

    // Rollup by detector (rule): count + highest severity.
    let mut by_rule: std::collections::BTreeMap<String, (usize, zensight_common::AlertSeverity)> =
        Default::default();
    for a in anoms {
        let e = by_rule
            .entry(a.rule.clone())
            .or_insert((0, zensight_common::AlertSeverity::Info));
        e.0 += 1;
        if sev_rank(a.severity) > sev_rank(e.1) {
            e.1 = a.severity;
        }
    }
    col = col.push(text("by detector").size(font::CAPTION).style(dim));
    for (rule, (count, sev)) in &by_rule {
        col = col.push(
            row![
                badge(sev_color(*sev), rule.clone()),
                text(format!("×{count}")).size(font::CAPTION).style(dim),
            ]
            .spacing(space::SM)
            .align_y(iced::Alignment::Center),
        );
    }

    // Individual anomalies (severity desc), each pivoting to its flows.
    let mut sorted: Vec<&zensight_common::Alert> = anoms.iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(sev_rank(a.severity)));
    col = col.push(text("detections").size(font::CAPTION).style(dim));
    for a in sorted {
        let mut r = row![
            badge(sev_color(a.severity), sev_label(a.severity)),
            text(a.summary.clone()).size(font::CAPTION),
        ]
        .spacing(space::SM)
        .align_y(iced::Alignment::Center);
        if let Some(src) = a.labels.get("src") {
            r = r.push(pivot_button(state, src, "flows →"));
        }
        col = col.push(r);
    }
    col.into()
}

/// Flow-direction glyph: a directed initiator→responder arrow when orientation
/// is authoritative (TCP), an undirected `↔` otherwise (UDP / handshake-less).
fn dir_glyph(directed: bool) -> &'static str {
    if directed { "→" } else { "↔" }
}

/// Compact per-direction byte split for a flow: `out↑` = initiator→responder
/// (request), `in↓` = the reply. `-` when neither side has a count (old records).
fn dir_split(bytes_initiator: u64, bytes_responder: u64) -> String {
    if bytes_initiator == 0 && bytes_responder == 0 {
        "-".to_string()
    } else {
        format!(
            "{} ↑ / {} ↓",
            format_bytes(bytes_initiator as f64),
            format_bytes(bytes_responder as f64)
        )
    }
}

fn cell<'a>(s: &str, width: u16) -> Element<'a, Message> {
    text(s.to_string())
        .size(12)
        .width(Length::Fixed(width as f32))
        .into()
}

/// A fixed-width table cell whose endpoint text is a **drill-down pivot** (#246):
/// clicking it jumps to the Flows tab filtered to `endpoint`. The shared
/// affordance reused by talkers / matrix / assets rows ("every label is a link").
fn pivot_cell<'a>(state: &DeviceDetailState, endpoint: &str, width: u16) -> Element<'a, Message> {
    container(pivot_button(state, endpoint, endpoint))
        .width(Length::Fixed(width as f32))
        .into()
}

/// The width-less drill-down affordance for use inside a [`DataTable`] cell
/// (the table owns the column width). Clicking pivots to the Flows tab filtered
/// to `endpoint` (#246).
fn pivot_button<'a>(
    state: &DeviceDetailState,
    endpoint: &str,
    label: &str,
) -> Element<'a, Message> {
    button(text(label.to_string()).size(font::CAPTION))
        .padding(0)
        .style(iced::widget::button::text)
        .on_press(Message::NetringPivotToFlows(
            state.device_id.clone(),
            endpoint.to_string(),
        ))
        .into()
}

/// A fixed-width table cell whose text is tinted by `style` (e.g. drop-rate).
fn cell_styled<'a>(s: &str, width: u16, style: fn(&Theme) -> text::Style) -> Element<'a, Message> {
    text(s.to_string())
        .size(12)
        .width(Length::Fixed(width as f32))
        .style(style)
        .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

fn danger(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).danger()),
    }
}

fn warn(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).warning()),
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

/// Numeric projection of a telemetry value for charts (`0.0` for non-numerics).
fn value_f64(v: &TelemetryValue) -> f64 {
    match v {
        TelemetryValue::Counter(c) => *c as f64,
        TelemetryValue::Gauge(g) => *g,
        TelemetryValue::Boolean(true) => 1.0,
        TelemetryValue::Boolean(false) => 0.0,
        _ => 0.0,
    }
}

/// A compact RED/KPI tile: a big value over a muted caption, in a card. Used by
/// the DNS/HTTP RED headers (#250).
fn metric_tile<'a>(label: &str, value: String) -> Element<'a, Message> {
    card(
        column![
            text(value).size(font::SECTION),
            text(label.to_string()).size(font::CAPTION).style(dim),
        ]
        .spacing(space::XS),
    )
}

#[cfg(test)]
mod tests {
    use iced_test::simulator;
    use zensight_common::{Protocol, TalkerRecord};

    use super::*;
    use crate::message::DeviceId;
    use crate::view::specialized::fetch::Fetch;

    #[test]
    fn talker_destination_pivots_to_flows() {
        let mut state = DeviceDetailState::new(DeviceId::new(Protocol::Netring, "host01"));
        state.netring_detail.talkers = Fetch::Ready(vec![TalkerRecord {
            dst: "10.0.0.42:443".to_string(),
            bytes: 1234,
            packets: 10,
            flows: 3,
        }]);
        let mut ui = simulator(render_talkers(&state));
        let _ = ui.click("10.0.0.42:443");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::NetringPivotToFlows(d, ep)
                if d.source == "host01" && ep == "10.0.0.42:443"
        )));
    }
}
