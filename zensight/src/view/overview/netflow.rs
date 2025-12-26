//! NetFlow overview - aggregates traffic across all exporters.

use std::collections::HashMap;

use iced::widget::{Column, Row, column, container, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;

/// Flow record summary.
struct FlowRecord {
    src_ip: String,
    dst_ip: String,
    protocol: u8,
    bytes: u64,
    packets: u64,
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

/// Render the NetFlow traffic overview.
pub fn netflow_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return text("No NetFlow exporters available")
            .size(12)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            })
            .into();
    }

    // Collect all flows
    let flows = collect_flows(devices);

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

    // Summary row
    let summary_row = row![
        render_stat("Exporters", devices.len().to_string()),
        render_stat("Flows", flows.len().to_string()),
        render_stat("Total Traffic", format_bytes(total_bytes)),
        render_stat("Packets", format_count(total_packets)),
        render_stat("Unique Sources", unique_sources.to_string()),
        render_stat("Unique Destinations", unique_dests.to_string()),
    ]
    .spacing(25)
    .align_y(Alignment::Center);

    // Top talkers
    let top_talkers = render_top_talkers(&flows);

    // Protocol distribution
    let protocol_dist = render_protocol_distribution(&flows);

    column![summary_row, top_talkers, protocol_dist]
        .spacing(15)
        .width(Length::Fill)
        .into()
}

/// Collect all flow records from all devices.
fn collect_flows(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<FlowRecord> {
    let mut flows = Vec::new();

    for state in devices.values() {
        for point in state.metrics.values() {
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
                protocol,
                bytes,
                packets,
            });
        }
    }

    flows
}

/// Render a stat label and value.
fn render_stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        }),
        text(value).size(16)
    ]
    .spacing(2)
    .into()
}

/// Render top talkers by bytes.
fn render_top_talkers<'a>(flows: &[FlowRecord]) -> Element<'a, Message> {
    let title = text("Top Talkers (by bytes)")
        .size(12)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
        });

    // Aggregate by src -> dst pair
    let mut talkers: HashMap<(String, String), u64> = HashMap::new();
    for flow in flows {
        let key = (flow.src_ip.clone(), flow.dst_ip.clone());
        *talkers.entry(key).or_insert(0) += flow.bytes;
    }

    let mut sorted: Vec<_> = talkers.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let rows: Vec<Element<'a, Message>> = sorted
        .into_iter()
        .take(5)
        .enumerate()
        .map(|(i, ((src, dst), bytes))| render_talker_row(i + 1, src, dst, bytes))
        .collect();

    if rows.is_empty() {
        return column![title, text("No flow data").size(11)]
            .spacing(4)
            .into();
    }

    column![title, Column::with_children(rows).spacing(4)]
        .spacing(8)
        .into()
}

/// Render a single talker row.
fn render_talker_row<'a>(
    rank: usize,
    src: String,
    dst: String,
    bytes: u64,
) -> Element<'a, Message> {
    row![
        text(format!("{}.", rank))
            .size(10)
            .width(Length::Fixed(20.0)),
        text(src).size(11).width(Length::Fixed(120.0)),
        text("→").size(11).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        }),
        text(dst).size(11).width(Length::Fixed(120.0)),
        text(format_bytes(bytes))
            .size(11)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.4, 0.7, 0.9)),
            }),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// Render protocol distribution.
fn render_protocol_distribution<'a>(flows: &[FlowRecord]) -> Element<'a, Message> {
    let title = text("Protocol Distribution")
        .size(12)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
        });

    // Aggregate by protocol
    let mut by_protocol: HashMap<&'static str, u64> = HashMap::new();
    for flow in flows {
        *by_protocol.entry(flow.protocol_name()).or_insert(0) += flow.bytes;
    }

    let total: u64 = by_protocol.values().sum();

    let mut sorted: Vec<_> = by_protocol.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let colors = [
        iced::Color::from_rgb(0.3, 0.6, 0.9), // Blue - TCP
        iced::Color::from_rgb(0.4, 0.8, 0.4), // Green - UDP
        iced::Color::from_rgb(0.9, 0.5, 0.3), // Orange - ICMP
        iced::Color::from_rgb(0.7, 0.4, 0.8), // Purple - Other
    ];

    let bars: Vec<Element<'a, Message>> = sorted
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, (proto, bytes))| {
            let pct = if total > 0 {
                (*bytes as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            render_protocol_bar(proto, pct, colors[i % colors.len()])
        })
        .collect();

    if bars.is_empty() {
        return column![title, text("No protocol data").size(11)]
            .spacing(4)
            .into();
    }

    column![title, Row::with_children(bars).spacing(20)]
        .spacing(8)
        .into()
}

/// Render a protocol bar.
fn render_protocol_bar<'a>(
    protocol: &'a str,
    pct: f64,
    color: iced::Color,
) -> Element<'a, Message> {
    let bar_width = (pct * 2.0).clamp(5.0, 100.0) as f32;

    let bar = container(text(""))
        .width(Length::Fixed(bar_width))
        .height(Length::Fixed(16.0))
        .style(move |_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(color)),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    row![bar, text(format!("{} {:.0}%", protocol, pct)).size(11)]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// Format bytes as human-readable string.
fn format_bytes(bytes: u64) -> String {
    let bytes = bytes as f64;
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

/// Format count with K/M suffix.
fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1000 {
        format!("{:.1}K", count as f64 / 1000.0)
    } else {
        format!("{}", count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn test_format_count() {
        assert_eq!(format_count(500), "500");
        assert_eq!(format_count(1500), "1.5K");
        assert_eq!(format_count(1_500_000), "1.5M");
    }
}
