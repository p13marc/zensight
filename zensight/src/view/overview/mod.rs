//! Protocol-specific overview sections for the dashboard.
//!
//! These sections aggregate metrics across all devices of each protocol type,
//! providing at-a-glance insights before diving into individual devices.

pub mod gnmi;
pub mod modbus;
pub mod netflow;
pub mod netlink;
pub mod netring;
pub mod snmp;
pub mod sysinfo;
pub mod syslog;

use std::collections::HashMap;

use iced::widget::{Row, column, container, row, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::Protocol;

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::icons::{self, IconSize};
use crate::view::theme;

/// State for the overview section.
#[derive(Debug, Clone)]
pub struct OverviewState {
    /// Which protocol overview is currently selected (None = collapsed).
    pub selected_protocol: Option<Protocol>,
    /// Whether the overview section is expanded.
    pub expanded: bool,
}

impl Default for OverviewState {
    fn default() -> Self {
        Self {
            selected_protocol: None,
            expanded: true,
        }
    }
}

impl OverviewState {
    /// Select a protocol for overview.
    pub fn select_protocol(&mut self, protocol: Protocol) {
        if self.selected_protocol == Some(protocol) {
            // Toggle off if already selected
            self.selected_protocol = None;
        } else {
            self.selected_protocol = Some(protocol);
            self.expanded = true;
        }
    }

    /// Toggle the expanded state.
    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }
}

/// Render the overview section.
pub fn overview_section<'a>(
    state: &'a OverviewState,
    devices: &'a HashMap<DeviceId, DeviceState>,
) -> Element<'a, Message> {
    // Count devices by protocol
    let protocol_counts = count_devices_by_protocol(devices);

    // Only show protocols that have devices
    let available_protocols: Vec<Protocol> = protocol_counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(proto, _)| *proto)
        .collect();

    if available_protocols.is_empty() {
        return column![].into();
    }

    // Header with expand/collapse toggle
    let toggle_icon = if state.expanded {
        icons::arrow_down(IconSize::Small)
    } else {
        icons::arrow_right(IconSize::Small)
    };

    let header_btn = button(
        row![toggle_icon, text("Protocol Overviews").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleOverviewExpanded)
    .style(iced::widget::button::text);

    if !state.expanded {
        return container(header_btn).width(Length::Fill).into();
    }

    // Protocol tabs
    let tabs = render_protocol_tabs(state, &protocol_counts);

    // Selected protocol content
    let content: Element<'a, Message> = if let Some(protocol) = state.selected_protocol {
        let protocol_devices: HashMap<&DeviceId, &DeviceState> = devices
            .iter()
            .filter(|(id, _)| id.protocol == protocol)
            .collect();

        match protocol {
            Protocol::Snmp => snmp::snmp_overview(&protocol_devices),
            Protocol::Sysinfo => sysinfo::sysinfo_overview(&protocol_devices),
            Protocol::Logs => syslog::syslog_overview(&protocol_devices),
            Protocol::Netflow => netflow::netflow_overview(&protocol_devices),
            Protocol::Modbus => modbus::modbus_overview(&protocol_devices),
            Protocol::Gnmi => gnmi::gnmi_overview(&protocol_devices),
            Protocol::Netlink => netlink::netlink_overview(&protocol_devices),
            Protocol::Netring => netring::netring_overview(&protocol_devices),
            Protocol::Opcua => generic_overview(&protocol_devices, "OPC-UA nodes"),
        }
    } else {
        text("Select a protocol tab to view aggregated metrics")
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into()
    };

    let content_container =
        container(content)
            .padding(15)
            .width(Length::Fill)
            .style(|t: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::colors(t).card_background())),
                border: iced::Border {
                    color: theme::colors(t).border(),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            });

    column![header_btn, tabs, content_container]
        .spacing(8)
        .width(Length::Fill)
        .into()
}

/// Render the protocol tabs.
fn render_protocol_tabs<'a>(
    state: &'a OverviewState,
    counts: &HashMap<Protocol, usize>,
) -> Element<'a, Message> {
    let protocols = [
        Protocol::Sysinfo,
        Protocol::Snmp,
        Protocol::Logs,
        Protocol::Netflow,
        Protocol::Modbus,
        Protocol::Gnmi,
        Protocol::Netlink,
        Protocol::Netring,
        Protocol::Opcua,
    ];

    let tabs: Vec<Element<'a, Message>> = protocols
        .iter()
        .filter(|proto| counts.get(proto).copied().unwrap_or(0) > 0)
        .map(|&proto| {
            let count = counts.get(&proto).copied().unwrap_or(0);
            let is_selected = state.selected_protocol == Some(proto);

            let icon = icons::protocol_icon(proto, IconSize::Small);
            let label = text(format!("{} ({})", protocol_short_name(proto), count)).size(12);

            let btn = button(row![icon, label].spacing(6).align_y(Alignment::Center))
                .on_press(Message::SelectOverviewProtocol(proto))
                .padding([6, 12])
                .style(if is_selected {
                    iced::widget::button::primary
                } else {
                    iced::widget::button::secondary
                });

            btn.into()
        })
        .collect();

    Row::with_children(tabs)
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

/// A minimal count-and-health overview for protocols without richer aggregates
/// (e.g. OPC-UA, whose specialized telemetry isn't modelled yet).
fn generic_overview<'a>(
    devices: &HashMap<&DeviceId, &DeviceState>,
    noun: &'a str,
) -> Element<'a, Message> {
    if devices.is_empty() {
        return text(format!("No {noun} available"))
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into();
    }
    let healthy = devices.values().filter(|d| d.is_healthy).count();
    let metrics: usize = devices.values().map(|d| d.metric_count).sum();
    row![
        column![
            text("Devices").size(10).style(muted),
            text(devices.len().to_string()).size(16)
        ]
        .spacing(2),
        column![
            text("Online").size(10).style(muted),
            text(healthy.to_string()).size(16)
        ]
        .spacing(2),
        column![
            text("Metrics").size(10).style(muted),
            text(metrics.to_string()).size(16)
        ]
        .spacing(2),
    ]
    .spacing(25)
    .align_y(Alignment::Center)
    .into()
}

fn muted(t: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(t).text_muted()),
    }
}

/// Count devices by protocol.
fn count_devices_by_protocol(devices: &HashMap<DeviceId, DeviceState>) -> HashMap<Protocol, usize> {
    let mut counts = HashMap::new();
    for device_id in devices.keys() {
        *counts.entry(device_id.protocol).or_insert(0) += 1;
    }
    counts
}

/// Get a short display name for a protocol.
fn protocol_short_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Snmp => "SNMP",
        Protocol::Sysinfo => "Sysinfo",
        Protocol::Logs => "Logs",
        Protocol::Netflow => "NetFlow",
        Protocol::Modbus => "Modbus",
        Protocol::Gnmi => "gNMI",
        Protocol::Opcua => "OPC-UA",
        Protocol::Netlink => "Netlink",
        Protocol::Netring => "Netring",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overview_state_default() {
        let state = OverviewState::default();
        assert!(state.selected_protocol.is_none());
        assert!(state.expanded);
    }

    #[test]
    fn test_select_protocol_toggle() {
        let mut state = OverviewState::default();

        state.select_protocol(Protocol::Snmp);
        assert_eq!(state.selected_protocol, Some(Protocol::Snmp));

        // Selecting same protocol toggles off
        state.select_protocol(Protocol::Snmp);
        assert_eq!(state.selected_protocol, None);
    }

    #[test]
    fn test_toggle_expanded() {
        let mut state = OverviewState::default();
        assert!(state.expanded);

        state.toggle_expanded();
        assert!(!state.expanded);

        state.toggle_expanded();
        assert!(state.expanded);
    }
}
