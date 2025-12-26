//! SNMP network overview - aggregates interface status and traffic across all devices.

use std::collections::HashMap;

use iced::widget::{Column, column, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;
use crate::view::theme;

/// Interface summary data.
#[derive(Debug)]
struct InterfaceSummary {
    device: String,
    name: String,
    is_up: bool,
    in_octets: u64,
    out_octets: u64,
    errors: u64,
}

/// Render the SNMP network overview.
pub fn snmp_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return text("No SNMP devices available")
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into();
    }

    // Collect all interfaces across all devices
    let interfaces = collect_interfaces(devices);

    let total_interfaces = interfaces.len();
    let up_count = interfaces.iter().filter(|i| i.is_up).count();
    let down_count = total_interfaces - up_count;
    let error_count = interfaces.iter().filter(|i| i.errors > 0).count();

    // Total traffic
    let total_in: u64 = interfaces.iter().map(|i| i.in_octets).sum();
    let total_out: u64 = interfaces.iter().map(|i| i.out_octets).sum();

    // Summary row
    let summary_row = row![
        render_stat("Devices", devices.len().to_string()),
        render_stat("Interfaces", total_interfaces.to_string()),
        render_status_stat("UP", up_count, StatusLedState::Active),
        render_status_stat("DOWN", down_count, StatusLedState::Inactive),
        render_stat("With Errors", error_count.to_string()),
        render_stat("Total In", format_bytes(total_in)),
        render_stat("Total Out", format_bytes(total_out)),
    ]
    .spacing(25)
    .align_y(Alignment::Center);

    // Top interfaces by traffic
    let top_interfaces = render_top_interfaces(&interfaces);

    // Error hotspots
    let error_interfaces = render_error_interfaces(&interfaces);

    column![summary_row, top_interfaces, error_interfaces]
        .spacing(15)
        .width(Length::Fill)
        .into()
}

/// Collect all interfaces from all devices.
fn collect_interfaces(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<InterfaceSummary> {
    let mut interfaces = Vec::new();

    for (device_id, state) in devices {
        // Find all interface indices
        let mut if_indices: Vec<u32> = state
            .metrics
            .keys()
            .filter_map(|k| {
                if k.starts_with("if/") {
                    let parts: Vec<&str> = k.split('/').collect();
                    if parts.len() >= 2 {
                        parts[1].parse().ok()
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        if_indices.sort();
        if_indices.dedup();

        for idx in if_indices {
            let name = get_text_metric(state, &format!("if/{}/ifName", idx))
                .or_else(|| get_text_metric(state, &format!("if/{}/ifDescr", idx)))
                .unwrap_or_else(|| format!("if{}", idx));

            let oper_status = get_numeric_metric(state, &format!("if/{}/ifOperStatus", idx));
            let is_up = oper_status.map(|v| v == 1.0).unwrap_or(false);

            let in_octets = get_numeric_metric(state, &format!("if/{}/ifInOctets", idx))
                .or_else(|| get_numeric_metric(state, &format!("if/{}/ifHCInOctets", idx)))
                .unwrap_or(0.0) as u64;

            let out_octets = get_numeric_metric(state, &format!("if/{}/ifOutOctets", idx))
                .or_else(|| get_numeric_metric(state, &format!("if/{}/ifHCOutOctets", idx)))
                .unwrap_or(0.0) as u64;

            let in_errors =
                get_numeric_metric(state, &format!("if/{}/ifInErrors", idx)).unwrap_or(0.0) as u64;
            let out_errors =
                get_numeric_metric(state, &format!("if/{}/ifOutErrors", idx)).unwrap_or(0.0) as u64;

            interfaces.push(InterfaceSummary {
                device: device_id.source.clone(),
                name,
                is_up,
                in_octets,
                out_octets,
                errors: in_errors + out_errors,
            });
        }
    }

    interfaces
}

/// Get a numeric metric from device state.
fn get_numeric_metric(state: &DeviceState, metric: &str) -> Option<f64> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Counter(v) => Some(*v as f64),
            TelemetryValue::Gauge(v) => Some(*v),
            _ => None,
        })
}

/// Get a text metric from device state.
fn get_text_metric(state: &DeviceState, metric: &str) -> Option<String> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Text(s) => Some(s.clone()),
            _ => None,
        })
}

/// Render a stat label and value.
fn render_stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        text(value).size(16)
    ]
    .spacing(2)
    .into()
}

/// Render a status stat with LED.
fn render_status_stat<'a>(
    label: &'a str,
    count: usize,
    state: StatusLedState,
) -> Element<'a, Message> {
    let led = StatusLed::new(state).with_size(10.0);

    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        row![led.view(), text(count.to_string()).size(16)]
            .spacing(6)
            .align_y(Alignment::Center)
    ]
    .spacing(2)
    .into()
}

/// Render top interfaces by traffic.
fn render_top_interfaces<'a>(interfaces: &[InterfaceSummary]) -> Element<'a, Message> {
    let title = text("Top Interfaces by Traffic")
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        });

    // Sort by total traffic
    let mut sorted: Vec<&InterfaceSummary> = interfaces.iter().collect();
    sorted.sort_by(|a, b| {
        let a_total = a.in_octets + a.out_octets;
        let b_total = b.in_octets + b.out_octets;
        b_total.cmp(&a_total)
    });

    let rows: Vec<Element<'a, Message>> = sorted
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, iface)| render_interface_row(i + 1, iface))
        .collect();

    if rows.is_empty() {
        return column![title, text("No interface data").size(11)]
            .spacing(4)
            .into();
    }

    column![title, Column::with_children(rows).spacing(4)]
        .spacing(8)
        .into()
}

/// Render error interfaces.
fn render_error_interfaces<'a>(interfaces: &[InterfaceSummary]) -> Element<'a, Message> {
    let error_ifaces: Vec<&InterfaceSummary> = interfaces.iter().filter(|i| i.errors > 0).collect();

    if error_ifaces.is_empty() {
        return text("No interface errors")
            .size(11)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).success()),
            })
            .into();
    }

    let title = text(format!("Interfaces with Errors ({})", error_ifaces.len()))
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).warning()),
        });

    let rows: Vec<Element<'a, Message>> = error_ifaces
        .iter()
        .take(5)
        .map(|iface| {
            row![
                text(format!("{}/{}", iface.device, iface.name)).size(11),
                text(format!("{} errors", iface.errors))
                    .size(11)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).danger()),
                    })
            ]
            .spacing(15)
            .into()
        })
        .collect();

    column![title, Column::with_children(rows).spacing(2)]
        .spacing(8)
        .into()
}

/// Render a single interface row.
fn render_interface_row<'a>(rank: usize, iface: &InterfaceSummary) -> Element<'a, Message> {
    let status = StatusLed::new(if iface.is_up {
        StatusLedState::Active
    } else {
        StatusLedState::Inactive
    })
    .with_size(8.0);

    let total = iface.in_octets + iface.out_octets;

    row![
        text(format!("{}.", rank))
            .size(10)
            .width(Length::Fixed(20.0)),
        status.view(),
        text(format!("{}/{}", iface.device, iface.name))
            .size(11)
            .width(Length::Fixed(180.0)),
        text(format!("In: {}", format_bytes(iface.in_octets))).size(10),
        text(format!("Out: {}", format_bytes(iface.out_octets))).size(10),
        text(format!("Total: {}", format_bytes(total)))
            .size(10)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).primary()),
            }),
    ]
    .spacing(10)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
        assert_eq!(format_bytes(1_610_612_736), "1.5 GB");
    }
}
