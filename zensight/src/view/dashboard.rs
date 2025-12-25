//! Dashboard view showing all monitored devices.

use std::collections::HashMap;

use iced::widget::{Column, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::Protocol;

use crate::message::{DeviceId, Message};
use crate::view::icons::{self, IconSize};

/// State for a single device on the dashboard.
#[derive(Debug, Clone)]
pub struct DeviceState {
    /// Device identifier.
    pub id: DeviceId,
    /// Last update timestamp (Unix epoch ms).
    pub last_update: i64,
    /// Number of metrics received.
    pub metric_count: usize,
    /// Most recent metric values (metric name -> formatted value).
    pub metrics: HashMap<String, String>,
    /// Whether this device is healthy (received recent updates).
    pub is_healthy: bool,
}

impl DeviceState {
    /// Create a new device state.
    pub fn new(id: DeviceId) -> Self {
        Self {
            id,
            last_update: 0,
            metric_count: 0,
            metrics: HashMap::new(),
            is_healthy: true,
        }
    }

    /// Update health status based on last update time.
    pub fn update_health(&mut self, now: i64, stale_threshold_ms: i64) {
        self.is_healthy = (now - self.last_update) < stale_threshold_ms;
    }
}

/// Dashboard view state.
#[derive(Debug, Default)]
pub struct DashboardState {
    /// All known devices, keyed by DeviceId.
    pub devices: HashMap<DeviceId, DeviceState>,
    /// Active protocol filters (empty = show all).
    pub protocol_filters: std::collections::HashSet<Protocol>,
    /// Whether we are connected to Zenoh.
    pub connected: bool,
    /// Last error message, if any.
    pub last_error: Option<String>,
}

impl DashboardState {
    /// Get devices filtered by active protocol filters.
    pub fn filtered_devices(&self) -> Vec<&DeviceState> {
        let mut devices: Vec<_> = self
            .devices
            .values()
            .filter(|d| {
                self.protocol_filters.is_empty() || self.protocol_filters.contains(&d.id.protocol)
            })
            .collect();

        // Sort by protocol, then by source name
        devices.sort_by(|a, b| match a.id.protocol.cmp(&b.id.protocol) {
            std::cmp::Ordering::Equal => a.id.source.cmp(&b.id.source),
            other => other,
        });

        devices
    }

    /// Toggle a protocol filter.
    pub fn toggle_filter(&mut self, protocol: Protocol) {
        if self.protocol_filters.contains(&protocol) {
            self.protocol_filters.remove(&protocol);
        } else {
            self.protocol_filters.insert(protocol);
        }
    }

    /// Get all protocols that have devices.
    pub fn active_protocols(&self) -> Vec<Protocol> {
        let mut protocols: Vec<_> = self
            .devices
            .values()
            .map(|d| d.id.protocol)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        protocols.sort();
        protocols
    }
}

/// Render the dashboard view.
pub fn dashboard_view(state: &DashboardState) -> Element<'_, Message> {
    let header = render_header(state);
    let filters = render_protocol_filters(state);
    let devices = render_device_grid(state);

    let content = column![header, filters, rule::horizontal(1), devices]
        .spacing(10)
        .padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with connection status.
fn render_header(state: &DashboardState) -> Element<'_, Message> {
    let title = text("ZenSight Dashboard").size(24);

    let status_icon = if state.connected {
        icons::connected(IconSize::Medium)
    } else {
        icons::disconnected(IconSize::Medium)
    };

    let status_text = if state.connected {
        text("Connected")
            .size(14)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.2, 0.8, 0.2)),
            })
    } else {
        text("Disconnected")
            .size(14)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.8, 0.2, 0.2)),
            })
    };

    let status = row![status_icon, status_text]
        .spacing(5)
        .align_y(Alignment::Center);

    let device_count = text(format!("{} devices", state.devices.len())).size(14);

    let alerts_button = button(
        row![icons::alert(IconSize::Medium), text("Alerts").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenAlerts)
    .style(iced::widget::button::secondary);

    let settings_button = button(
        row![icons::settings(IconSize::Medium), text("Settings").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenSettings)
    .style(iced::widget::button::secondary);

    let header_row = row![title, device_count, status, alerts_button, settings_button]
        .spacing(20)
        .align_y(Alignment::Center);

    let mut header_col = Column::new().push(header_row);

    if let Some(ref error) = state.last_error {
        let error_text = text(format!("Error: {}", error))
            .size(12)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.8, 0.2, 0.2)),
            });
        header_col = header_col.push(error_text);
    }

    header_col.spacing(5).into()
}

/// Render protocol filter buttons.
fn render_protocol_filters(state: &DashboardState) -> Element<'_, Message> {
    let protocols = state.active_protocols();

    if protocols.is_empty() {
        return text("No devices yet...").size(12).into();
    }

    let filter_label = text("Filter by protocol:").size(14);

    let mut filter_row = row![filter_label].spacing(10).align_y(Alignment::Center);

    for protocol in protocols {
        let is_active =
            state.protocol_filters.is_empty() || state.protocol_filters.contains(&protocol);

        let label = format!("{}", protocol);
        let btn = button(text(label).size(12)).on_press(Message::ToggleProtocolFilter(protocol));

        let btn = if is_active {
            btn.style(iced::widget::button::primary)
        } else {
            btn.style(iced::widget::button::secondary)
        };

        filter_row = filter_row.push(btn);
    }

    filter_row.into()
}

/// Render the device grid.
fn render_device_grid(state: &DashboardState) -> Element<'_, Message> {
    let devices = state.filtered_devices();

    if devices.is_empty() {
        let message = if state.devices.is_empty() {
            "Waiting for telemetry data..."
        } else {
            "No devices match the current filters"
        };
        return container(text(message).size(16))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    }

    let mut device_list = Column::new().spacing(10);

    for device in devices {
        device_list = device_list.push(render_device_card(device));
    }

    scrollable(device_list)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render a single device card.
fn render_device_card(device: &DeviceState) -> Element<'_, Message> {
    let status_indicator = if device.is_healthy {
        icons::status_healthy(IconSize::Small)
    } else {
        icons::status_warning(IconSize::Small)
    };

    let protocol_icon = icons::protocol_icon(device.id.protocol, IconSize::Medium);
    let device_name = text(&device.id.source).size(16);
    let metric_count = text(format!("{} metrics", device.metric_count)).size(12);

    let header = row![status_indicator, protocol_icon, device_name, metric_count]
        .spacing(10)
        .align_y(Alignment::Center);

    // Show a few recent metrics as preview
    let mut preview = Column::new().spacing(2);
    for (name, value) in device.metrics.iter().take(3) {
        let metric_line = text(format!("  {} = {}", name, value)).size(11);
        preview = preview.push(metric_line);
    }

    if device.metrics.len() > 3 {
        preview =
            preview.push(text(format!("  ... and {} more", device.metrics.len() - 3)).size(11));
    }

    let card_content = column![header, preview].spacing(5);

    button(card_content)
        .on_press(Message::SelectDevice(device.id.clone()))
        .padding(10)
        .width(Length::Fill)
        .style(iced::widget::button::secondary)
        .into()
}
