//! Dashboard view showing all monitored devices.

use std::collections::HashMap;

use iced::widget::{Column, column, container, row, rule, scrollable, text, text_input, tooltip};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// Get the current timestamp in milliseconds.
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

use crate::app::{AppTheme, DASHBOARD_SEARCH_ID};
use crate::message::{DeviceId, Message};
use crate::view::groups::{GroupTag, GroupsState, device_group_tags, group_filter_bar};
use crate::view::icons::{self, IconSize};
use crate::view::overview::{OverviewState, overview_section};

/// State for a single device on the dashboard.
#[derive(Debug, Clone)]
pub struct DeviceState {
    /// Device identifier.
    pub id: DeviceId,
    /// Last update timestamp (Unix epoch ms).
    pub last_update: i64,
    /// Number of metrics received.
    pub metric_count: usize,
    /// Most recent metric values (metric name -> full telemetry point).
    pub metrics: HashMap<String, TelemetryPoint>,
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

/// Default number of devices per page.
pub const DEFAULT_DEVICES_PER_PAGE: usize = 20;

/// Debounce delay for search input in milliseconds.
pub const SEARCH_DEBOUNCE_MS: i64 = 300;

/// Dashboard view state.
#[derive(Debug)]
pub struct DashboardState {
    /// All known devices, keyed by DeviceId.
    pub devices: HashMap<DeviceId, DeviceState>,
    /// Active protocol filters (empty = show all).
    pub protocol_filters: std::collections::HashSet<Protocol>,
    /// Search filter for device names (applied after debounce).
    pub search_filter: String,
    /// Pending search filter (user input, not yet applied).
    pub pending_search: String,
    /// Timestamp when pending search was last updated.
    pub pending_search_time: i64,
    /// Whether we are connected to Zenoh.
    pub connected: bool,
    /// Last error message, if any.
    pub last_error: Option<String>,
    /// Current page number (0-indexed).
    pub current_page: usize,
    /// Number of devices per page.
    pub devices_per_page: usize,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            devices: HashMap::new(),
            protocol_filters: std::collections::HashSet::new(),
            search_filter: String::new(),
            pending_search: String::new(),
            pending_search_time: 0,
            connected: false,
            last_error: None,
            current_page: 0,
            devices_per_page: DEFAULT_DEVICES_PER_PAGE,
        }
    }
}

impl DashboardState {
    /// Get devices filtered by active protocol filters and search term.
    pub fn filtered_devices(&self) -> Vec<&DeviceState> {
        let search_lower = self.search_filter.to_lowercase();

        let mut devices: Vec<_> = self
            .devices
            .values()
            .filter(|d| {
                // Protocol filter
                let protocol_match = self.protocol_filters.is_empty()
                    || self.protocol_filters.contains(&d.id.protocol);

                // Search filter (case-insensitive match on device source name)
                let search_match =
                    search_lower.is_empty() || d.id.source.to_lowercase().contains(&search_lower);

                protocol_match && search_match
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

    /// Set the pending search filter (debounced).
    /// The actual filter is applied after SEARCH_DEBOUNCE_MS via `apply_pending_search`.
    pub fn set_search_filter(&mut self, filter: String) {
        self.pending_search = filter;
        self.pending_search_time = current_timestamp();
    }

    /// Apply the pending search filter if debounce delay has passed.
    /// Returns true if the filter was applied.
    pub fn apply_pending_search(&mut self) -> bool {
        if self.pending_search != self.search_filter {
            let elapsed = current_timestamp() - self.pending_search_time;
            if elapsed >= SEARCH_DEBOUNCE_MS {
                self.search_filter = self.pending_search.clone();
                self.current_page = 0;
                return true;
            }
        }
        false
    }

    /// Get the current search input (pending, for display in text field).
    pub fn search_input(&self) -> &str {
        &self.pending_search
    }

    /// Get the total number of pages.
    pub fn total_pages(&self) -> usize {
        let filtered_count = self.filtered_devices().len();
        if filtered_count == 0 {
            1
        } else {
            filtered_count.div_ceil(self.devices_per_page)
        }
    }

    /// Go to the next page.
    pub fn next_page(&mut self) {
        let total = self.total_pages();
        if self.current_page + 1 < total {
            self.current_page += 1;
        }
    }

    /// Go to the previous page.
    pub fn prev_page(&mut self) {
        if self.current_page > 0 {
            self.current_page -= 1;
        }
    }

    /// Go to a specific page.
    pub fn go_to_page(&mut self, page: usize) {
        let total = self.total_pages();
        self.current_page = page.min(total.saturating_sub(1));
    }

    /// Get devices for the current page.
    pub fn paginated_devices(&self) -> Vec<&DeviceState> {
        let all = self.filtered_devices();
        let start = self.current_page * self.devices_per_page;
        let end = (start + self.devices_per_page).min(all.len());

        if start >= all.len() {
            vec![]
        } else {
            all[start..end].to_vec()
        }
    }

    /// Set devices per page.
    pub fn set_devices_per_page(&mut self, count: usize) {
        self.devices_per_page = count.max(5); // Minimum 5 per page
        // Reset to first page to avoid invalid page
        self.current_page = 0;
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
pub fn dashboard_view<'a>(
    state: &'a DashboardState,
    theme: AppTheme,
    unacknowledged_alerts: usize,
    groups: &'a GroupsState,
    overview: &'a OverviewState,
) -> Element<'a, Message> {
    let header = render_header(state, theme, unacknowledged_alerts);
    let filters = render_protocol_filters(state);
    let group_filters = group_filter_bar(groups);
    let overview_panel = overview_section(overview, &state.devices);
    let devices = render_device_grid(state, groups);

    let content = column![
        header,
        filters,
        group_filters,
        overview_panel,
        rule::horizontal(1),
        devices
    ]
    .spacing(10)
    .padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with connection status.
fn render_header(
    state: &DashboardState,
    theme: AppTheme,
    unacknowledged_alerts: usize,
) -> Element<'_, Message> {
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

    // Theme toggle button - show moon when dark (click to go light), sun when light (click to go dark)
    let theme_icon = match theme {
        AppTheme::Dark => icons::moon(IconSize::Medium),
        AppTheme::Light => icons::sun(IconSize::Medium),
    };
    let theme_button = button(theme_icon)
        .on_press(Message::ToggleTheme)
        .style(iced::widget::button::secondary);

    let alerts_label = if unacknowledged_alerts > 0 {
        // White text for contrast on danger (red) button background
        text(format!("Alerts ({})", unacknowledged_alerts))
            .size(14)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::WHITE),
            })
    } else {
        text("Alerts").size(14)
    };

    let alerts_button = button(
        row![icons::alert(IconSize::Medium), alerts_label]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenAlerts)
    .style(if unacknowledged_alerts > 0 {
        iced::widget::button::danger
    } else {
        iced::widget::button::secondary
    });

    let topology_button = button(
        row![icons::network(IconSize::Medium), text("Topology").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenTopology)
    .style(iced::widget::button::secondary);

    let settings_button = button(
        row![icons::settings(IconSize::Medium), text("Settings").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenSettings)
    .style(iced::widget::button::secondary);

    let header_row = row![
        title,
        device_count,
        status,
        theme_button,
        alerts_button,
        topology_button,
        settings_button
    ]
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

/// Render protocol filter buttons and search input.
fn render_protocol_filters(state: &DashboardState) -> Element<'_, Message> {
    let protocols = state.active_protocols();

    if protocols.is_empty() {
        return text("No devices yet...").size(12).into();
    }

    // Protocol filter buttons
    let filter_label = text("Filter:").size(14);

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

    // Device search input (with ID for keyboard focus)
    let search_input = text_input("Search devices... (Ctrl+F)", state.search_input())
        .id(DASHBOARD_SEARCH_ID.clone())
        .on_input(Message::SetDeviceSearchFilter)
        .padding(6)
        .width(Length::Fixed(200.0));

    let search_row = row![icons::search(IconSize::Small), search_input]
        .spacing(6)
        .align_y(Alignment::Center);

    // Device count
    let filtered_count = state.filtered_devices().len();
    let total_count = state.devices.len();
    let count_text = if filtered_count == total_count {
        text(format!("{} devices", total_count)).size(12)
    } else {
        text(format!("{} of {} devices", filtered_count, total_count)).size(12)
    };

    row![filter_row, search_row, count_text]
        .spacing(20)
        .align_y(Alignment::Center)
        .into()
}

/// Render the device grid with pagination.
fn render_device_grid<'a>(
    state: &'a DashboardState,
    groups: &'a GroupsState,
) -> Element<'a, Message> {
    // Filter devices by both protocol/search and group filters
    let all_devices: Vec<_> = state
        .filtered_devices()
        .into_iter()
        .filter(|d| groups.device_passes_filter(&d.id))
        .collect();

    if all_devices.is_empty() {
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

    // Paginate the filtered devices
    let start = state.current_page * state.devices_per_page;
    let end = (start + state.devices_per_page).min(all_devices.len());
    let devices = if start < all_devices.len() {
        &all_devices[start..end]
    } else {
        &[]
    };

    let mut device_list = Column::new().spacing(10);

    for device in devices {
        device_list = device_list.push(render_device_card(device, groups));
    }

    // Add pagination controls if there are multiple pages
    let total_pages = if all_devices.is_empty() {
        1
    } else {
        all_devices.len().div_ceil(state.devices_per_page)
    };

    if total_pages > 1 {
        let pagination = render_pagination_controls_with_count(
            state.current_page,
            total_pages,
            all_devices.len(),
            state.devices_per_page,
        );
        device_list = device_list.push(pagination);
    }

    scrollable(device_list)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render pagination controls with explicit count (for group-filtered lists).
fn render_pagination_controls_with_count(
    current_page: usize,
    total_pages: usize,
    filtered_count: usize,
    devices_per_page: usize,
) -> Element<'static, Message> {
    // Previous button
    let prev_btn = if current_page > 0 {
        button(text("<").size(14))
            .on_press(Message::PrevPage)
            .style(iced::widget::button::secondary)
    } else {
        button(text("<").size(14)).style(iced::widget::button::secondary)
    };

    // Next button
    let next_btn = if current_page + 1 < total_pages {
        button(text(">").size(14))
            .on_press(Message::NextPage)
            .style(iced::widget::button::secondary)
    } else {
        button(text(">").size(14)).style(iced::widget::button::secondary)
    };

    // Page numbers
    let mut page_row = row![prev_btn].spacing(5).align_y(Alignment::Center);

    let pages_to_show = calculate_visible_pages(current_page, total_pages);
    let mut last_shown: Option<usize> = None;

    for page in pages_to_show {
        if let Some(last) = last_shown
            && page > last + 1
        {
            page_row = page_row.push(text("...").size(14));
        }

        let page_btn = if page == current_page {
            button(text(format!("{}", page + 1)).size(14)).style(iced::widget::button::primary)
        } else {
            button(text(format!("{}", page + 1)).size(14))
                .on_press(Message::GoToPage(page))
                .style(iced::widget::button::secondary)
        };
        page_row = page_row.push(page_btn);
        last_shown = Some(page);
    }

    page_row = page_row.push(next_btn);

    // Page info
    let start = current_page * devices_per_page + 1;
    let end = ((current_page + 1) * devices_per_page).min(filtered_count);
    let info = text(format!("Showing {}-{} of {}", start, end, filtered_count)).size(12);

    row![page_row, info]
        .spacing(20)
        .align_y(Alignment::Center)
        .padding(10)
        .into()
}

/// Calculate which page numbers to display.
fn calculate_visible_pages(current: usize, total: usize) -> Vec<usize> {
    if total <= 7 {
        // Show all pages
        (0..total).collect()
    } else {
        // Show first, last, and pages around current
        let mut pages = Vec::new();

        // Always show first page
        pages.push(0);

        // Calculate range around current page
        let start = current.saturating_sub(2).max(1);
        let end = (current + 3).min(total - 1);

        for page in start..end {
            if !pages.contains(&page) {
                pages.push(page);
            }
        }

        // Always show last page
        if !pages.contains(&(total - 1)) {
            pages.push(total - 1);
        }

        pages.sort();
        pages
    }
}

/// Maximum length for displayed metric values before truncation.
const MAX_VALUE_DISPLAY_LEN: usize = 30;

/// Format a telemetry value for display.
fn format_telemetry_value(value: &TelemetryValue) -> String {
    match value {
        TelemetryValue::Counter(v) => format!("{}", v),
        TelemetryValue::Gauge(v) => {
            if v.fract() == 0.0 {
                format!("{:.0}", v)
            } else {
                format!("{:.2}", v)
            }
        }
        TelemetryValue::Text(s) => s.clone(),
        TelemetryValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        TelemetryValue::Binary(data) => format!("<{} bytes>", data.len()),
    }
}

/// Render a single device card.
fn render_device_card<'a>(
    device: &'a DeviceState,
    groups: &'a GroupsState,
) -> Element<'a, Message> {
    let status_indicator = if device.is_healthy {
        icons::status_healthy(IconSize::Small)
    } else {
        icons::status_warning(IconSize::Small)
    };

    let protocol_icon = icons::protocol_icon(device.id.protocol, IconSize::Medium);

    // Device name with tooltip showing full ID
    let device_name_text = text(&device.id.source).size(16);
    let device_name = tooltip(
        device_name_text,
        container(text(format!("{}/{}", device.id.protocol, device.id.source)).size(12))
            .padding(6)
            .style(container::rounded_box),
        tooltip::Position::Top,
    );

    let metric_count = text(format!("{} metrics", device.metric_count)).size(12);

    // Get device's group tags (convert to owned GroupTag to avoid lifetime issues)
    let device_groups: Vec<GroupTag> = groups
        .device_groups(&device.id)
        .iter()
        .map(|g| GroupTag::from_group(g))
        .collect();
    let group_tags = device_group_tags(device_groups);

    let header = row![
        status_indicator,
        protocol_icon,
        device_name,
        metric_count,
        group_tags
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Show a few recent metrics as preview with tooltips for full values
    let mut preview = Column::new().spacing(2);
    for (name, point) in device.metrics.iter().take(3) {
        let value = format_telemetry_value(&point.value);
        let display_value = if value.len() > MAX_VALUE_DISPLAY_LEN {
            format!("{}...", &value[..MAX_VALUE_DISPLAY_LEN])
        } else {
            value.clone()
        };

        let metric_line = text(format!("  {} = {}", name, display_value)).size(11);

        // Add tooltip only if value was truncated
        if value.len() > MAX_VALUE_DISPLAY_LEN {
            let metric_with_tooltip = tooltip(
                metric_line,
                container(column![text(name).size(11), text(value).size(11)].spacing(2))
                    .padding(6)
                    .style(container::rounded_box),
                tooltip::Position::Right,
            );
            preview = preview.push(metric_with_tooltip);
        } else {
            preview = preview.push(metric_line);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_state_with_devices(count: usize) -> DashboardState {
        let mut state = DashboardState::default();
        for i in 0..count {
            let id = DeviceId::new(Protocol::Snmp, format!("device{:03}", i));
            state.devices.insert(id.clone(), DeviceState::new(id));
        }
        state
    }

    #[test]
    fn test_pagination_total_pages() {
        let mut state = create_test_state_with_devices(50);
        state.devices_per_page = 20;

        assert_eq!(state.total_pages(), 3); // 50 / 20 = 2.5, rounds up to 3

        state.devices_per_page = 25;
        assert_eq!(state.total_pages(), 2); // 50 / 25 = 2
    }

    #[test]
    fn test_pagination_empty_state() {
        let state = DashboardState::default();
        assert_eq!(state.total_pages(), 1); // Always at least 1 page
        assert_eq!(state.paginated_devices().len(), 0);
    }

    #[test]
    fn test_pagination_next_prev() {
        let mut state = create_test_state_with_devices(50);
        state.devices_per_page = 20;

        assert_eq!(state.current_page, 0);

        state.next_page();
        assert_eq!(state.current_page, 1);

        state.next_page();
        assert_eq!(state.current_page, 2);

        // Should not go past last page
        state.next_page();
        assert_eq!(state.current_page, 2);

        state.prev_page();
        assert_eq!(state.current_page, 1);

        state.prev_page();
        assert_eq!(state.current_page, 0);

        // Should not go below 0
        state.prev_page();
        assert_eq!(state.current_page, 0);
    }

    #[test]
    fn test_pagination_go_to_page() {
        let mut state = create_test_state_with_devices(100);
        state.devices_per_page = 20;

        state.go_to_page(3);
        assert_eq!(state.current_page, 3);

        // Should clamp to max page
        state.go_to_page(100);
        assert_eq!(state.current_page, 4); // Last valid page (5 pages total: 0-4)
    }

    #[test]
    fn test_paginated_devices_returns_correct_slice() {
        let mut state = create_test_state_with_devices(50);
        state.devices_per_page = 20;

        assert_eq!(state.paginated_devices().len(), 20);

        state.current_page = 1;
        assert_eq!(state.paginated_devices().len(), 20);

        state.current_page = 2;
        assert_eq!(state.paginated_devices().len(), 10); // Last page has remainder
    }

    #[test]
    fn test_search_resets_page() {
        let mut state = create_test_state_with_devices(50);
        state.devices_per_page = 20;
        state.current_page = 2;

        // Set the search filter (goes to pending)
        state.set_search_filter("device".to_string());
        // Page doesn't reset until filter is applied
        assert_eq!(state.current_page, 2);

        // Simulate time passing and apply the filter
        state.pending_search_time = current_timestamp() - SEARCH_DEBOUNCE_MS - 1;
        state.apply_pending_search();
        assert_eq!(state.current_page, 0);
    }

    #[test]
    fn test_calculate_visible_pages_small() {
        // With 5 pages, show all
        let pages = calculate_visible_pages(2, 5);
        assert_eq!(pages, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_calculate_visible_pages_large() {
        // With 20 pages, current=0, should show first, nearby, and last
        let pages = calculate_visible_pages(0, 20);
        assert!(pages.contains(&0));
        assert!(pages.contains(&19));
        assert!(pages.len() <= 7);

        // With 20 pages, current=10, should show first, nearby, and last
        let pages = calculate_visible_pages(10, 20);
        assert!(pages.contains(&0));
        assert!(pages.contains(&10));
        assert!(pages.contains(&19));
    }
}
