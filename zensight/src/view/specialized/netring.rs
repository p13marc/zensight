//! Netring sensor specialized view — flows + per-app bandwidth.

use iced::Element;
use iced::widget::{Column, button, column, container, row, scrollable, text};
use iced::{Length, Theme};
use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::components::{card, empty_state, section_header};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::fetch::Fetch;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the netring sensor specialized view.
pub fn netring_sensor_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut content = column![
        render_header(state),
        card(render_flows(state)),
        card(render_tcp_health(state)),
        card(render_bandwidth(state)),
        card(render_tls(state)),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    // L7 RED + per-protocol breakdowns — only when the sensor publishes them (#45).
    if has_prefix(state, "dns/") {
        content = content.push(card(render_dns(state)));
    }
    if has_prefix(state, "http/") {
        content = content.push(card(render_http(state)));
    }
    if has_prefix(state, "flow/by_l4/") {
        content = content.push(card(render_per_l4(state)));
    }
    // L7 QUIC SNI/ALPN + SSH/HASSH inventories (#72) — shown when the sensor
    // publishes their aggregate count or after a fetch has been attempted.
    if has_prefix(state, "quic/") || !matches!(state.netring_detail.quic, Fetch::Idle) {
        content = content.push(card(render_quic(state)));
    }
    if has_prefix(state, "ssh/") || !matches!(state.netring_detail.ssh, Fetch::Idle) {
        content = content.push(card(render_ssh(state)));
    }

    // Passive asset inventory (#70) — shown when the sensor publishes a
    // discovered-count or after a fetch has been attempted.
    if has_prefix(state, "assets/") || !matches!(state.netring_detail.assets, Fetch::Idle) {
        content = content.push(card(render_assets(state)));
    }

    // Capture self-health only exists under live capture (not pcap replay).
    if state.metrics.keys().any(|k| k.starts_with("capture/")) {
        content = content.push(card(render_capture(state)));
    }
    content = content.push(card(render_flow_detail(state)));

    container(scrollable(content))
        .width(Length::Fill)
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
                    cell("sni", 240),
                    cell("ja4", 240),
                    cell("alpn", 90),
                    cell("count", 60)
                ]
                .spacing(8),
            );
            for r in records.iter().take(200) {
                list = list.push(
                    row![
                        cell(r.sni.as_deref().unwrap_or("-"), 240),
                        cell(r.ja4.as_deref().unwrap_or("-"), 240),
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
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("mac", 150),
                    cell("ip", 150),
                    cell("hostname", 150),
                    cell("platform", 160),
                    cell("caps", 130),
                    cell("seen via", 110),
                ]
                .spacing(8),
            );
            for r in records.iter().take(200) {
                let ip = r
                    .ipv4
                    .first()
                    .or_else(|| r.ipv6.first())
                    .map(String::as_str)
                    .unwrap_or("-");
                list = list.push(
                    row![
                        cell(&r.mac, 150),
                        cell(ip, 150),
                        cell(r.hostname.as_deref().unwrap_or("-"), 150),
                        cell(r.platform.as_deref().unwrap_or("-"), 160),
                        cell(&join_or_dash(&r.capabilities), 130),
                        cell(&join_or_dash(&r.seen_via), 110),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("{} assets", records.len())).size(font::EMPHASIS))
                .push(list);
        }
    }
    col.into()
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
        {
            sources
                .entry(src.to_string())
                .or_default()
                .insert(stat.to_string(), &point.value);
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
        let g = |s: &str| num(stats.get(s).copied());
        let drop_rate = match stats.get("drop_rate") {
            Some(TelemetryValue::Gauge(r)) => format!("{:.2}%", r * 100.0),
            _ => "-".into(),
        };
        list = list.push(
            row![
                cell(src, 90),
                cell(&g("packets"), 120),
                cell(&g("drops"), 100),
                cell(&drop_rate, 100),
                cell(&g("freezes"), 90),
            ]
            .spacing(8),
        );
        // AF_XDP per-cause breakdown (only present on XDP sources).
        let xdp: Vec<(String, String)> = stats
            .iter()
            .filter_map(|(stat, v)| {
                let cause = stat.strip_prefix("xdp/")?;
                Some((cause.to_string(), num(Some(*v))))
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
            cell(&get("flow/bytes_total"), 100)
        ]
        .spacing(8),
        row![
            cell("packets (total)", 160),
            cell(&get("flow/packets_total"), 100)
        ]
        .spacing(8),
        row![
            cell("retransmits (total)", 160),
            cell(&get("flow/retransmits_total"), 100)
        ]
        .spacing(8),
    ]
    .spacing(4)
    .into()
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

/// DNS RED card (#45): rates, RTT percentiles, and rcode breakdown.
fn render_dns(state: &DeviceDetailState) -> Element<'_, Message> {
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 200), cell(&get(metric), 120)].spacing(8);

    let mut col = column![
        section_header("DNS (RED)", None),
        line("queries (total)", "dns/queries_total"),
        line("unanswered (total)", "dns/unanswered_total"),
        line("RTT p50 (ms)", "dns/query_rtt_p50_ms"),
        line("RTT p95 (ms)", "dns/query_rtt_p95_ms"),
        line("RTT p99 (ms)", "dns/query_rtt_p99_ms"),
    ]
    .spacing(4);

    // Response-code breakdown (dynamic `dns/responses_by_rcode/<rcode>_total`).
    let mut rcodes: Vec<(String, String)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            let r = m.strip_prefix("dns/responses_by_rcode/")?;
            Some((r.to_string(), num(Some(&p.value))))
        })
        .collect();
    rcodes.sort();
    if !rcodes.is_empty() {
        col = col.push(text("by rcode").size(font::CAPTION).style(dim));
        for (r, v) in rcodes {
            col = col.push(row![cell(&format!("  {r}"), 200), cell(&v, 120)].spacing(8));
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
    col.into()
}

/// Per-L4 (tcp/udp/icmp) flow + byte split (#45).
fn render_per_l4(state: &DeviceDetailState) -> Element<'_, Message> {
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let mut col = column![
        section_header("Per-protocol (L4)", None),
        row![cell("proto", 120), cell("flows", 120), cell("bytes", 140)].spacing(8),
    ]
    .spacing(4);
    for proto in ["tcp", "udp", "icmp"] {
        col = col.push(
            row![
                cell(proto, 120),
                cell(&get(&format!("flow/by_l4/{proto}/flows_total")), 120),
                cell(&get(&format!("flow/by_l4/{proto}/bytes_total")), 140),
            ]
            .spacing(8),
        );
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
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("src", 190),
                    cell("dst", 190),
                    cell("proto", 60),
                    cell("bytes", 90),
                    cell("dur_ms", 80),
                    cell("reason", 90),
                ]
                .spacing(8),
            );
            for f in flows.iter().take(200) {
                list = list.push(
                    row![
                        cell(&f.src, 190),
                        cell(&f.dst, 190),
                        cell(&f.proto, 60),
                        cell(&f.bytes.to_string(), 90),
                        cell(&f.duration_ms.to_string(), 80),
                        cell(&f.reason, 90),
                    ]
                    .spacing(8),
                );
            }
            col = col
                .push(text(format!("{} flows", flows.len())).size(font::EMPHASIS))
                .push(list);
        }
    }
    col.into()
}

fn render_bandwidth(state: &DeviceDetailState) -> Element<'_, Message> {
    // Collect `bandwidth/<app>/bytes_per_sec` and sort by value desc.
    let mut rows: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(metric, point)| {
            let app = metric
                .strip_prefix("bandwidth/")?
                .strip_suffix("/bytes_per_sec")?;
            let bps = match &point.value {
                TelemetryValue::Gauge(g) => *g,
                TelemetryValue::Counter(c) => *c as f64,
                _ => return None,
            };
            Some((app.to_string(), bps))
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    let title = section_header(format!("Top Talkers ({})", rows.len()), None);
    if rows.is_empty() {
        return column![title, empty_state("No bandwidth data", None)]
            .spacing(space::SM)
            .into();
    }

    let mut list = Column::new().spacing(4).push(
        row![
            cell("application", 200),
            cell("bytes/sec", 140),
            cell("trend", 80)
        ]
        .spacing(8),
    );
    for (app, bps) in rows.iter().take(30) {
        // Per-talker bytes/sec trend sparkline (#44).
        let metric = format!("bandwidth/{app}/bytes_per_sec");
        list = list.push(
            row![
                cell(app, 200),
                cell(&format!("{bps:.0}"), 140),
                super::metric_sparkline(state, &metric),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }
    column![title, list].spacing(8).into()
}

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

fn danger(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).danger()),
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
