//! Netring sensor specialized view — a tabbed, chart-driven, drill-down surface
//! (Overview · Flows · Talkers & Matrix · DNS · HTTP/TLS · Bandwidth · Assets ·
//! Security · Capture) over the sensor's streamed metrics and `@rpc/netring/*`
//! procedures (#247, epic #257).

use iced::Element;
use iced::widget::{Column, button, column, container, row, scrollable, text};
use iced::{Length, Theme};
use zensight_common::TelemetryValue;

use zensight_common::registry::netring::Subject;

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
use crate::view::subject::{leaf, var};
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the netring sensor specialized view: a header + the tabbed container
/// over the active tab's content (#247). `artifact` threads the app's shared
/// artifact state in so the Capture tab can host the on-demand capture form
/// in-context (#351); `None` renders health-only.
pub fn netring_sensor_view<'a>(
    state: &'a DeviceDetailState,
    artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'a>>,
) -> Element<'a, Message> {
    let tabs = netring_tabs(state, capture_advertised(artifact));
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
    let content = netring_tab_content(state, active, artifact);
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
fn netring_tabs(
    state: &DeviceDetailState,
    capture_advertised: bool,
) -> Vec<TabItem<SpecializedTab>> {
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
            .visible(state.metrics.keys().any(|k| k.starts_with("capture/")) || capture_advertised),
    ]
}

/// Build the scrollable content for a netring tab by composing the existing
/// per-section cards. No data regression: every card in the old single-scroll
/// view is reachable from exactly one tab.
fn netring_tab_content<'a>(
    state: &'a DeviceDetailState,
    tab: SpecializedTab,
    artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'a>>,
) -> Element<'a, Message> {
    use SpecializedTab::*;
    let inner: Column<'_, Message> = match tab {
        Overview => {
            let mut c = column![].spacing(space::MD);
            // Live anomaly strip: a compact rollup of firing detectors that
            // click-throughs to the Security tab (#253).
            if let Some(strip) = anomaly_strip(state) {
                c = c.push(strip);
            }
            if let Some(chip) = capture_chip(state) {
                c = c.push(chip);
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
            c = c.push(card(render_ja4h(state)));
            c
        }
        Bandwidth => column![card(render_bandwidth(state))].spacing(space::MD),
        Assets => column![card(render_assets(state))].spacing(space::MD),
        Capture => {
            let mut c = column![card(render_capture(state, artifact))].spacing(space::MD);
            if let Some(disk) = render_capture_to_disk(state) {
                c = c.push(card(disk));
            }
            c
        }
        Security => column![card(render_netring_security(state))].spacing(space::MD),
        // netlink-only tabs never reach a netring view (falls back to Overview).
        _ => column![].spacing(space::MD),
    };
    scrollable(inner.width(Length::Fill))
        .height(Length::Fill)
        .into()
}

/// TLS section: streamed handshake aggregates + an on-demand fingerprint
/// inventory (SNI / JA4) fetched from `@rpc/netring/tls`.
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
        // Post-quantum readiness (#326): share of handshakes offering a PQ hybrid.
        row![
            cell("PQ readiness (ratio)", 180),
            cell(&get("tls/pq_ratio"), 100)
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
            use zensight_common::TlsRecord;
            let columns = vec![
                TableColumn::fill("sni", 4, |r: &TlsRecord| {
                    text(r.sni.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &TlsRecord| SortKey::Text(r.sni.clone().unwrap_or_default())),
                TableColumn::fill("ja4", 4, |r: &TlsRecord| {
                    text(r.ja4.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                // JA3 was fetched but never rendered before (#45/#249).
                TableColumn::fill("ja3", 4, |r: &TlsRecord| {
                    text(r.ja3.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                TableColumn::fixed("alpn", 90.0, |r: &TlsRecord| {
                    text(r.alpn.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                // App protocol classified from ALPN + SNI + port (#326).
                TableColumn::fixed("app", 70.0, |r: &TlsRecord| {
                    text(r.app_protocol.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                // Post-quantum key-share offered (#326) — the per-fingerprint
                // PQ-readiness flag behind the aggregate `tls/pq_ratio`.
                TableColumn::fixed("pq", 50.0, |r: &TlsRecord| {
                    text(if r.pq_key_share { "PQ" } else { "-" })
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &TlsRecord| SortKey::Num(r.pq_key_share as u8 as f64)),
                TableColumn::fixed("count", 60.0, |r: &TlsRecord| {
                    text(r.count.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &TlsRecord| SortKey::Num(r.count as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &TlsRecord| {
                        format!(
                            "{} {} {}",
                            r.sni.clone().unwrap_or_default(),
                            r.ja4.clone().unwrap_or_default(),
                            r.ja3.clone().unwrap_or_default(),
                        )
                    })
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Tls, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Tls, q))
                    .on_more(Message::NetringTableMore(NetringTable::Tls))
                    .noun("fingerprints")
                    .view(records, state.netring_detail.table(NetringTable::Tls)),
            );
        }
    }
    col.into()
}

/// Drop-rate fraction at/above which the capture-health card flags an overload
/// badge — mirrors the netring `OverloadDetector` default enter threshold (5%),
/// so the GUI's local signal lines up with the `capture-overload` alert (#71).
const OVERLOAD_DROP_RATE: f64 = 0.05;

/// QUIC section (#72): streamed distinct-SNI count + an on-demand SNI/ALPN/version
/// inventory fetched from `@rpc/netring/quic` — the QUIC analogue of the TLS card.
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
            use zensight_common::QuicRecord;
            let columns = vec![
                TableColumn::fill("sni", 5, |r: &QuicRecord| {
                    text(r.sni.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &QuicRecord| SortKey::Text(r.sni.clone().unwrap_or_default())),
                TableColumn::fill("alpn", 3, |r: &QuicRecord| {
                    text(join_or_dash(&r.alpn)).size(font::CAPTION).into()
                }),
                TableColumn::fixed("version", 90.0, |r: &QuicRecord| {
                    text(r.version.clone()).size(font::CAPTION).into()
                }),
                TableColumn::fixed("count", 60.0, |r: &QuicRecord| {
                    text(r.count.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &QuicRecord| SortKey::Num(r.count as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &QuicRecord| r.sni.clone().unwrap_or_default())
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Quic, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Quic, q))
                    .on_more(Message::NetringTableMore(NetringTable::Quic))
                    .noun("SNI/version pairs")
                    .view(records, state.netring_detail.table(NetringTable::Quic)),
            );
        }
    }
    col.into()
}

/// JA4H section (#256): on-demand HTTP-client fingerprint inventory fetched
/// from `@rpc/netring/ja4h`. Served only by `ja4plus` sensor builds, so there is no
/// streamed metric to gate on — the section always shows its fetch button and
/// the error path names the build flag.
fn render_ja4h(state: &DeviceDetailState) -> Element<'_, Message> {
    let loading = state.netring_detail.ja4h.is_loading();
    let label = if loading { "Fetching…" } else { "Fetch JA4H" };
    let mut fetch = button(text(label).size(font::CAPTION)).padding([4, 10]);
    if !loading {
        fetch = fetch.on_press(Message::FetchNetringJa4h);
    }

    let mut col =
        column![section_header("HTTP clients (JA4H)", Some(fetch.into()))].spacing(space::SM);

    if let Some(err) = state.netring_detail.ja4h.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(records) = state.netring_detail.ja4h.ready() {
        if records.is_empty() {
            col = col.push(empty_state("No HTTP client fingerprints observed", None));
        } else {
            use zensight_common::Ja4hRecord;
            let columns = vec![
                TableColumn::fill("ja4h", 4, |r: &Ja4hRecord| {
                    text(r.ja4h.clone()).size(font::CAPTION).into()
                })
                .sortable(|r: &Ja4hRecord| SortKey::Text(r.ja4h.clone())),
                TableColumn::fixed("method", 70.0, |r: &Ja4hRecord| {
                    text(r.method.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                TableColumn::fill("host", 3, |r: &Ja4hRecord| {
                    text(r.host.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &Ja4hRecord| SortKey::Text(r.host.clone().unwrap_or_default())),
                TableColumn::fill("user-agent", 4, |r: &Ja4hRecord| {
                    text(r.user_agent.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                TableColumn::fixed("count", 60.0, |r: &Ja4hRecord| {
                    text(r.count.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &Ja4hRecord| SortKey::Num(r.count as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &Ja4hRecord| {
                        format!(
                            "{} {} {}",
                            r.ja4h,
                            r.host.clone().unwrap_or_default(),
                            r.user_agent.clone().unwrap_or_default(),
                        )
                    })
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Ja4h, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Ja4h, q))
                    .on_more(Message::NetringTableMore(NetringTable::Ja4h))
                    .noun("fingerprints")
                    .view(records, state.netring_detail.table(NetringTable::Ja4h)),
            );
        }
    }
    col.into()
}

/// SSH section (#72): streamed distinct-HASSH count + an on-demand HASSH
/// inventory (fingerprint · role · banner) fetched from `@rpc/netring/ssh`.
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
            use zensight_common::SshRecord;
            let columns = vec![
                TableColumn::fill("hassh", 5, |r: &SshRecord| {
                    text(r.hassh.clone()).size(font::CAPTION).into()
                })
                .sortable(|r: &SshRecord| SortKey::Text(r.hassh.clone())),
                TableColumn::fixed("role", 70.0, |r: &SshRecord| {
                    text(r.role.clone()).size(font::CAPTION).into()
                }),
                TableColumn::fill("banner", 4, |r: &SshRecord| {
                    text(r.banner.clone().unwrap_or_else(|| "-".into()))
                        .size(font::CAPTION)
                        .into()
                }),
                TableColumn::fixed("count", 60.0, |r: &SshRecord| {
                    text(r.count.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &SshRecord| SortKey::Num(r.count as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &SshRecord| {
                        format!("{} {}", r.hassh, r.banner.clone().unwrap_or_default())
                    })
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Ssh, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Ssh, q))
                    .on_more(Message::NetringTableMore(NetringTable::Ssh))
                    .noun("fingerprints")
                    .view(records, state.netring_detail.table(NetringTable::Ssh)),
            );
        }
    }
    col.into()
}

/// Passive asset-inventory section (#70): a streamed discovered-count plus an
/// on-demand table (MAC · IP · hostname · platform · capabilities · seen-via)
/// fetched from `@rpc/netring/assets`. Surfaces hosts seen on the wire that emit no
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
        // Asset → topology node pivot (#252); resolution (hostname, then the
        // ip→node map) happens in the handler, which toasts when unmapped.
        TableColumn::fill("topology", 2, |r: &AssetRecord| {
            let ip = asset_ip(r);
            if ip == "-" {
                text("-").size(font::CAPTION).into()
            } else {
                button(text("map").size(font::CAPTION))
                    .padding([2, 8])
                    .on_press(Message::NetringAssetToTopology {
                        ip: ip.to_string(),
                        hostname: r.hostname.clone(),
                    })
                    .into()
            }
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
/// The netring key prefix used for artifact lookups (matches `sensors.rs`'s
/// `zensight/<sensor>` rule).
fn netring_producer() -> String {
    zensight_common::Protocol::Netring.as_str().to_string()
}

/// Whether the app-side artifact state says this sensor advertises on-demand
/// capture (#351) — gates both the Capture tab and the in-context form.
fn capture_advertised(artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'_>>) -> bool {
    let Some(ctx) = artifact else { return false };
    ctx.kinds.get(&netring_producer()).is_some_and(|kinds| {
        kinds
            .iter()
            .any(|k| matches!(k.advert, zensight_common::KindAdvert::Capture { .. }))
    })
}

fn render_capture<'a>(
    state: &'a DeviceDetailState,
    artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'a>>,
) -> Element<'a, Message> {
    // Group capture/<src>/<stat>; `stat` may itself be `xdp/<cause>`.
    let mut sources: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, &TelemetryValue>,
    > = Default::default();
    for (metric, point) in &state.metrics {
        // `capture/{source}/…` — the per-NIC-leg family. `capture/focus/*` (the
        // reloadable-filter counter) and `capture/disk/*` (the capture-to-disk
        // engine, #327) are *different registered subjects*, so literal-beats-var
        // precedence keeps them out of this table on its own — the hand-coded
        // `src != "focus" && src != "disk"` exclusion list is gone (#475).
        if let Some(s) = Subject::parse_metric(metric)
            && let Some(src) = var(&s.vars(), "source")
        {
            let stat = s
                .pattern()
                .strip_prefix("capture/{source}/")
                .unwrap_or_else(|| leaf(s.pattern()));
            sources
                .entry(src)
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

    // In-context on-demand capture (#351): render the shared capture form
    // right here when the sensor advertises the Capture kind — same state as
    // the Sensors-page card (mirror, not move), so edits track across both.
    // `artifact_section` with the kinds filtered to Capture also carries the
    // in-flight job controls (pause/resume/cancel) and the finished status.
    let prefix = netring_producer();
    let capture_kinds: Vec<zensight_common::KindStatus> = artifact
        .and_then(|ctx| ctx.kinds.get(&prefix))
        .map(|kinds| {
            kinds
                .iter()
                .filter(|k| matches!(k.advert, zensight_common::KindAdvert::Capture { .. }))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    match artifact {
        Some(ctx) if !capture_kinds.is_empty() => {
            // No target_source: this aggregated view spans every netring host,
            // so a capture request here keeps the historical fan-out (the
            // per-instance Sensors cards target a single host).
            col = col.push(crate::view::artifact_fetch::artifact_section(
                ctx.fetch,
                &prefix,
                None,
                &capture_kinds,
                ctx.active_prefix,
                ctx.active_kind,
                ctx.capture_forms.get(&prefix),
            ));
        }
        _ => {
            col = col.push(
                text(
                    "Live capture health. This sensor does not advertise on-demand captures \
                      (enable `artifacts.capture` in its config).",
                )
                .size(font::CAPTION)
                .style(dim),
            );
        }
    }

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

/// Capture-to-disk section (#327): the engine's live mode + pre-trigger ring
/// occupancy + retention usage (from the `capture/disk/*` telemetry), the
/// `capture_now` / mode hot-switch controls, and the capture-file index from
/// `@rpc/netring/captures` with a per-file download for served triggered captures.
/// `None` when the sensor never published `capture/disk/*` (engine unarmed).
fn render_capture_to_disk(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    let mode = match state.metrics.get("capture/disk/mode").map(|p| &p.value) {
        Some(TelemetryValue::Text(m)) => m.clone(),
        _ => return None,
    };

    let gauge = |m: &str| metric_f64(state, m).unwrap_or(0.0);
    let ring_packets = gauge("capture/disk/ring_packets") as u64;
    let ring_bytes = gauge("capture/disk/ring_bytes") as u64;
    let retained_files = gauge("capture/disk/retained_files") as u64;
    let retained_bytes = gauge("capture/disk/retained_bytes") as u64;
    let dropped = gauge("capture/disk/dropped") as u64;
    let evictions = gauge("capture/disk/evictions") as u64;
    let triggers = gauge("capture/disk/triggers") as u64;

    let mode_color = match mode.as_str() {
        "triggered" => theme::ACCENT_ANOMALY,
        "rotating" => theme::STATUS_ONLINE,
        _ => theme::STATUS_UNKNOWN,
    };
    let capture_now = button(text("Capture now").size(font::CAPTION))
        .padding([4, 10])
        .on_press(Message::NetringCaptureNow);
    let header = section_header(
        "Capture to disk",
        Some(
            row![badge(mode_color, mode.clone()), capture_now]
                .spacing(space::SM)
                .align_y(iced::Alignment::Center)
                .into(),
        ),
    );

    let mut col = column![header].spacing(space::SM);

    // Live engine counters: ring occupancy is the pre-trigger evidence window;
    // retained files/bytes show retention pressure; dropped counts engine-channel
    // overflow (honest loss, like the NIC-leg drops above).
    col = col.push(
        row![
            cell("ring", 60),
            cell(
                &format!(
                    "{} pkts · {}",
                    format_count(ring_packets),
                    format_bytes(ring_bytes as f64)
                ),
                200
            ),
            cell("retained", 70),
            cell(
                &format!(
                    "{retained_files} files · {}",
                    format_bytes(retained_bytes as f64)
                ),
                180
            ),
            cell("triggers", 70),
            cell(&format_count(triggers), 70),
        ]
        .spacing(8),
    );
    if dropped > 0 || evictions > 0 {
        col = col.push(
            row![
                cell_styled(&format!("dropped {}", format_count(dropped)), 140, warn),
                cell(&format!("evicted {} files", format_count(evictions)), 160),
            ]
            .spacing(8),
        );
    }
    // Last lifecycle event (trigger fired / capture ready / mode switch).
    if let Some(TelemetryValue::Text(ev)) = state.metrics.get("capture/events").map(|p| &p.value) {
        col = col.push(
            text(format!("last event: {ev}"))
                .size(font::CAPTION)
                .style(dim),
        );
    }

    // Mode hot-switch (live between the armed modes; off-at-startup needs a
    // restart, same rule as the detector registry).
    let mut modes = row![text("mode:").size(font::CAPTION).style(dim)]
        .spacing(space::SM)
        .align_y(iced::Alignment::Center);
    for m in ["off", "rotating", "triggered"] {
        let mut b = button(text(m).size(font::CAPTION)).padding([3, 9]);
        if m != mode {
            b = b.on_press(Message::NetringSetCaptureDiskMode(m.to_string()));
        }
        modes = modes.push(b);
    }
    col = col.push(modes);

    // Capture-file index (`@rpc/netring/captures`): triggered captures download
    // through the artifact blob path; rotating spool files are metadata-only.
    let captures = &state.netring_detail.captures;
    let loading = captures.is_loading();
    let mut refresh =
        button(text(if loading { "Fetching…" } else { "Refresh" }).size(font::CAPTION))
            .padding([4, 10]);
    if !loading {
        refresh = refresh.on_press(Message::FetchNetringCaptures);
    }
    col = col.push(
        row![text("Capture files").size(font::EMPHASIS), refresh,]
            .spacing(space::SM)
            .align_y(iced::Alignment::Center),
    );

    if let Some(err) = captures.error() {
        col = col.push(text(err.to_string()).size(font::CAPTION).style(dim));
    } else if let Some(records) = captures.ready() {
        if records.is_empty() {
            col = col.push(text("no capture files yet").size(font::CAPTION).style(dim));
        } else {
            let mut list = Column::new().spacing(3).push(
                row![
                    cell("file", 320),
                    cell("trigger", 130),
                    cell("packets", 80),
                    cell("size", 90),
                    cell("", 110),
                ]
                .spacing(8),
            );
            for rec in records.iter().take(50) {
                let trigger = rec.trigger_kind.clone().unwrap_or_else(|| rec.mode.clone());
                let size = format_bytes(rec.bytes as f64);
                // A download needs the id *and* the origin holding it: a bulk
                // fetch must name a literal origin (RFC 07 §3), and a sensor
                // too old to say which one is also too old to serve this wire
                // version at all — so there is nothing a wildcard would buy
                // here.
                let action: Element<'_, Message> = match (&rec.artifact_id, &rec.artifact_prefix) {
                    (Some(id), Some(prefix)) => button(text("Download").size(font::CAPTION))
                        .padding([3, 9])
                        .on_press(Message::DownloadCaptureBlob {
                            producer: "netring".to_string(),
                            artifact_id: id.clone(),
                            blob_prefix: prefix.clone(),
                            root: rec.artifact_root.clone(),
                            filename: rec.filename.clone(),
                        })
                        .into(),
                    (Some(_), None) => text("sensor too old").size(font::CAPTION).style(dim).into(),
                    (None, _) => text(if rec.mode == "rotating" {
                        "on sensor disk"
                    } else {
                        "expired"
                    })
                    .size(font::CAPTION)
                    .style(dim)
                    .into(),
                };
                let name = if rec.truncated {
                    format!("{} · truncated", rec.filename)
                } else {
                    rec.filename.clone()
                };
                list = list.push(
                    row![
                        cell(&name, 320),
                        cell(&trigger, 130),
                        cell(&format_count(rec.packets), 80),
                        cell(&size, 90),
                        action,
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );
            }
            col = col.push(list);
        }
    } else if !loading {
        col = col.push(
            text("select Refresh to list capture files")
                .size(font::CAPTION)
                .style(dim),
        );
    }

    Some(col.into())
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

    // Encrypted-DNS visibility (#326): DoT/DoQ/DoH session split + the un-known
    // resolver subset (the policy-bypass / tunneling signal). Only shown once the
    // sensor has classified an encrypted-DNS session, so the panel stays hidden
    // when `collect.encrypted_dns` is off.
    if state
        .metrics
        .keys()
        .any(|m| m.starts_with("dns/encrypted/"))
    {
        let enc_tiles = row![
            metric_tile("DoT", get("dns/encrypted/dot")),
            metric_tile("DoQ", get("dns/encrypted/doq")),
            metric_tile("DoH", get("dns/encrypted/doh")),
            metric_tile("unknown resolver", get("dns/encrypted/unknown_resolver")),
        ]
        .spacing(space::SM);
        col = col
            .push(section_header("Encrypted DNS", None))
            .push(enc_tiles);
    }

    // Response-code breakdown (`dns/responses_by_rcode/<rcode>_total`) as a
    // ranked bar chart instead of a text list.
    let mut rcodes: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| match Subject::parse_metric(m) {
            Some(Subject::DnsResponsesByRcode { rcode }) => Some((
                rcode.trim_end_matches("_total").to_string(),
                value_f64(&p.value),
            )),
            _ => None,
        })
        .collect();
    rcodes.sort_by(|a, b| b.1.total_cmp(&a.1));
    if !rcodes.is_empty() {
        col = col
            .push(text("by rcode").size(font::CAPTION).style(dim))
            .push(chart::ranked_bar(&rcodes, |v| format_count(v as u64), 8));
    }

    // On-demand top-SLD / top-NXDOMAIN drill-down via `@rpc/netring/dns`.
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
    col = col.push(encrypted_dns_section(state));
    col.into()
}

/// The destinations *behind* the encrypted-DNS counts above (#326,
/// `@rpc/netring/encrypted_dns`) — the same rollup/detail split as everywhere
/// else: the tiles are streamed because they are bounded, the inventory is pulled
/// because it is not.
///
/// The interesting column is `via_known_resolver`: encrypted DNS to Cloudflare or
/// Quad9 is a policy question, whereas encrypted DNS to somewhere unrecognised is
/// how a tunnel or an exfil channel looks from the wire. So an unknown resolver is
/// called out rather than left as a `false` in a cell.
fn encrypted_dns_section<'a>(state: &'a DeviceDetailState) -> Element<'a, Message> {
    use zensight_common::EncryptedDnsRecord;

    let mut col = column![section_header("Encrypted DNS destinations", None)].spacing(space::SM);

    if state.netring_detail.encrypted_dns.is_loading() {
        return col
            .push(empty_state("Fetching encrypted-DNS destinations…", None))
            .into();
    }
    if let Some(err) = state.netring_detail.encrypted_dns.error() {
        return col
            .push(empty_state(format!("Fetch failed: {err}"), None))
            .into();
    }
    let Some(records) = state.netring_detail.encrypted_dns.ready() else {
        return col
            .push(
                button(text("Fetch encrypted DNS").size(font::CAPTION))
                    .padding([4, 10])
                    .on_press(Message::FetchNetringEncryptedDns),
            )
            .into();
    };
    if records.is_empty() {
        return col
            .push(empty_state("No encrypted DNS observed on this host.", None))
            .into();
    }

    let rogue = records.iter().filter(|r| !r.via_known_resolver).count();
    if rogue > 0 {
        col = col.push(
            text(format!(
                "⚠ {rogue} destination(s) are not a recognised public resolver"
            ))
            .size(font::CAPTION)
            .style(warn),
        );
    }

    let columns = vec![
        TableColumn::fixed("transport", 90.0, |r: &EncryptedDnsRecord| {
            text(r.transport.to_uppercase()).size(font::CAPTION).into()
        })
        .sortable(|r: &EncryptedDnsRecord| SortKey::Text(r.transport.clone())),
        TableColumn::fill("resolver (SNI)", 4, |r: &EncryptedDnsRecord| {
            text(r.sni.clone().unwrap_or_else(|| "—".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &EncryptedDnsRecord| SortKey::Text(r.sni.clone().unwrap_or_default())),
        TableColumn::fixed("known", 90.0, |r: &EncryptedDnsRecord| {
            let t = text(if r.via_known_resolver { "yes" } else { "no" }).size(font::CAPTION);
            if r.via_known_resolver {
                t
            } else {
                t.style(warn)
            }
            .into()
        })
        .sortable(|r: &EncryptedDnsRecord| SortKey::Num(u8::from(r.via_known_resolver) as f64)),
        TableColumn::fixed("sessions", 100.0, |r: &EncryptedDnsRecord| {
            text(r.count.to_string()).size(font::CAPTION).into()
        })
        .sortable(|r: &EncryptedDnsRecord| SortKey::Num(r.count as f64)),
    ];
    col.push(
        DataTable::new(columns)
            .searchable(|r: &EncryptedDnsRecord| r.sni.clone().unwrap_or_default())
            .on_sort(|c| Message::NetringTableSort(NetringTable::EncryptedDns, c))
            .on_filter(|q| Message::NetringTableFilter(NetringTable::EncryptedDns, q))
            .on_more(Message::NetringTableMore(NetringTable::EncryptedDns))
            .noun("destinations")
            .view(
                records,
                state.netring_detail.table(NetringTable::EncryptedDns),
            ),
    )
    .into()
}

/// HTTP tab (#250-style): RED tiles + status-class & method bar charts + an
/// on-demand top-hosts table.
fn render_http(state: &DeviceDetailState) -> Element<'_, Message> {
    let getf = |m: &str| {
        state
            .metrics
            .get(m)
            .map(|p| value_f64(&p.value))
            .unwrap_or(0.0)
    };
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));

    let tiles = row![
        metric_tile("requests", get("http/requests_total")),
        metric_tile("latency p50 (ms)", get("http/latency_p50_ms")),
        metric_tile("latency p95 (ms)", get("http/latency_p95_ms")),
    ]
    .spacing(space::SM);
    let mut col = column![section_header("HTTP (RED)", None), tiles].spacing(space::SM);

    // Status-class distribution as a bar chart (fixed 2xx→5xx order).
    let statuses: Vec<(String, f64)> = ["2xx", "3xx", "4xx", "5xx"]
        .iter()
        .map(|c| (c.to_string(), getf(&format!("http/status_{c}_total"))))
        .filter(|(_, v)| *v > 0.0)
        .collect();
    if !statuses.is_empty() {
        col = col
            .push(text("by status class").size(font::CAPTION).style(dim))
            .push(chart::ranked_bar(&statuses, |v| format_count(v as u64), 4));
    }

    // Method distribution as a bar chart (desc by count).
    let mut methods: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| match Subject::parse_metric(m) {
            Some(Subject::HttpMethods { method }) => Some((
                method.trim_end_matches("_total").to_string(),
                value_f64(&p.value),
            )),
            _ => None,
        })
        .collect();
    methods.sort_by(|a, b| b.1.total_cmp(&a.1));
    if !methods.is_empty() {
        col = col
            .push(text("by method").size(font::CAPTION).style(dim))
            .push(chart::ranked_bar(&methods, |v| format_count(v as u64), 8));
    }

    // On-demand top-hosts / error-hosts drill-down via `@rpc/netring/http` (#45).
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
            use zensight_common::HttpHostRecord;
            let columns = vec![
                TableColumn::fill("host", 6, |r: &HttpHostRecord| {
                    text(r.host.clone()).size(font::CAPTION).into()
                })
                .sortable(|r: &HttpHostRecord| SortKey::Text(r.host.clone())),
                TableColumn::fixed("requests", 100.0, |r: &HttpHostRecord| {
                    text(r.requests.to_string()).size(font::CAPTION).into()
                })
                .sortable(|r: &HttpHostRecord| SortKey::Num(r.requests as f64)),
                TableColumn::fixed("errors", 100.0, |r: &HttpHostRecord| {
                    let t = text(r.errors.to_string()).size(font::CAPTION);
                    if r.errors > 0 { t.style(warn) } else { t }.into()
                })
                .sortable(|r: &HttpHostRecord| SortKey::Num(r.errors as f64)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &HttpHostRecord| r.host.clone())
                    .on_sort(|c| Message::NetringTableSort(NetringTable::Http, c))
                    .on_filter(|q| Message::NetringTableFilter(NetringTable::Http, q))
                    .on_more(Message::NetringTableMore(NetringTable::Http))
                    .noun("hosts")
                    .view(records, state.netring_detail.table(NetringTable::Http)),
            );
        }
    }
    col.into()
}

/// Top-talker drill-down (#45): the per-destination histogram the sensor serves
/// on `@rpc/netring/talkers` — distinct from the per-app bandwidth card. "Who are the
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
            // Ranked bar chart of the busiest sources by rolling bytes/sec (#369).
            let bars: Vec<(String, f64)> = records
                .iter()
                .take(RANKED_BAR_ROWS)
                .map(|r| (r.src.clone(), r.bytes_per_sec))
                .collect();
            col = col.push(chart::ranked_bar(&bars, format_rate, RANKED_BAR_ROWS));

            let columns = vec![
                TableColumn::fill("source", 5, |r: &zensight_common::TalkerRecord| {
                    pivot_button(state, &r.src, &r.src)
                })
                .sortable(|r: &zensight_common::TalkerRecord| SortKey::Text(r.src.clone())),
                TableColumn::fixed("rate", 130.0, |r: &zensight_common::TalkerRecord| {
                    text(format_rate(r.bytes_per_sec))
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &zensight_common::TalkerRecord| SortKey::Num(r.bytes_per_sec)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &zensight_common::TalkerRecord| r.src.clone())
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
/// by byte volume, served on `@rpc/netring/matrix`. "Who talks to whom?" — the service
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
                TableColumn::fixed("rate", 130.0, |r: &zensight_common::MatrixRecord| {
                    text(format_rate(r.bytes_per_sec))
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &zensight_common::MatrixRecord| SortKey::Num(r.bytes_per_sec)),
            ];
            col = col.push(
                DataTable::new(columns)
                    .searchable(|r: &zensight_common::MatrixRecord| format!("{} {}", r.src, r.dst))
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
            grid[s][d] += r.bytes_per_sec;
        }
    }
    Some(chart::heatmap(&grid, 16.0))
}

/// Elephant-flow drill-down (#45): the biggest recently-ended flows, served on
/// `@rpc/netring/elephant_flows`. "What were the biggest transfers?"
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
                    text(format_bytes(r.bytes as f64))
                        .size(font::CAPTION)
                        .into()
                })
                .sortable(|r: &ElephantRecord| SortKey::Num(r.bytes as f64)),
                TableColumn::fixed("out↑ / in↓", 150.0, |r: &ElephantRecord| {
                    text(dir_split(r.bytes_initiator, r.bytes_responder))
                        .size(font::CAPTION)
                        .into()
                }),
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

/// Per-L4 (tcp/udp/icmp) split (#45): a donut of the byte distribution over a
/// compact flows/bytes table.
fn render_per_l4(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut col = column![section_header("Per-protocol (L4)", None)].spacing(space::SM);

    // Byte-distribution donut (skips protocols with no bytes).
    let split: Vec<(String, f64)> = ["tcp", "udp", "icmp"]
        .iter()
        .filter_map(|proto| {
            metric_f64(state, &format!("flow/by_l4/{proto}/bytes_total"))
                .filter(|v| *v > 0.0)
                .map(|v| (proto.to_string(), v))
        })
        .collect();
    if !split.is_empty() {
        col = col.push(chart::donut(&split, 90.0));
    }

    col = col.push(row![cell("proto", 120), cell("flows", 120), cell("bytes", 140)].spacing(8));
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

/// Compact capture-health chip for the Overview tab (#247): backend + worst
/// drop-rate, tinted and flagged when a source is overloaded or shedding.
/// `None` when the sensor publishes no `capture/*` metrics (e.g. pcap replay).
fn capture_chip(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    let has_capture = state.metrics.keys().any(|k| k.starts_with("capture/"));
    if !has_capture {
        return None;
    }
    let backend = match state.metrics.get("capture/backend").map(|p| &p.value) {
        Some(TelemetryValue::Text(s)) => s.clone(),
        _ => "capture".to_string(),
    };
    // Worst windowed drop-rate across sources.
    let worst = state
        .metrics
        .iter()
        .filter(|(k, _)| k.starts_with("capture/") && k.ends_with("/drop_rate"))
        .map(|(_, p)| value_f64(&p.value))
        .fold(0.0_f64, f64::max);
    let shedding = state.metrics.iter().any(|(k, p)| {
        k.starts_with("capture/") && k.ends_with("/shed/active") && value_f64(&p.value) >= 1.0
    });
    let style: fn(&Theme) -> text::Style = if worst >= OVERLOAD_DROP_RATE {
        danger
    } else if shedding || worst >= 0.01 {
        warn
    } else {
        dim
    };
    let mut label = format!("capture: {backend} · drop {:.2}%", worst * 100.0);
    if shedding {
        label.push_str(" · SHEDDING");
    } else if worst >= OVERLOAD_DROP_RATE {
        label.push_str(" · OVERLOAD");
    }
    Some(card(text(label).size(font::CAPTION).style(style)))
}

/// On-demand recent-flow detail: a fetch button + the fetched flow table (P2 —
/// pulled from the sensor's `@rpc/netring/flows` channel, never streamed).
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
            // Flow ↔ process join result (#309), for the row whose "who?" was
            // clicked last.
            if let Some(line) = attribution_line(state.netring_detail.attribution.as_ref()) {
                col = col.push(line);
            }
        }
    }
    col.into()
}

/// Render the flow↔process join outcome (#309): the owning process labelled
/// with its attribution source, "unattributed" when no socket matched, or the
/// no-netlink hint. `None` while nothing was asked.
fn attribution_line<'a>(
    attribution: Option<&'a (
        String,
        Fetch<Option<crate::view::specialized::attribution::AttributedProcess>>,
    )>,
) -> Option<Element<'a, Message>> {
    let (key, fetch) = attribution?;
    let line: Element<'a, Message> = match fetch {
        Fetch::Idle | Fetch::Loading => text(format!("{key}: looking up owning process…"))
            .size(font::CAPTION)
            .style(dim)
            .into(),
        Fetch::Error(e) => text(format!("{key}: unattributed ({e})"))
            .size(font::CAPTION)
            .style(dim)
            .into(),
        Fetch::Ready(Some(a)) => text(format!("{key}: {} — endpoint {}", a.display(), a.endpoint))
            .size(font::CAPTION)
            .into(),
        Fetch::Ready(None) => text(format!(
            "{key}: unattributed (no matching socket on any netlink host)"
        ))
        .size(font::CAPTION)
        .style(dim)
        .into(),
    };
    Some(line)
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
        // Flow ↔ process join (#309): ask the endpoint hosts' netlink sensors
        // who owns this 5-tuple. The result renders below the table.
        TableColumn::fixed("process", 60.0, |f: &zensight_common::FlowRecord| {
            button(text("who?").size(font::CAPTION))
                .padding([2, 6])
                .style(iced::widget::button::text)
                .on_press(Message::FetchFlowAttribution {
                    target: crate::message::AttributionTarget::Device,
                    key: crate::view::specialized::attribution::flow_key(&f.src, &f.dst),
                    src: f.src.clone(),
                    dst: f.dst.clone(),
                })
                .into()
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

/// How many apps the stacked-area bandwidth trend stacks (#251) — kept below
/// the table's top-N so the bands and legend stay legible.
const BANDWIDTH_TREND_SERIES: usize = 6;

/// Pixel height of the stacked-area bandwidth trend (#251).
const BANDWIDTH_TREND_HEIGHT: f32 = 120.0;

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
        .filter_map(|(metric, point)| match Subject::parse_metric(metric) {
            Some(Subject::BandwidthBytesPerSec { app }) => {
                Some((app.to_string(), value_f64(&point.value)))
            }
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    // Contextual pivot (#351): the richer process/service live monitor is a
    // global view — surface it from here, pre-scoped to this host.
    let monitor_btn = button(text("Open in Bandwidth monitor").size(font::CAPTION))
        .padding([2, 8])
        .on_press(Message::OpenBandwidthForHost(
            state.device_id.source.clone(),
        ));
    let title = section_header(
        format!("Per-app bandwidth ({})", rows.len()),
        Some(monitor_btn.into()),
    );
    if rows.is_empty() {
        return column![title, empty_state("No bandwidth data", None)]
            .spacing(space::SM)
            .into();
    }

    // Stacked-area trend of the top apps over the stored history (#251) — the
    // share-over-time view; per-row sparklines below show shape only.
    let series: Vec<(String, Vec<f64>)> = rows
        .iter()
        .take(BANDWIDTH_TREND_SERIES)
        .map(|(app, _)| {
            let metric = format!("bandwidth/{app}/bytes_per_sec");
            (app.clone(), state.history_values(&metric, 60))
        })
        .filter(|(_, values)| values.len() >= 2)
        .collect();
    let trend: Option<Element<'_, Message>> = (!series.is_empty())
        .then(|| chart::stacked_area(&series, format_rate, BANDWIDTH_TREND_HEIGHT));

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
    let mut content = column![title].spacing(space::SM);
    if let Some(trend) = trend {
        content = content.push(trend);
    }
    content.push(bars).push(list).push(footer).into()
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

/// A compact RED/KPI tile — the shared kit form (#350), kept as a thin local
/// alias so call sites stay short.
fn metric_tile<'a>(label: &str, value: String) -> Element<'a, Message> {
    crate::view::components::metric_tile(label, value)
}

#[cfg(test)]
mod tests {
    use iced_test::simulator;
    use zensight_common::{MatrixRecord, Protocol};

    use super::*;
    use crate::message::DeviceId;
    use crate::view::specialized::fetch::Fetch;

    #[test]
    fn matrix_destination_pivots_to_flows() {
        // The matrix table's destination is a drill-down pivot (#246). Use the
        // matrix (heatmap is canvas, no text) so the clicked dst is unambiguous.
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "host01"));
        state.netring_detail.matrix = Fetch::Ready(vec![MatrixRecord {
            src: "10.0.0.1:5555".to_string(),
            dst: "10.0.0.42:443".to_string(),
            bytes_per_sec: 1234.0,
            names: Vec::new(),
        }]);
        let mut ui = simulator(render_matrix(&state));
        let _ = ui.click("10.0.0.42:443");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::NetringPivotToFlows(d, ep)
                if d.source == "host01" && ep == "10.0.0.42:443"
        )));
    }

    /// #327: seed a state with `capture/disk/*` telemetry + a fetched capture
    /// index, then pin the Capture-to-disk section: capture-now emits the manual
    /// trigger, a served file's Download emits the blob download, a mode button
    /// emits the hot-switch.
    fn capture_disk_state() -> DeviceDetailState {
        use zensight_common::{CaptureRecord, TelemetryPoint};
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "host01"));
        for (metric, value) in [
            (
                "capture/disk/mode",
                TelemetryValue::Text("triggered".into()),
            ),
            ("capture/disk/ring_packets", TelemetryValue::Gauge(1200.0)),
            (
                "capture/disk/ring_bytes",
                TelemetryValue::Gauge(4.0 * 1024.0 * 1024.0),
            ),
            ("capture/disk/retained_files", TelemetryValue::Gauge(2.0)),
            (
                "capture/disk/retained_bytes",
                TelemetryValue::Gauge(90.0 * 1024.0 * 1024.0),
            ),
            ("capture/disk/triggers", TelemetryValue::Counter(3)),
        ] {
            state.metrics.insert(
                metric.to_string(),
                TelemetryPoint::new("host01", Protocol::Netring, metric.to_string(), value),
            );
        }
        state.netring_detail.captures = Fetch::Ready(vec![
            CaptureRecord {
                filename: "zensight-host01-trigger-BeaconRita-1.pcap.zst".into(),
                bytes: 2 * 1024 * 1024,
                packets: 812,
                mode: "triggered".into(),
                trigger_kind: Some("BeaconRita".into()),
                artifact_id: Some("01J00000000000000000000000".into()),
                // A servable record names the origin holding it and the root
                // to pin — a bulk fetch may not wildcard the origin (RFC 07 §3).
                artifact_prefix: Some("v1/h-3fa9c2d41b7e/@blob/artifact".into()),
                artifact_root: Some(zenkey::ContentHash::parse(&"ab".repeat(32)).unwrap()),
                ..Default::default()
            },
            CaptureRecord {
                filename: "zensight-host01-trigger-PortScanTRW-0.pcap.zst".into(),
                bytes: 1024,
                packets: 5,
                mode: "triggered".into(),
                trigger_kind: Some("PortScanTRW".into()),
                artifact_id: None, // TTL reaped — no download affordance
                ..Default::default()
            },
            CaptureRecord {
                filename: "zensight-host01-trigger-Legacy-0.pcap.zst".into(),
                bytes: 4096,
                packets: 9,
                mode: "triggered".into(),
                trigger_kind: Some("Legacy".into()),
                // Served by a pre-wire-v2 sensor: an id but no origin. There
                // is no download to offer — a wildcard fetch is forbidden, and
                // that sensor could not answer this build anyway.
                artifact_id: Some("01J00000000000000000000001".into()),
                artifact_prefix: None,
                ..Default::default()
            },
        ]);
        state
    }

    #[test]
    fn capture_to_disk_section_renders_and_controls_emit() {
        let state = capture_disk_state();
        let section = render_capture_to_disk(&state).expect("disk telemetry present");
        let mut ui = simulator(section);
        assert!(ui.find("Capture to disk").is_ok());
        assert!(ui.find("triggered").is_ok());
        // The served file offers Download; the reaped one shows "expired"; the
        // one from a sensor that named no origin says so instead of offering a
        // fetch it is not allowed to make.
        assert!(ui.find("expired").is_ok());
        assert!(ui.find("sensor too old").is_ok());
        let _ = ui.click("Capture now");
        let _ = ui.click("Download");
        let _ = ui.click("rotating");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(m, Message::NetringCaptureNow)));
        // The emitted message carries the concrete origin and the root, which
        // is what lets the fetch be both literal-keyed (RFC 07 §3) and
        // anchored (§2.1).
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::DownloadCaptureBlob { artifact_id, filename, blob_prefix, root, .. }
                if artifact_id == "01J00000000000000000000000"
                    && filename.contains("BeaconRita")
                    && !blob_prefix.contains('*')
                    && root.is_some()
        )));
        // …and only one row is downloadable: the prefix-less record must not
        // have produced a message at all, or the guard above is decorative.
        assert_eq!(
            msgs.iter()
                .filter(|m| matches!(m, Message::DownloadCaptureBlob { .. }))
                .count(),
            1,
            "a record without an origin must offer no download"
        );
        assert!(
            msgs.iter().any(
                |m| matches!(m, Message::NetringSetCaptureDiskMode(mode) if mode == "rotating")
            )
        );
    }

    #[test]
    fn capture_to_disk_hidden_without_disk_telemetry() {
        let state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "host01"));
        assert!(render_capture_to_disk(&state).is_none());
    }

    /// #309: the flows table's "who?" affordance emits the flow↔process join
    /// with the exact 5-tuple endpoints; the join outcome renders below.
    #[test]
    fn flow_who_button_emits_attribution_fetch() {
        use crate::view::specialized::attribution::{AttributedProcess, AttributionSource};
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "host01"));
        state.netring_detail.flows = Fetch::Ready(vec![zensight_common::FlowRecord {
            src: "10.0.0.5:44444".into(),
            dst: "1.1.1.1:443".into(),
            proto: "tcp".into(),
            bytes: 100,
            packets: 2,
            duration_ms: 10,
            reason: "fin".into(),
            community_id: None,
            directed: true,
            bytes_initiator: 60,
            bytes_responder: 40,
            packets_initiator: 1,
            packets_responder: 1,
            dst_names: Vec::new(),
        }]);
        // A previously-fetched attribution renders under the table.
        state.netring_detail.attribution = Some((
            "10.0.0.5:44444 → 1.1.1.1:443".into(),
            Fetch::Ready(Some(AttributedProcess {
                pid: Some(4242),
                comm: Some("curl".into()),
                uid: 1000,
                state: "established".into(),
                endpoint: "10.0.0.5:44444".into(),
                source: AttributionSource::LiveSocket,
            })),
        ));
        let mut ui = simulator(render_flow_detail(&state));
        assert!(
            ui.find(
                "10.0.0.5:44444 → 1.1.1.1:443: curl (4242) · uid 1000 · live socket \
                 — endpoint 10.0.0.5:44444"
            )
            .is_ok()
        );
        let _ = ui.click("who?");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::FetchFlowAttribution { target: crate::message::AttributionTarget::Device, src, dst, .. }
                if src == "10.0.0.5:44444" && dst == "1.1.1.1:443"
        )));
    }

    /// #309: no socket matched → graceful "unattributed", never an error look.
    #[test]
    fn flow_attribution_unattributed_renders_gracefully() {
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "host01"));
        state.netring_detail.attribution =
            Some(("10.0.0.5:1 → 1.1.1.1:2".into(), Fetch::Ready(None)));
        let line = attribution_line(state.netring_detail.attribution.as_ref())
            .expect("line renders once asked");
        let mut ui = simulator(line);
        assert!(
            ui.find(
                "10.0.0.5:1 → 1.1.1.1:2: unattributed (no matching socket on any netlink host)"
            )
            .is_ok()
        );
    }
}
