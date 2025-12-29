//! Dashboard view showing all monitored devices.

use std::collections::HashMap;

use iced::widget::{
    Column, column, container, row, rule, scrollable, table, text, text_input, tooltip,
};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{DeviceStatus, HealthSnapshot, Protocol, TelemetryPoint, TelemetryValue};

/// Dashboard view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardViewMode {
    /// Card grid view (default).
    #[default]
    Grid,
    /// Table view with sortable columns.
    Table,
}

impl DashboardViewMode {
    /// Toggle between view modes.
    pub fn toggle(self) -> Self {
        match self {
            DashboardViewMode::Grid => DashboardViewMode::Table,
            DashboardViewMode::Table => DashboardViewMode::Grid,
        }
    }
}

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
    /// This is based on local staleness detection.
    pub is_healthy: bool,
    /// Device status from bridge liveness tracking.
    /// This is more accurate as it comes from the bridge's polling results.
    pub bridge_status: DeviceStatus,
    /// Number of consecutive failures reported by bridge.
    pub consecutive_failures: u32,
    /// Last error message from bridge (if any).
    pub last_error: Option<String>,
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
            bridge_status: DeviceStatus::Unknown,
            consecutive_failures: 0,
            last_error: None,
        }
    }

    /// Update health status based on last update time.
    pub fn update_health(&mut self, now: i64, stale_threshold_ms: i64) {
        self.is_healthy = (now - self.last_update) < stale_threshold_ms;
    }

    /// Update device status from bridge liveness data.
    pub fn update_from_liveness(
        &mut self,
        status: DeviceStatus,
        consecutive_failures: u32,
        last_error: Option<String>,
    ) {
        self.bridge_status = status;
        self.consecutive_failures = consecutive_failures;
        self.last_error = last_error;
    }

    /// Get the effective status for display.
    ///
    /// Combines local staleness detection with bridge liveness status.
    /// Bridge status takes precedence when available.
    pub fn effective_status(&self) -> DeviceStatus {
        // If bridge has reported a status other than Unknown, use it
        if self.bridge_status != DeviceStatus::Unknown {
            return self.bridge_status;
        }

        // Fall back to local staleness detection
        if self.is_healthy {
            DeviceStatus::Online
        } else {
            DeviceStatus::Offline
        }
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
    /// Current view mode (grid or table).
    pub view_mode: DashboardViewMode,
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
            view_mode: DashboardViewMode::default(),
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

    /// Toggle the view mode between grid and table.
    pub fn toggle_view_mode(&mut self) {
        self.view_mode = self.view_mode.toggle();
    }
}

/// Render the dashboard view.
pub fn dashboard_view<'a>(
    state: &'a DashboardState,
    theme: AppTheme,
    unacknowledged_alerts: usize,
    groups: &'a GroupsState,
    overview: &'a OverviewState,
    bridge_health: &'a HashMap<String, HealthSnapshot>,
) -> Element<'a, Message> {
    let header = render_header(state, theme, unacknowledged_alerts);
    let bridge_summary = render_bridge_health_summary(bridge_health);
    let filters = render_protocol_filters(state);
    let group_filters = group_filter_bar(groups);
    let overview_panel = overview_section(overview, &state.devices);
    let devices = render_device_grid(state, groups);

    let content = column![
        header,
        bridge_summary,
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
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).status_connected()),
            })
    } else {
        text("Disconnected")
            .size(14)
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).status_disconnected()),
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

    // View mode toggle button (grid vs table)
    let view_mode_icon = match state.view_mode {
        DashboardViewMode::Grid => icons::table(IconSize::Medium),
        DashboardViewMode::Table => icons::protocol(IconSize::Medium), // grid-like icon
    };
    let view_mode_label = match state.view_mode {
        DashboardViewMode::Grid => "Table",
        DashboardViewMode::Table => "Grid",
    };
    let view_mode_button = button(
        row![view_mode_icon, text(view_mode_label).size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleDashboardViewMode)
    .style(iced::widget::button::secondary);

    let header_row = row![
        title,
        device_count,
        status,
        view_mode_button,
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
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).danger()),
            });
        header_col = header_col.push(error_text);
    }

    header_col.spacing(5).into()
}

/// Render bridge health summary bar.
fn render_bridge_health_summary(
    bridge_health: &HashMap<String, HealthSnapshot>,
) -> Element<'_, Message> {
    if bridge_health.is_empty() {
        // No bridge health data received yet
        return row![].into();
    }

    let label = text("Bridges:").size(12);

    let mut bridge_row = row![label].spacing(10).align_y(Alignment::Center);

    // Sort bridges by name for consistent display
    let mut bridges: Vec<_> = bridge_health.values().collect();
    bridges.sort_by(|a, b| a.bridge.cmp(&b.bridge));

    for snapshot in bridges {
        // Determine status color based on health
        let status_icon = match snapshot.status.as_str() {
            "healthy" => icons::status_healthy(IconSize::Small),
            "degraded" => icons::status_degraded(IconSize::Small),
            "error" => icons::status_error(IconSize::Small),
            _ => icons::status_unknown(IconSize::Small),
        };

        // Build tooltip with detailed health info
        let tooltip_content = format!(
            "{}\nStatus: {}\nDevices: {}/{}\nMetrics: {}\nErrors (1h): {}",
            snapshot.bridge,
            snapshot.status,
            snapshot.devices_responding,
            snapshot.devices_total,
            snapshot.metrics_published,
            snapshot.errors_last_hour
        );

        let bridge_indicator = tooltip(
            row![status_icon, text(&snapshot.bridge).size(11)]
                .spacing(4)
                .align_y(Alignment::Center),
            container(text(tooltip_content).size(10))
                .padding(6)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        );

        bridge_row = bridge_row.push(bridge_indicator);
    }

    bridge_row.into()
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
    let devices: Vec<_> = if start < all_devices.len() {
        all_devices[start..end].to_vec()
    } else {
        vec![]
    };

    // Render based on view mode
    let content: Element<'a, Message> = match state.view_mode {
        DashboardViewMode::Grid => render_device_cards(&devices, groups),
        DashboardViewMode::Table => render_device_table(devices),
    };

    // Add pagination controls if there are multiple pages
    let total_pages = if all_devices.is_empty() {
        1
    } else {
        all_devices.len().div_ceil(state.devices_per_page)
    };

    let mut device_list = Column::new().spacing(10).push(content);

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

/// Render devices as cards (grid view).
fn render_device_cards<'a>(
    devices: &[&'a DeviceState],
    groups: &'a GroupsState,
) -> Element<'a, Message> {
    let mut device_list = Column::new().spacing(10);

    for device in devices {
        device_list = device_list.push(render_device_card(device, groups));
    }

    device_list.into()
}

/// Render devices as a table.
fn render_device_table(devices: Vec<&DeviceState>) -> Element<'_, Message> {
    use crate::view::theme;

    // Status column with colored indicator
    let status_column = table::column(
        text("Status").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            let status = device.effective_status();
            let (color, label) = match status {
                DeviceStatus::Online => (iced::Color::from_rgb(0.2, 0.8, 0.2), "Online"),
                DeviceStatus::Degraded => (iced::Color::from_rgb(1.0, 0.6, 0.0), "Degraded"),
                DeviceStatus::Offline => (iced::Color::from_rgb(0.9, 0.2, 0.2), "Offline"),
                DeviceStatus::Unknown => (iced::Color::from_rgb(0.5, 0.5, 0.5), "Unknown"),
            };

            row![
                container(text("").size(8))
                    .width(10)
                    .height(10)
                    .style(move |_theme: &Theme| container::Style {
                        background: Some(iced::Background::Color(color)),
                        border: iced::Border::default().rounded(5),
                        ..Default::default()
                    }),
                text(label).size(11)
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        },
    )
    .width(80);

    // Device name column (clickable)
    let name_column = table::column(
        text("Device").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            let device_id = device.id.clone();
            button(text(&device.id.source).size(11))
                .on_press(Message::SelectDevice(device_id))
                .style(iced::widget::button::text)
                .padding(0)
                .into()
        },
    )
    .width(Length::FillPortion(2));

    // Protocol column with icon
    let protocol_column = table::column(
        text("Protocol").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            row![
                icons::protocol_icon::<Message>(device.id.protocol, IconSize::Small),
                text(format!("{:?}", device.id.protocol)).size(11)
            ]
            .spacing(4)
            .align_y(Alignment::Center)
            .into()
        },
    )
    .width(100);

    // Metrics count column
    let metrics_column = table::column(
        text("Metrics").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            text(format!("{}", device.metric_count)).size(11).into()
        },
    )
    .width(70);

    // Last update column
    let update_column = table::column(
        text("Last Update").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            let ago = format_time_ago(device.last_update);
            text(ago)
                .size(11)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                })
                .into()
        },
    )
    .width(100);

    // Actions column
    let actions_column = table::column(
        text("").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            let device_id = device.id.clone();
            button(text("View").size(10))
                .on_press(Message::SelectDevice(device_id))
                .style(iced::widget::button::secondary)
                .padding([2, 8])
                .into()
        },
    )
    .width(60);

    // Table API: columns first, then data
    let tbl = table(
        [
            status_column,
            name_column,
            protocol_column,
            metrics_column,
            update_column,
            actions_column,
        ],
        devices,
    )
    .padding(6)
    .padding_y(4);

    container(tbl).width(Length::Fill).padding(10).into()
}

/// Format a timestamp as "X ago" string.
fn format_time_ago(timestamp: i64) -> String {
    let now = current_timestamp();
    let diff_ms = now - timestamp;

    if diff_ms < 0 {
        return "just now".to_string();
    }

    let seconds = diff_ms / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        format!("{}d ago", days)
    } else if hours > 0 {
        format!("{}h ago", hours)
    } else if minutes > 0 {
        format!("{}m ago", minutes)
    } else if seconds > 0 {
        format!("{}s ago", seconds)
    } else {
        "just now".to_string()
    }
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
    // Use effective_status which combines bridge liveness with local staleness detection
    let status = device.effective_status();
    let status_icon = icons::device_status_icon(status, IconSize::Small);

    // Add tooltip to status indicator showing status details
    let status_tooltip_text = match status {
        DeviceStatus::Online => "Online".to_string(),
        DeviceStatus::Offline => {
            if let Some(ref error) = device.last_error {
                format!("Offline: {}", error)
            } else {
                format!("Offline ({} failures)", device.consecutive_failures)
            }
        }
        DeviceStatus::Degraded => {
            if let Some(ref error) = device.last_error {
                format!("Degraded: {}", error)
            } else {
                format!("Degraded ({} failures)", device.consecutive_failures)
            }
        }
        DeviceStatus::Unknown => "Status unknown".to_string(),
    };
    let status_indicator = tooltip(
        status_icon,
        container(text(status_tooltip_text).size(11))
            .padding(6)
            .style(container::rounded_box),
        tooltip::Position::Top,
    );

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
