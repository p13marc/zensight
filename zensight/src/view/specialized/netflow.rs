//! NetFlow traffic analysis specialized view.
//!
//! Displays flow data with top talkers, protocol distribution,
//! and traffic timeline.

use std::collections::HashMap;

use iced::widget::{Column, Row, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::icons::{self, IconSize};

/// Parsed flow record.
#[derive(Debug, Clone)]
struct FlowRecord {
    src_ip: String,
    dst_ip: String,
    src_port: u16,
    dst_port: u16,
    protocol: u8,
    bytes: u64,
    packets: u64,
    timestamp: i64,
}

impl FlowRecord {
    fn protocol_name(&self) -> &'static str {
        match self.protocol {
            1 => "ICMP",
            6 => "TCP",
            17 => "UDP",
            47 => "GRE",
            50 => "ESP",
            _ => "Other",
        }
    }
}

/// Render the NetFlow traffic specialized view.
pub fn netflow_traffic_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let summary = render_summary(state);
    let top_talkers = render_top_talkers(state);
    let protocol_dist = render_protocol_distribution(state);
    let flow_table = render_flow_table(state);

    let content = column![
        header,
        rule::horizontal(1),
        summary,
        rule::horizontal(1),
        top_talkers,
        rule::horizontal(1),
        protocol_dist,
        rule::horizontal(1),
        flow_table,
    ]
    .spacing(15)
    .padding(20);

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

    let flow_count = state.metrics.len();
    let count_text = text(format!("{} flows", flow_count)).size(14);

    row![back_button, protocol_icon, exporter_name, count_text]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render traffic summary.
fn render_summary(state: &DeviceDetailState) -> Element<'_, Message> {
    let flows = parse_flows(state);

    let total_bytes: u64 = flows.iter().map(|f| f.bytes).sum();
    let total_packets: u64 = flows.iter().map(|f| f.packets).sum();
    let unique_sources: usize = flows
        .iter()
        .map(|f| &f.src_ip)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let unique_dests: usize = flows
        .iter()
        .map(|f| &f.dst_ip)
        .collect::<std::collections::HashSet<_>>()
        .len();

    let items = vec![
        format!("Total: {}", format_bytes(total_bytes as f64)),
        format!("Packets: {}", format_count(total_packets)),
        format!("Sources: {}", unique_sources),
        format!("Destinations: {}", unique_dests),
    ];

    let item_widgets: Vec<Element<'_, Message>> =
        items.into_iter().map(|s| text(s).size(12).into()).collect();

    container(Row::with_children(item_widgets).spacing(30))
        .padding(10)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render top talkers section.
fn render_top_talkers(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::chart(IconSize::Medium),
        text("Top Talkers (by bytes)").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let flows = parse_flows(state);

    // Aggregate by source->destination pair
    let mut talkers: HashMap<(String, String), u64> = HashMap::new();
    for flow in &flows {
        let key = (flow.src_ip.clone(), flow.dst_ip.clone());
        *talkers.entry(key).or_insert(0) += flow.bytes;
    }

    let mut sorted_talkers: Vec<_> = talkers.into_iter().collect();
    sorted_talkers.sort_by(|a, b| b.1.cmp(&a.1));

    let is_empty = sorted_talkers.is_empty();
    let mut rows = Column::new().spacing(4);

    for (i, ((src, dst), bytes)) in sorted_talkers.into_iter().take(10).enumerate() {
        let rank = text(format!("{}.", i + 1))
            .size(11)
            .width(Length::Fixed(25.0));

        let src_text = text(src).size(11).width(Length::Fixed(120.0));

        let arrow = text("→").size(11).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        });

        let dst_text = text(dst).size(11).width(Length::Fixed(120.0));

        let bytes_text = text(format_bytes(bytes as f64))
            .size(11)
            .width(Length::Fixed(80.0))
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.4, 0.7, 0.9)),
            });

        let row_content = row![rank, src_text, arrow, dst_text, bytes_text]
            .spacing(10)
            .align_y(Alignment::Center);

        rows = rows.push(container(row_content).padding(6).width(Length::Fill).style(
            |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgb(
                    0.13, 0.13, 0.15,
                ))),
                border: iced::Border {
                    color: iced::Color::from_rgb(0.2, 0.2, 0.22),
                    width: 1.0,
                    radius: 2.0.into(),
                },
                ..Default::default()
            },
        ));
    }

    if is_empty {
        rows = rows.push(
            text("No flow data available")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                }),
        );
    }

    column![title, rows].spacing(10).into()
}

/// Render protocol distribution.
fn render_protocol_distribution(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::protocol(IconSize::Medium),
        text("Protocol Distribution").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let flows = parse_flows(state);

    // Aggregate bytes by protocol
    let mut by_protocol: HashMap<&'static str, u64> = HashMap::new();
    for flow in &flows {
        *by_protocol.entry(flow.protocol_name()).or_insert(0) += flow.bytes;
    }

    let total: u64 = by_protocol.values().sum();

    let mut sorted_protocols: Vec<_> = by_protocol.into_iter().collect();
    sorted_protocols.sort_by(|a, b| b.1.cmp(&a.1));

    let mut bars: Vec<Element<'_, Message>> = Vec::new();

    let colors = [
        iced::Color::from_rgb(0.3, 0.6, 0.9), // Blue - TCP
        iced::Color::from_rgb(0.4, 0.8, 0.4), // Green - UDP
        iced::Color::from_rgb(0.9, 0.5, 0.3), // Orange - ICMP
        iced::Color::from_rgb(0.7, 0.4, 0.8), // Purple - Other
    ];

    for (i, (proto, bytes)) in sorted_protocols.iter().take(4).enumerate() {
        let pct = if total > 0 {
            (*bytes as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let color = colors[i % colors.len()];

        let bar_width = (pct * 2.0) as f32; // Scale to reasonable width

        let bar = container(text(""))
            .width(Length::Fixed(bar_width.max(5.0)))
            .height(Length::Fixed(16.0))
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(color)),
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        let label = text(format!("{} {:.0}%", proto, pct)).size(11);

        bars.push(
            row![bar, label]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
        );
    }

    if bars.is_empty() {
        bars.push(
            text("No protocol data")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                })
                .into(),
        );
    }

    column![title, Column::with_children(bars).spacing(6)]
        .spacing(10)
        .into()
}

/// Render flow table.
fn render_flow_table(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::table(IconSize::Medium),
        text("Recent Flows").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let flows = parse_flows(state);

    // Table header
    let header = container(
        row![
            text("Source").size(10).width(Length::Fixed(100.0)),
            text("Src Port").size(10).width(Length::Fixed(60.0)),
            text("Dest").size(10).width(Length::Fixed(100.0)),
            text("Dst Port").size(10).width(Length::Fixed(60.0)),
            text("Proto").size(10).width(Length::Fixed(50.0)),
            text("Bytes").size(10).width(Length::Fixed(70.0)),
            text("Packets").size(10).width(Length::Fixed(60.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(6)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgb(
            0.18, 0.18, 0.2,
        ))),
        ..Default::default()
    });

    // Sort by timestamp descending
    let mut sorted_flows = flows;
    sorted_flows.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let is_empty = sorted_flows.is_empty();
    let mut table_rows = Column::new().spacing(2);

    for flow in sorted_flows.into_iter().take(20) {
        let proto_name = flow.protocol_name();
        let row_content = row![
            text(flow.src_ip).size(10).width(Length::Fixed(100.0)),
            text(format!("{}", flow.src_port))
                .size(10)
                .width(Length::Fixed(60.0)),
            text(flow.dst_ip).size(10).width(Length::Fixed(100.0)),
            text(format!("{}", flow.dst_port))
                .size(10)
                .width(Length::Fixed(60.0)),
            text(proto_name).size(10).width(Length::Fixed(50.0)),
            text(format_bytes(flow.bytes as f64))
                .size(10)
                .width(Length::Fixed(70.0)),
            text(format!("{}", flow.packets))
                .size(10)
                .width(Length::Fixed(60.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let row_container =
            container(row_content)
                .padding(6)
                .width(Length::Fill)
                .style(|_theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgb(
                        0.13, 0.13, 0.15,
                    ))),
                    border: iced::Border {
                        color: iced::Color::from_rgb(0.2, 0.2, 0.22),
                        width: 1.0,
                        radius: 2.0.into(),
                    },
                    ..Default::default()
                });

        table_rows = table_rows.push(row_container);
    }

    if is_empty {
        table_rows =
            table_rows.push(
                text("No flow records")
                    .size(12)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                    }),
            );
    }

    column![title, header, table_rows].spacing(10).into()
}

/// Parse flow records from metrics.
fn parse_flows(state: &DeviceDetailState) -> Vec<FlowRecord> {
    let mut flows = Vec::new();

    for point in state.metrics.values() {
        // Expect format: flow/<src>/<dst> or similar
        let src_ip = point
            .labels
            .get("src_ip")
            .or_else(|| point.labels.get("source"))
            .cloned()
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let dst_ip = point
            .labels
            .get("dst_ip")
            .or_else(|| point.labels.get("destination"))
            .cloned()
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let src_port: u16 = point
            .labels
            .get("src_port")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let dst_port: u16 = point
            .labels
            .get("dst_port")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let protocol: u8 = point
            .labels
            .get("protocol")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let bytes = match &point.value {
            TelemetryValue::Counter(c) => *c,
            TelemetryValue::Gauge(g) => *g as u64,
            _ => 0,
        };

        let packets: u64 = point
            .labels
            .get("packets")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        flows.push(FlowRecord {
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            protocol,
            bytes,
            packets,
            timestamp: point.timestamp,
        });
    }

    flows
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1_073_741_824.0 {
        format!("{:.1} GB", bytes / 1_073_741_824.0)
    } else if bytes >= 1_048_576.0 {
        format!("{:.1} MB", bytes / 1_048_576.0)
    } else if bytes >= 1024.0 {
        format!("{:.1} KB", bytes / 1024.0)
    } else {
        format!("{:.0} B", bytes)
    }
}

fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1000 {
        format!("{:.1}K", count as f64 / 1000.0)
    } else {
        format!("{}", count)
    }
}

fn section_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgb(
            0.12, 0.12, 0.14,
        ))),
        border: iced::Border {
            color: iced::Color::from_rgb(0.25, 0.25, 0.3),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::Protocol;

    #[test]
    fn test_protocol_name() {
        let flow = FlowRecord {
            src_ip: "10.0.0.1".to_string(),
            dst_ip: "10.0.0.2".to_string(),
            src_port: 80,
            dst_port: 443,
            protocol: 6,
            bytes: 1000,
            packets: 10,
            timestamp: 0,
        };
        assert_eq!(flow.protocol_name(), "TCP");
    }

    #[test]
    fn test_netflow_view_renders() {
        let device_id = DeviceId::new(Protocol::Netflow, "router01");
        let state = DeviceDetailState::new(device_id);
        let _view = netflow_traffic_view(&state);
    }
}
