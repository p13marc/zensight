//! NetFlow traffic analysis specialized view.
//!
//! Two data sources, and the split is the point (RFC 08 §4):
//!
//! - **Rollups** — `flows_total` / `bytes_total` / `packets_total` /
//!   `by_proto/{proto}/flows` — are *streamed* telemetry, because they are
//!   bounded per exporter. They drive the summary and the protocol mix.
//! - **Individual flows** are *pulled* from `@rpc/netflow/flows` (#469), because
//!   a flow is an event with unbounded cardinality, not a metric. The bounded
//!   ring behind that procedure is exactly what keyspace-v2 put in place of the
//!   per-flow-pair telemetry keys the old keyspace invited.
//!
//! Before #469 this view reconstructed flows from telemetry *labels* — and the
//! sensor publishes `labels: HashMap::new()`, so every "flow" it drew was a
//! fabrication: one row per rollup series, each reading `0.0.0.0:0 → 0.0.0.0:0`,
//! protocol 0, with the rollup counter's value in the bytes column. The
//! procedure that fixes it shipped with the cutover and had no caller.

use std::collections::HashMap;

use iced::widget::{Column, Row, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{NetflowRecord, TelemetryValue};

use crate::message::Message;
use crate::view::components::{
    Column as TableColumn, DataTable, SortKey, card, empty_state, section_header,
};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::{format_bytes, format_count};
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::tokens::{font, space};

/// IANA protocol number → name, for the handful that actually show up. NetFlow
/// carries the number; a human reads the name.
fn protocol_name(proto: u8) -> &'static str {
    match proto {
        1 => "ICMP",
        6 => "TCP",
        17 => "UDP",
        47 => "GRE",
        50 => "ESP",
        58 => "ICMPv6",
        _ => "Other",
    }
}

/// The 5-tuple of a NetFlow record, as far as the exporter's template gives it.
///
/// NetFlow v9/IPFIX field names are defined by the *device*, not by us, so every
/// accessor here is a lookup that can miss. A missing field is rendered as `—`,
/// never invented — inventing them is what produced the phantom flows this view
/// used to show.
struct Tuple {
    src: Option<String>,
    dst: Option<String>,
    src_port: Option<String>,
    dst_port: Option<String>,
    proto: Option<u8>,
    bytes: u64,
    packets: u64,
}

impl Tuple {
    fn of(r: &NetflowRecord) -> Self {
        // Both the v5 field names and the IPFIX information-element names are in
        // the wild; try each.
        let f = |names: &[&str]| -> Option<String> { names.iter().find_map(|n| r.field(n)) };
        let n = |names: &[&str]| -> u64 { f(names).and_then(|v| v.parse().ok()).unwrap_or(0) };
        Self {
            src: f(&[
                "src_addr",
                "sourceIPv4Address",
                "sourceIPv6Address",
                "src_ip",
            ]),
            dst: f(&[
                "dst_addr",
                "destinationIPv4Address",
                "destinationIPv6Address",
                "dst_ip",
            ]),
            src_port: f(&["src_port", "sourceTransportPort"]),
            dst_port: f(&["dst_port", "destinationTransportPort"]),
            proto: f(&["protocol", "protocolIdentifier"]).and_then(|v| v.parse().ok()),
            bytes: n(&["bytes", "octetDeltaCount", "in_bytes"]),
            packets: n(&["packets", "packetDeltaCount", "in_packets"]),
        }
    }

    fn endpoint(a: &Option<String>, port: &Option<String>) -> String {
        match (a, port) {
            (Some(a), Some(p)) => format!("{a}:{p}"),
            (Some(a), None) => a.clone(),
            _ => "—".into(),
        }
    }

    fn src_endpoint(&self) -> String {
        Self::endpoint(&self.src, &self.src_port)
    }

    fn dst_endpoint(&self) -> String {
        Self::endpoint(&self.dst, &self.dst_port)
    }

    fn proto_name(&self) -> &'static str {
        self.proto.map(protocol_name).unwrap_or("—")
    }
}

/// Render the NetFlow traffic specialized view.
pub fn netflow_traffic_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let content = column![
        render_header(state),
        card(render_summary(state)),
        card(render_protocol_distribution(state)),
        card(render_top_talkers(state)),
        card(render_flow_table(state)),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and exporter info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let exporter_name = text(format!("Exporter: {}", state.device_id.source)).size(24);

    row![back_button, protocol_icon, exporter_name]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// A rollup counter for this exporter, by metric-tail suffix.
///
/// NetFlow keeps a `{exporter}/{metric...}` rest-var in the registry by design —
/// the metric tree is the exporter's, not ours — so this is one of the sites
/// that stays hand-parsed (#475), and says so.
fn rollup(state: &DeviceDetailState, suffix: &str) -> Option<u64> {
    state
        .metrics
        .values()
        .find(|p| p.metric.ends_with(suffix))
        .and_then(|p| match &p.value {
            TelemetryValue::Counter(c) => Some(*c),
            TelemetryValue::Gauge(g) => Some(*g as u64),
            _ => None,
        })
}

/// Traffic summary — from the streamed rollups, which are the numbers the sensor
/// actually publishes.
fn render_summary(state: &DeviceDetailState) -> Element<'_, Message> {
    let flows = rollup(state, "/flows_total");
    let bytes = rollup(state, "/bytes_total");
    let packets = rollup(state, "/packets_total");

    if flows.is_none() && bytes.is_none() && packets.is_none() {
        return column![
            section_header("Traffic", None),
            empty_state("No rollups yet from this exporter.", None),
        ]
        .spacing(space::SM)
        .into();
    }

    let cell = |label: &'static str, v: String| -> Element<'_, Message> {
        column![
            text(label)
                .size(font::CAPTION)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
            text(v).size(font::EMPHASIS),
        ]
        .spacing(2)
        .into()
    };

    let items: Vec<Element<'_, Message>> = vec![
        cell(
            "flows",
            flows.map(format_count).unwrap_or_else(|| "—".into()),
        ),
        cell(
            "bytes",
            bytes
                .map(|b| format_bytes(b as f64))
                .unwrap_or_else(|| "—".into()),
        ),
        cell(
            "packets",
            packets.map(format_count).unwrap_or_else(|| "—".into()),
        ),
    ];

    column![
        section_header("Traffic", None),
        Row::with_children(items).spacing(space::XL),
    ]
    .spacing(space::SM)
    .into()
}

/// Protocol mix — from the `by_proto/{proto}/flows` rollups.
fn render_protocol_distribution(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::protocol(IconSize::Medium),
        text("Protocol Distribution").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut by_protocol: HashMap<String, u64> = HashMap::new();
    for point in state.metrics.values() {
        // `<exporter>/by_proto/<proto>/flows`
        let Some(rest) = point.metric.split("by_proto/").nth(1) else {
            continue;
        };
        let Some(proto) = rest.strip_suffix("/flows") else {
            continue;
        };
        let count = match &point.value {
            TelemetryValue::Counter(c) => *c,
            TelemetryValue::Gauge(g) => *g as u64,
            _ => continue,
        };
        *by_protocol.entry(proto.to_string()).or_insert(0) += count;
    }

    let total: u64 = by_protocol.values().sum();
    let mut sorted: Vec<_> = by_protocol.into_iter().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.1));

    let mut bars: Vec<Element<'_, Message>> = Vec::new();
    let colors = theme::PROTOCOL_CATEGORY;

    for (i, (proto, flows)) in sorted.iter().take(6).enumerate() {
        let pct = if total > 0 {
            (*flows as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let color = colors[i % colors.len()];
        let bar = container(text(""))
            .width(Length::Fixed(((pct * 2.0) as f32).max(5.0)))
            .height(Length::Fixed(16.0))
            .style(move |_t: &Theme| container::Style {
                background: Some(iced::Background::Color(color)),
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
        // The rollup key carries the protocol *number*; name it if we can.
        let name = proto
            .parse::<u8>()
            .map(protocol_name)
            .map(str::to_string)
            .unwrap_or_else(|_| proto.clone());
        bars.push(
            row![bar, text(format!("{name} {pct:.0}%")).size(11)]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
        );
    }

    if bars.is_empty() {
        bars.push(empty_state("No protocol rollups yet", None));
    }

    column![title, Column::with_children(bars).spacing(6)]
        .spacing(10)
        .into()
}

/// Top talkers, folded from the fetched flow ring.
fn render_top_talkers(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::chart(IconSize::Medium),
        text("Top Talkers (by bytes)").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let Some(records) = state.netflow_detail.flows.ready() else {
        return column![
            title,
            empty_state("Fetch the flow ring below to see talkers.", None)
        ]
        .spacing(10)
        .into();
    };

    let mut talkers: HashMap<(String, String), u64> = HashMap::new();
    for r in records {
        let t = Tuple::of(r);
        *talkers
            .entry((t.src_endpoint(), t.dst_endpoint()))
            .or_insert(0) += t.bytes;
    }
    let mut sorted: Vec<_> = talkers.into_iter().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.1));

    if sorted.is_empty() {
        return column![title, empty_state("No flows in the ring.", None)]
            .spacing(10)
            .into();
    }

    let mut rows = Column::new().spacing(4);
    for (i, ((src, dst), bytes)) in sorted.into_iter().take(10).enumerate() {
        let row_content = row![
            text(format!("{}.", i + 1))
                .size(11)
                .width(Length::Fixed(25.0)),
            text(src).size(11).width(Length::Fixed(160.0)),
            text("→").size(11),
            text(dst).size(11).width(Length::Fixed(160.0)),
            text(format_bytes(bytes as f64))
                .size(11)
                .width(Length::Fixed(80.0))
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).primary()),
                }),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        rows = rows.push(container(row_content).padding(6).width(Length::Fill).style(
            |t: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::colors(t).row_background())),
                border: iced::Border {
                    color: theme::colors(t).border_subtle(),
                    width: 1.0,
                    radius: 2.0.into(),
                },
                ..Default::default()
            },
        ));
    }

    column![title, rows].spacing(10).into()
}

/// The recent-flow ring, pulled from `@rpc/netflow/flows` (#469).
fn render_flow_table(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::table(IconSize::Medium),
        text("Recent Flows").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let detail = &state.netflow_detail;

    if detail.flows.is_loading() {
        return column![title, empty_state("Fetching the flow ring…", None)]
            .spacing(10)
            .into();
    }
    if let Some(err) = detail.flows.error() {
        return column![title, empty_state(format!("Fetch failed: {err}"), None)]
            .spacing(10)
            .into();
    }
    let fetch = button(text("Fetch flows").size(font::CAPTION))
        .padding([4, 10])
        .on_press(Message::FetchNetflowFlows);

    let Some(records) = detail.flows.ready() else {
        return column![
            title,
            text(
                "Individual flows are pulled on demand — they are events with unbounded \
                 cardinality, never published as keys."
            )
            .size(font::CAPTION),
            fetch,
        ]
        .spacing(10)
        .into();
    };
    if records.is_empty() {
        return column![
            title,
            empty_state("The exporter's flow ring is empty.", None),
            fetch,
        ]
        .spacing(10)
        .into();
    }

    let columns = vec![
        TableColumn::fill("source", 3, |r: &NetflowRecord| {
            text(Tuple::of(r).src_endpoint()).size(font::CAPTION).into()
        })
        .sortable(|r: &NetflowRecord| SortKey::Text(Tuple::of(r).src_endpoint())),
        TableColumn::fill("destination", 3, |r: &NetflowRecord| {
            text(Tuple::of(r).dst_endpoint()).size(font::CAPTION).into()
        })
        .sortable(|r: &NetflowRecord| SortKey::Text(Tuple::of(r).dst_endpoint())),
        TableColumn::fixed("proto", 70.0, |r: &NetflowRecord| {
            text(Tuple::of(r).proto_name()).size(font::CAPTION).into()
        })
        .sortable(|r: &NetflowRecord| SortKey::Text(Tuple::of(r).proto_name().to_string())),
        TableColumn::fixed("bytes", 90.0, |r: &NetflowRecord| {
            text(format_bytes(Tuple::of(r).bytes as f64))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &NetflowRecord| SortKey::Num(Tuple::of(r).bytes as f64)),
        TableColumn::fixed("packets", 80.0, |r: &NetflowRecord| {
            text(format_count(Tuple::of(r).packets))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &NetflowRecord| SortKey::Num(Tuple::of(r).packets as f64)),
        TableColumn::fixed("version", 70.0, |r: &NetflowRecord| {
            text(format!("v{}", r.version)).size(font::CAPTION).into()
        })
        .sortable(|r: &NetflowRecord| SortKey::Num(r.version as f64)),
    ];

    column![
        title,
        fetch,
        DataTable::new(columns)
            .searchable(|r: &NetflowRecord| {
                let t = Tuple::of(r);
                format!("{} {}", t.src_endpoint(), t.dst_endpoint())
            })
            .on_sort(Message::NetflowTableSort)
            .on_filter(Message::NetflowTableFilter)
            .on_more(Message::NetflowTableMore)
            .noun("flows")
            .view(records, &detail.table),
    ]
    .spacing(10)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::{NetflowFieldValue, Protocol};

    fn record(src: &str, dst: &str, proto: u64, bytes: u64) -> NetflowRecord {
        let mut fields = HashMap::new();
        fields.insert("src_addr".into(), NetflowFieldValue::IpAddr(src.into()));
        fields.insert("dst_addr".into(), NetflowFieldValue::IpAddr(dst.into()));
        fields.insert("src_port".into(), NetflowFieldValue::Uint(51234));
        fields.insert("dst_port".into(), NetflowFieldValue::Uint(443));
        fields.insert("protocol".into(), NetflowFieldValue::Uint(proto));
        fields.insert("bytes".into(), NetflowFieldValue::Uint(bytes));
        fields.insert("packets".into(), NetflowFieldValue::Uint(12));
        NetflowRecord {
            exporter_ip: "10.0.0.1".into(),
            exporter_name: "router01".into(),
            version: 9,
            fields,
            timestamp: 1,
        }
    }

    #[test]
    fn tuple_reads_the_five_tuple_out_of_the_template_fields() {
        let r = record("10.0.0.5", "1.1.1.1", 6, 4096);
        let t = Tuple::of(&r);
        assert_eq!(t.src_endpoint(), "10.0.0.5:51234");
        assert_eq!(t.dst_endpoint(), "1.1.1.1:443");
        assert_eq!(t.proto_name(), "TCP");
        assert_eq!(t.bytes, 4096);
    }

    /// A template that does not carry the 5-tuple must render `—`, not a
    /// fabricated `0.0.0.0:0` — the bug #469's procedure exists to fix.
    #[test]
    fn a_missing_field_is_not_invented() {
        let r = NetflowRecord {
            exporter_ip: "10.0.0.1".into(),
            exporter_name: "router01".into(),
            version: 5,
            fields: HashMap::new(),
            timestamp: 1,
        };
        let t = Tuple::of(&r);
        assert_eq!(t.src_endpoint(), "—");
        assert_eq!(t.dst_endpoint(), "—");
        assert_eq!(t.proto_name(), "—");
        assert_eq!(t.bytes, 0);
    }

    #[test]
    fn the_flow_table_renders_fetched_records() {
        let device_id = DeviceId::new(Protocol::Netflow, "router01");
        let mut state = DeviceDetailState::new(device_id);
        state
            .netflow_detail
            .apply(Ok(vec![record("10.0.0.5", "1.1.1.1", 6, 4096)]));

        let mut ui = iced_test::simulator(netflow_traffic_view(&state));
        assert!(ui.find("10.0.0.5:51234").is_ok());
        assert!(ui.find("1.1.1.1:443").is_ok());
    }

    /// Before a fetch the view must offer one, not silently show an empty table.
    #[test]
    fn idle_offers_a_fetch() {
        let device_id = DeviceId::new(Protocol::Netflow, "router01");
        let state = DeviceDetailState::new(device_id);
        let mut ui = iced_test::simulator(netflow_traffic_view(&state));
        assert!(ui.find("Fetch flows").is_ok());
    }
}
