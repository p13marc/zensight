//! SNMP network device specialized view.
//!
//! Displays network device metrics with interface tables, status indicators,
//! and bandwidth charts.

use std::collections::HashMap;

use iced::widget::{Column, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::components::{Gauge, StatusLed, StatusLedState};
use crate::view::device::DeviceDetailState;
use crate::view::icons::{self, IconSize};

/// Parsed interface data from SNMP metrics.
#[derive(Debug, Clone)]
struct InterfaceInfo {
    index: u32,
    name: String,
    description: String,
    admin_status: InterfaceStatus,
    oper_status: InterfaceStatus,
    in_octets: u64,
    out_octets: u64,
    in_errors: u64,
    out_errors: u64,
    speed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceStatus {
    Up,
    Down,
    Testing,
    Unknown,
}

impl InterfaceStatus {
    fn from_snmp_value(val: u64) -> Self {
        match val {
            1 => InterfaceStatus::Up,
            2 => InterfaceStatus::Down,
            3 => InterfaceStatus::Testing,
            _ => InterfaceStatus::Unknown,
        }
    }

    fn to_led_state(self) -> StatusLedState {
        match self {
            InterfaceStatus::Up => StatusLedState::Active,
            InterfaceStatus::Down => StatusLedState::Inactive,
            InterfaceStatus::Testing => StatusLedState::Warning,
            InterfaceStatus::Unknown => StatusLedState::Unknown,
        }
    }
}

/// Render the SNMP network device specialized view.
pub fn snmp_device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let system_info = render_system_info(state);
    let interfaces = render_interface_table(state);
    let system_metrics = render_system_metrics(state);

    let content = column![
        header,
        rule::horizontal(1),
        system_info,
        rule::horizontal(1),
        interfaces,
        rule::horizontal(1),
        system_metrics,
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and device info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let device_name = text(&state.device_id.source).size(24);

    // Try to get sysName
    let sys_name =
        get_metric_text(state, "system/sysName").unwrap_or_else(|| "Unknown Device".to_string());

    let sys_name_text = text(sys_name).size(14).style(|_theme: &Theme| text::Style {
        color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
    });

    // Health status based on sysUpTime presence
    let status = if state.metrics.contains_key("system/sysUpTime") {
        StatusLed::new(StatusLedState::Active).with_label("Healthy")
    } else {
        StatusLed::new(StatusLedState::Warning).with_label("Limited")
    };

    let metric_count = text(format!("{} metrics", state.metrics.len())).size(14);

    row![
        back_button,
        protocol_icon,
        device_name,
        sys_name_text,
        status.view(),
        metric_count
    ]
    .spacing(15)
    .align_y(Alignment::Center)
    .into()
}

/// Render system information section.
fn render_system_info(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut info_items: Vec<Element<'_, Message>> = Vec::new();

    // sysDescr
    if let Some(desc) = get_metric_text(state, "system/sysDescr") {
        let short_desc = if desc.len() > 60 {
            format!("{}...", &desc[..57])
        } else {
            desc
        };
        info_items.push(
            row![text("Description:").size(12), text(short_desc).size(12)]
                .spacing(8)
                .into(),
        );
    }

    // sysUpTime
    if let Some(uptime) = get_metric_value(state, "system/sysUpTime") {
        // SNMP uptime is in centiseconds
        let secs = (uptime / 100.0) as u64;
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        let mins = (secs % 3600) / 60;
        let uptime_str = format!("{}d {}h {}m", days, hours, mins);

        info_items.push(
            row![
                text("Uptime:").size(12),
                text(uptime_str)
                    .size(12)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(iced::Color::from_rgb(0.4, 0.8, 0.4)),
                    })
            ]
            .spacing(8)
            .into(),
        );
    }

    // sysContact
    if let Some(contact) = get_metric_text(state, "system/sysContact")
        && !contact.is_empty() {
            info_items.push(
                row![text("Contact:").size(12), text(contact).size(12)]
                    .spacing(8)
                    .into(),
            );
        }

    // sysLocation
    if let Some(location) = get_metric_text(state, "system/sysLocation")
        && !location.is_empty() {
            info_items.push(
                row![text("Location:").size(12), text(location).size(12)]
                    .spacing(8)
                    .into(),
            );
        }

    if info_items.is_empty() {
        info_items.push(
            text("Waiting for system information...")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                })
                .into(),
        );
    }

    container(Column::with_children(info_items).spacing(8))
        .padding(10)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render the interface table.
fn render_interface_table(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::network(IconSize::Medium),
        text("Interfaces").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let interfaces = parse_interfaces(state);

    if interfaces.is_empty() {
        return column![
            title,
            text("No interface data available")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                })
        ]
        .spacing(10)
        .into();
    }

    // Table header
    let header = container(
        row![
            text("Status").size(11).width(Length::Fixed(60.0)),
            text("Name").size(11).width(Length::Fixed(100.0)),
            text("Description").size(11).width(Length::Fill),
            text("In").size(11).width(Length::Fixed(80.0)),
            text("Out").size(11).width(Length::Fixed(80.0)),
            text("Errors").size(11).width(Length::Fixed(60.0)),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding(8)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgb(
            0.18, 0.18, 0.2,
        ))),
        ..Default::default()
    });

    // Table rows
    let mut table_rows = Column::new().spacing(2);

    for iface in interfaces {
        let status_led = StatusLed::new(iface.oper_status.to_led_state()).with_state_text();

        let name = if iface.name.is_empty() {
            format!("if{}", iface.index)
        } else {
            iface.name.clone()
        };

        let desc = if iface.description.len() > 30 {
            format!("{}...", &iface.description[..27])
        } else {
            iface.description.clone()
        };

        let in_str = format_bytes(iface.in_octets as f64);
        let out_str = format_bytes(iface.out_octets as f64);
        let errors = iface.in_errors + iface.out_errors;
        let errors_str = if errors > 0 {
            format!("{}", errors)
        } else {
            "-".to_string()
        };

        let errors_style = if errors > 0 {
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.9, 0.3, 0.3)),
            }
        } else {
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            }
        };

        let row_content = row![
            container(status_led.view()).width(Length::Fixed(60.0)),
            text(name).size(11).width(Length::Fixed(100.0)),
            text(desc).size(11).width(Length::Fill),
            text(in_str).size(11).width(Length::Fixed(80.0)),
            text(out_str).size(11).width(Length::Fixed(80.0)),
            text(errors_str)
                .size(11)
                .width(Length::Fixed(60.0))
                .style(errors_style),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let row_container =
            container(row_content)
                .padding(8)
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

    column![title, header, table_rows].spacing(10).into()
}

/// Render system metrics section (CPU, memory if available).
fn render_system_metrics(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::chart(IconSize::Medium),
        text("System Metrics").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut metrics_content = Column::new().spacing(10);
    let mut has_metrics = false;

    // CPU usage (common OIDs like hrProcessorLoad)
    if let Some(cpu) = get_metric_value(state, "host/hrProcessorLoad") {
        let gauge = Gauge::percentage(cpu, "CPU").with_width(200.0);
        metrics_content = metrics_content.push(gauge.view());
        has_metrics = true;
    }

    // Memory (hrStorageUsed/hrStorageSize for RAM type)
    if let Some(mem_used) = get_metric_value(state, "host/hrStorageUsed")
        && let Some(mem_total) = get_metric_value(state, "host/hrStorageSize") {
            let pct = if mem_total > 0.0 {
                (mem_used / mem_total) * 100.0
            } else {
                0.0
            };
            let gauge = Gauge::percentage(pct, "Memory").with_width(200.0);
            metrics_content = metrics_content.push(gauge.view());
            has_metrics = true;
        }

    // Temperature sensors
    let temp_metrics: Vec<_> = state
        .metrics
        .iter()
        .filter(|(k, _)| k.contains("temp") || k.contains("Temperature"))
        .collect();

    for (name, point) in temp_metrics {
        if let TelemetryValue::Gauge(temp) = &point.value {
            let short_name = name.split('/').next_back().unwrap_or(name);
            metrics_content = metrics_content.push(
                row![
                    text(format!("{}:", short_name)).size(12),
                    text(format!("{:.1}°C", temp)).size(12)
                ]
                .spacing(10),
            );
            has_metrics = true;
        }
    }

    if !has_metrics {
        metrics_content = metrics_content.push(text("No system metrics available").size(12).style(
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            },
        ));
    }

    column![title, metrics_content].spacing(10).into()
}

/// Parse interface data from SNMP metrics.
fn parse_interfaces(state: &DeviceDetailState) -> Vec<InterfaceInfo> {
    let mut interfaces: HashMap<u32, InterfaceInfo> = HashMap::new();

    // Look for interface metrics with pattern if/<index>/<metric>
    for (key, point) in &state.metrics {
        if !key.starts_with("if/") {
            continue;
        }

        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() < 3 {
            continue;
        }

        let index: u32 = match parts[1].parse() {
            Ok(i) => i,
            Err(_) => continue,
        };

        let iface = interfaces.entry(index).or_insert_with(|| InterfaceInfo {
            index,
            name: String::new(),
            description: String::new(),
            admin_status: InterfaceStatus::Unknown,
            oper_status: InterfaceStatus::Unknown,
            in_octets: 0,
            out_octets: 0,
            in_errors: 0,
            out_errors: 0,
            speed: 0,
        });

        let metric = parts[2];
        match metric {
            "ifDescr" => {
                if let TelemetryValue::Text(s) = &point.value {
                    iface.description = s.clone();
                }
            }
            "ifName" => {
                if let TelemetryValue::Text(s) = &point.value {
                    iface.name = s.clone();
                }
            }
            "ifAdminStatus" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.admin_status = InterfaceStatus::from_snmp_value(v);
                }
            }
            "ifOperStatus" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.oper_status = InterfaceStatus::from_snmp_value(v);
                }
            }
            "ifInOctets" | "ifHCInOctets" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.in_octets = v;
                }
            }
            "ifOutOctets" | "ifHCOutOctets" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.out_octets = v;
                }
            }
            "ifInErrors" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.in_errors = v;
                }
            }
            "ifOutErrors" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.out_errors = v;
                }
            }
            "ifSpeed" | "ifHighSpeed" => {
                if let Some(v) = value_to_u64(&point.value) {
                    iface.speed = v;
                }
            }
            _ => {}
        }
    }

    let mut result: Vec<_> = interfaces.into_values().collect();
    result.sort_by_key(|i| i.index);
    result
}

// Helper functions

fn get_metric_value(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Counter(v) => Some(*v as f64),
            TelemetryValue::Gauge(v) => Some(*v),
            _ => None,
        })
}

fn get_metric_text(state: &DeviceDetailState, metric: &str) -> Option<String> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Text(s) => Some(s.clone()),
            _ => None,
        })
}

fn value_to_u64(value: &TelemetryValue) -> Option<u64> {
    match value {
        TelemetryValue::Counter(v) => Some(*v),
        TelemetryValue::Gauge(v) => Some(*v as u64),
        _ => None,
    }
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1_073_741_824.0 {
        format!("{:.1}G", bytes / 1_073_741_824.0)
    } else if bytes >= 1_048_576.0 {
        format!("{:.1}M", bytes / 1_048_576.0)
    } else if bytes >= 1024.0 {
        format!("{:.1}K", bytes / 1024.0)
    } else {
        format!("{:.0}", bytes)
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
    fn test_interface_status_from_snmp() {
        assert_eq!(InterfaceStatus::from_snmp_value(1), InterfaceStatus::Up);
        assert_eq!(InterfaceStatus::from_snmp_value(2), InterfaceStatus::Down);
        assert_eq!(
            InterfaceStatus::from_snmp_value(99),
            InterfaceStatus::Unknown
        );
    }

    #[test]
    fn test_snmp_view_renders() {
        let device_id = DeviceId::new(Protocol::Snmp, "router01");
        let state = DeviceDetailState::new(device_id);
        let _view = snmp_device_view(&state);
    }
}
