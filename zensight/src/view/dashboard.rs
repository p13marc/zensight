//! Dashboard view showing all monitored devices.

use std::collections::HashMap;

use iced::widget::{
    Column, column, container, grid, mouse_area, row, rule, scrollable, table, text, text_input,
    tooltip,
};
use iced::{Alignment, Color, Element, Length, Theme};
use iced_anim::widget::button;
use iced_anim::{AnimationBuilder, Easing};

use zensight_common::{DeviceStatus, HealthSnapshot, HealthStatus, Protocol, TelemetryPoint};

use crate::view::components::{badge, empty_state};

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
    /// Device status from sensor liveness tracking.
    /// This is more accurate as it comes from the sensor's polling results.
    pub sensor_status: DeviceStatus,
    /// Number of consecutive failures reported by sensor.
    pub consecutive_failures: u32,
    /// Last error message from sensor (if any).
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
            sensor_status: DeviceStatus::Unknown,
            consecutive_failures: 0,
            last_error: None,
        }
    }

    /// Update health status based on last update time.
    pub fn update_health(&mut self, now: i64, stale_threshold_ms: i64) {
        self.is_healthy = (now - self.last_update) < stale_threshold_ms;
    }

    /// Update device status from sensor liveness data.
    pub fn update_from_liveness(
        &mut self,
        status: DeviceStatus,
        consecutive_failures: u32,
        last_error: Option<String>,
    ) {
        self.sensor_status = status;
        self.consecutive_failures = consecutive_failures;
        self.last_error = last_error;
    }

    /// Get the effective status for display.
    ///
    /// Combines local staleness detection with sensor liveness status.
    /// Sensor status takes precedence when available.
    pub fn effective_status(&self) -> DeviceStatus {
        // If sensor has reported a status other than Unknown, use it
        if self.sensor_status != DeviceStatus::Unknown {
            return self.sensor_status;
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

/// Age after which a device that has received no telemetry is evicted from the
/// device map to bound memory over a long session (#40). 24h — generous enough
/// that known-down devices remain visible; only long-gone ones are reaped.
pub const DEVICE_EVICTION_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// Connection state for Zenoh session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Not connected and not attempting.
    #[default]
    Disconnected,
    /// Actively connecting to Zenoh.
    Connecting,
    /// Successfully connected.
    Connected,
}

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
    /// Current connection state (more granular than `connected`).
    pub connection_state: ConnectionState,
    /// Last error message, if any.
    pub last_error: Option<String>,
    /// Current page number (0-indexed).
    pub current_page: usize,
    /// Number of devices per page.
    pub devices_per_page: usize,
    /// Current view mode (grid or table).
    pub view_mode: DashboardViewMode,
    /// Active status filter (None = show all). Driven by the fleet summary
    /// chips so a click on "3 Offline" narrows the grid to the problems (#34).
    pub status_filter: Option<DeviceStatus>,
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
            connection_state: ConnectionState::default(),
            last_error: None,
            current_page: 0,
            devices_per_page: DEFAULT_DEVICES_PER_PAGE,
            view_mode: DashboardViewMode::default(),
            status_filter: None,
        }
    }
}

/// Sort rank for device status — problems first (#34). Lower sorts earlier.
pub(crate) fn status_rank(status: DeviceStatus) -> u8 {
    match status {
        DeviceStatus::Offline => 0,
        DeviceStatus::Degraded => 1,
        DeviceStatus::Unknown => 2,
        DeviceStatus::Online => 3,
    }
}

impl DashboardState {
    /// Ordered list of device ids matching the current filters, in render order.
    /// Used for device→device navigation on the detail view (#35).
    pub fn ordered_device_ids(&self) -> Vec<DeviceId> {
        self.filtered_devices()
            .into_iter()
            .map(|d| d.id.clone())
            .collect()
    }

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

                // Status filter (driven by the fleet summary chips, #34)
                let status_match = self.status_filter.is_none_or(|s| d.effective_status() == s);

                protocol_match && search_match && status_match
            })
            .collect();

        // Problem-first ordering (#34): worst status floats to the top so an
        // operator sees what's wrong without hunting. Stable within a status
        // group by protocol then source name.
        devices.sort_by(|a, b| {
            status_rank(a.effective_status())
                .cmp(&status_rank(b.effective_status()))
                .then_with(|| a.id.protocol.cmp(&b.id.protocol))
                .then_with(|| a.id.source.cmp(&b.id.source))
        });

        devices
    }

    /// Drop devices not seen for longer than `max_age_ms` so a long-running
    /// session can't grow the device map unbounded (#40). The threshold is
    /// deliberately generous (see `DEVICE_EVICTION_AGE_MS`) so known-down
    /// devices stay visible — only truly-gone ones are reaped. Returns the
    /// number of devices removed.
    pub fn evict_stale_devices(&mut self, now: i64, max_age_ms: i64) -> usize {
        let before = self.devices.len();
        self.devices
            .retain(|_, d| now.saturating_sub(d.last_update) <= max_age_ms);
        before - self.devices.len()
    }

    /// Set (or clear) the status filter, resetting pagination (#34).
    pub fn set_status_filter(&mut self, status: Option<DeviceStatus>) {
        // Toggle off if the same chip is clicked again.
        self.status_filter = if self.status_filter == status {
            None
        } else {
            status
        };
        self.current_page = 0;
    }

    /// Count **hosts** by worst-facet status, for the fleet summary bar (#34/#128).
    /// One physical host counts once even when it runs several sensors. Returns
    /// (online, degraded, offline, unknown).
    pub fn status_counts(&self) -> (usize, usize, usize, usize) {
        let all: Vec<&DeviceState> = self.devices.values().collect();
        let mut counts = (0, 0, 0, 0);
        for host in crate::view::host::aggregate(&all) {
            match host.effective_status() {
                DeviceStatus::Online => counts.0 += 1,
                DeviceStatus::Degraded => counts.1 += 1,
                DeviceStatus::Offline => counts.2 += 1,
                DeviceStatus::Unknown => counts.3 += 1,
            }
        }
        counts
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
#[allow(clippy::too_many_arguments)]
pub fn dashboard_view<'a>(
    state: &'a DashboardState,
    theme: AppTheme,
    unacknowledged_alerts: usize,
    groups: &'a GroupsState,
    overview: &'a OverviewState,
    sensor_health: &'a HashMap<String, HealthSnapshot>,
    mut sparks: crate::view::trend::DeviceSparks,
) -> Element<'a, Message> {
    // Compute filtered devices once and pass through to avoid redundant work
    let filtered = state.filtered_devices();

    let header = render_header(state, theme, unacknowledged_alerts);
    let fleet_summary = render_fleet_summary(state, unacknowledged_alerts);
    let health_overview = render_health_overview(state);
    let sensor_summary = render_sensor_health_summary(sensor_health);
    let filters = render_protocol_filters(state, &filtered);
    let group_filters = group_filter_bar(groups);
    let overview_panel = overview_section(overview, &state.devices);
    let devices = render_device_grid(state, groups, &filtered, &mut sparks);

    let content = column![
        header,
        fleet_summary,
        health_overview,
        sensor_summary,
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

/// Render the fleet-health summary bar: a click-to-filter rollup of how many
/// devices are offline / degraded / unknown / online, plus a firing-alert chip
/// (#34). Answers "what's wrong right now?" at the top of the dashboard.
fn render_fleet_summary(
    state: &DashboardState,
    unacknowledged_alerts: usize,
) -> Element<'_, Message> {
    let (online, degraded, offline, unknown) = state.status_counts();

    // A click-to-filter chip for one status; highlighted when it's the active
    // filter. Toggling the same chip clears the filter (see set_status_filter).
    let chip = |status: DeviceStatus, count: usize, label: &'static str| -> Element<'_, Message> {
        let active = state.status_filter == Some(status);
        let content: Element<'_, Message> = badge(status_color(status), format!("{count} {label}"));
        let mut b = button(content)
            .on_press(Message::SetStatusFilter(Some(status)))
            .padding([4, 10]);
        b = if active {
            b.style(iced::widget::button::primary)
        } else {
            b.style(iced::widget::button::text)
        };
        b.into()
    };

    if state.devices.is_empty() {
        return row![].into();
    }

    let mut bar = row![text("Fleet:").size(14)]
        .spacing(10)
        .align_y(Alignment::Center);

    // Problems first, then healthy. Only show a status chip when it's non-empty
    // (Offline/Degraded always matter; Online/Unknown only when present).
    if offline > 0 {
        bar = bar.push(chip(DeviceStatus::Offline, offline, "Offline"));
    }
    if degraded > 0 {
        bar = bar.push(chip(DeviceStatus::Degraded, degraded, "Degraded"));
    }
    if unknown > 0 {
        bar = bar.push(chip(DeviceStatus::Unknown, unknown, "Unknown"));
    }
    bar = bar.push(chip(DeviceStatus::Online, online, "Online"));

    // Firing-alert chip routes to the Alerts view (#34/#35).
    if unacknowledged_alerts > 0 {
        let firing = button(
            text(format!("{unacknowledged_alerts} firing"))
                .size(font_caption())
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).danger()),
                }),
        )
        .on_press(Message::OpenAlerts)
        .padding([4, 10])
        .style(iced::widget::button::text);
        bar = bar.push(firing);
    }

    // A "Show all" affordance when a status filter is active.
    if state.status_filter.is_some() {
        bar = bar.push(
            button(text("Show all").size(font_caption()))
                .on_press(Message::SetStatusFilter(None))
                .padding([4, 10])
                .style(iced::widget::button::text),
        );
    }

    bar.into()
}

/// Caption font size as f32 (Iced 0.14 wants f32 for `.size()`).
fn font_caption() -> f32 {
    crate::view::tokens::font::CAPTION
}

/// Number of worst hosts surfaced in the health overview banner (#130).
const HEALTH_OVERVIEW_LIMIT: usize = 8;

/// Worst-first fleet health overview (#130): scores every device, then bands and
/// surfaces the lowest-scoring (Degraded/Critical) hosts worst-first as
/// click-to-open chips — the "what should I look at first" triage row. Collapses
/// to a single reassuring line when nothing is unhealthy.
fn render_health_overview(state: &DashboardState) -> Element<'_, Message> {
    use crate::view::health::HealthBand;

    // Score by physical HOST (#128) so a multi-protocol host counts once, with
    // the composite (worst-facet) score; keep only the actionable ones.
    let all: Vec<&DeviceState> = state.devices.values().collect();
    let mut scored: Vec<(&DeviceState, crate::view::health::HealthScore)> =
        crate::view::host::aggregate(&all)
            .into_iter()
            .map(|h| (h.primary(), h.health()))
            .filter(|(_, s)| matches!(s.band, HealthBand::Degraded | HealthBand::Critical))
            .collect();

    if scored.is_empty() {
        // Don't draw an empty banner before any telemetry has arrived.
        if state.devices.is_empty() {
            return column![].into();
        }
        return container(
            text("Fleet health: all hosts healthy")
                .size(font_caption())
                .style(|t: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(t).text_muted()),
                }),
        )
        .padding([6, 10])
        .into();
    }

    // Worst first (lowest score), then take the top offenders.
    scored.sort_by_key(|(_, s)| s.value);
    let total_unhealthy = scored.len();

    let mut items: Vec<Element<'_, Message>> = vec![
        text(format!("Worst hosts ({total_unhealthy})"))
            .size(font_caption())
            .style(|t: &Theme| text::Style {
                color: Some(crate::view::theme::colors(t).text_muted()),
            })
            .into(),
    ];
    for (device, score) in scored.into_iter().take(HEALTH_OVERVIEW_LIMIT) {
        let chip = button(badge::<Message>(
            score.band.color(),
            format!("{} · {}", device.id.source, score.value),
        ))
        .on_press(Message::SelectDevice(device.id.clone()))
        .padding([2, 6])
        .style(iced::widget::button::text);
        items.push(chip.into());
    }

    container(
        iced::widget::Row::with_children(items)
            .spacing(8)
            .align_y(Alignment::Center)
            .wrap(),
    )
    .padding([6, 10])
    .width(Length::Fill)
    .into()
}

/// Render the header with connection status.
fn render_header(
    state: &DashboardState,
    theme: AppTheme,
    _unacknowledged_alerts: usize,
) -> Element<'_, Message> {
    let title = text("ZenSight Dashboard").size(24);

    let device_count = text(format!("{} devices", state.devices.len())).size(14);

    // Theme toggle button - show moon when dark (click to go light), sun when light (click to go dark)
    let theme_icon = match theme {
        AppTheme::Dark => icons::moon(IconSize::Medium),
        AppTheme::Light => icons::sun(IconSize::Medium),
    };
    let theme_button = button(theme_icon)
        .on_press(Message::ToggleTheme)
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

    // Global metric search trigger (Ctrl+K) — discoverable button (#27).
    let search_button = button(
        row![
            icons::search(IconSize::Medium),
            text("Search (Ctrl+K)").size(14)
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(Message::OpenGlobalSearch)
    .style(iced::widget::button::secondary);

    // Connection status + primary navigation (Alerts/Topology/Settings) now live
    // in the persistent app shell (view/shell.rs), so the dashboard header keeps
    // only its page-local controls.
    let header_row = row![
        title,
        device_count,
        search_button,
        view_mode_button,
        theme_button
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

/// Render sensor health summary bar.
fn render_sensor_health_summary(
    sensor_health: &HashMap<String, HealthSnapshot>,
) -> Element<'_, Message> {
    if sensor_health.is_empty() {
        // No sensor health data received yet
        return row![].into();
    }

    let label = text("Sensors:").size(12);

    let mut sensor_row = row![label].spacing(10).align_y(Alignment::Center);

    // Sort sensors by name for consistent display
    let mut sensors: Vec<_> = sensor_health.values().collect();
    sensors.sort_by(|a, b| a.sensor.cmp(&b.sensor));

    for snapshot in sensors {
        // Determine status color based on health
        let status_icon = match snapshot.status {
            HealthStatus::Healthy => icons::status_healthy(IconSize::Small),
            HealthStatus::Degraded => icons::status_degraded(IconSize::Small),
            HealthStatus::Error | HealthStatus::Unhealthy => icons::status_error(IconSize::Small),
            _ => icons::status_unknown(IconSize::Small),
        };

        // Build tooltip with detailed health info
        let tooltip_content = format!(
            "{}\nStatus: {}\nDevices: {}/{}\nMetrics: {}\nErrors (1h): {}",
            snapshot.sensor,
            snapshot.status,
            snapshot.devices_responding,
            snapshot.devices_total,
            snapshot.metrics_published,
            snapshot.errors_last_hour
        );

        let sensor_indicator = tooltip(
            row![status_icon, text(&snapshot.sensor).size(11)]
                .spacing(4)
                .align_y(Alignment::Center),
            container(text(tooltip_content).size(10))
                .padding(6)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        );

        sensor_row = sensor_row.push(sensor_indicator);
    }

    sensor_row.into()
}

/// Render protocol filter buttons and search input.
fn render_protocol_filters<'a>(
    state: &'a DashboardState,
    filtered: &[&DeviceState],
) -> Element<'a, Message> {
    let protocols = state.active_protocols();

    if protocols.is_empty() {
        return empty_state("No devices yet — waiting for sensors…", None);
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

    // Device count (use pre-computed filtered list)
    let filtered_count = filtered.len();
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
    filtered: &[&'a DeviceState],
    sparks: &mut crate::view::trend::DeviceSparks,
) -> Element<'a, Message> {
    // Apply group filter on top of pre-computed protocol/search filter
    let all_devices: Vec<_> = filtered
        .iter()
        .copied()
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

    // Grid mode renders one card per physical HOST (#128) — facets merged by
    // source; table mode stays per-facet (the detail view). Paginate over the
    // unit each mode shows so the page count matches the cards/rows on screen.
    let per_page = state.devices_per_page;
    let (content, total_units): (Element<'a, Message>, usize) = match state.view_mode {
        DashboardViewMode::Grid => {
            let hosts = crate::view::host::aggregate(&all_devices);
            let total = hosts.len();
            let start = state.current_page * per_page;
            let end = (start + per_page).min(total);
            let page = if start < total {
                &hosts[start..end]
            } else {
                &[]
            };
            (render_host_cards(page, groups, sparks), total)
        }
        DashboardViewMode::Table => {
            let total = all_devices.len();
            let start = state.current_page * per_page;
            let end = (start + per_page).min(total);
            let devices: Vec<_> = if start < total {
                all_devices[start..end].to_vec()
            } else {
                vec![]
            };
            (render_device_table(devices), total)
        }
    };

    // Add pagination controls if there are multiple pages
    let total_pages = if total_units == 0 {
        1
    } else {
        total_units.div_ceil(per_page)
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

/// Minimum card width for responsive grid layout.
const CARD_MIN_WIDTH: f32 = 350.0;

/// Render physical hosts as cards (grid view) — the #128 keystone re-key. Each
/// card merges a host's per-protocol facets: one card per `source`, with a
/// composite health badge and a clickable badge per sensor facet.
fn render_host_cards<'a>(
    hosts: &[crate::view::host::Host<'a>],
    groups: &'a GroupsState,
    sparks: &mut crate::view::trend::DeviceSparks,
) -> Element<'a, Message> {
    let cards: Vec<Element<'a, Message>> = hosts
        .iter()
        .map(|host| {
            // Merge each facet's spark strip into one host-level strip.
            let mut merged: Vec<crate::view::trend::MetricSpark> = Vec::new();
            for facet in &host.facets {
                if let Some(s) = sparks.remove(&facet.id) {
                    merged.extend(s);
                }
            }
            let merged = if merged.is_empty() {
                None
            } else {
                Some(merged)
            };
            render_host_card(host, groups, merged)
        })
        .collect();

    grid(cards)
        .fluid(CARD_MIN_WIDTH)
        .spacing(10)
        .height(Length::Shrink)
        .into()
}

/// Render a single host card: merged identity + composite health + per-facet
/// clickable badges. The card body opens the primary facet; each facet badge
/// opens that specific facet's device view (#128).
fn render_host_card<'a>(
    host: &crate::view::host::Host<'a>,
    groups: &'a GroupsState,
    sparks: Option<Vec<crate::view::trend::MetricSpark>>,
) -> Element<'a, Message> {
    let primary = host.primary();
    let status = host.effective_status();

    let status_indicator_dot = animated_status_indicator(status, 12.0);
    let status_tooltip_text = match status {
        DeviceStatus::Online => "Online".to_string(),
        DeviceStatus::Offline => "Offline".to_string(),
        DeviceStatus::Degraded => "Degraded".to_string(),
        DeviceStatus::Unknown => "Status unknown".to_string(),
    };
    let status_indicator = tooltip(
        status_indicator_dot,
        container(text(status_tooltip_text).size(11))
            .padding(6)
            .style(container::rounded_box),
        tooltip::Position::Top,
    );

    let primary_icon = icons::protocol_icon(primary.id.protocol, IconSize::Medium);

    // Host name with a tooltip listing the sensors present on it.
    let protocols: Vec<String> = host
        .facets
        .iter()
        .map(|f| f.id.protocol.display_name().to_string())
        .collect();
    let host_name = tooltip(
        text(host.source).size(16),
        container(text(format!("{} · {}", host.source, protocols.join(", "))).size(12))
            .padding(6)
            .style(container::rounded_box),
        tooltip::Position::Top,
    );

    let metric_count = text(format!("{} metrics", host.metric_count())).size(12);

    // Composite health across all facets (#128/#130): the worst facet wins.
    let health = host.health();
    let health_badge =
        crate::view::components::badge::<Message>(health.band.color(), health.label());

    // Group tags come from the primary facet's device id (group membership is
    // keyed by DeviceId, which stays the facet key).
    let device_groups: Vec<GroupTag> = groups
        .device_groups(&primary.id)
        .iter()
        .map(|g| GroupTag::from_group(g))
        .collect();
    let group_tags = device_group_tags(device_groups);

    let header = row![
        status_indicator,
        primary_icon,
        host_name,
        health_badge,
        metric_count,
        group_tags
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // One clickable badge per facet — protocol icon + per-facet status dot —
    // pivoting to that sensor's device view.
    let mut facet_row = iced::widget::Row::new().spacing(6);
    for facet in &host.facets {
        let fstatus = facet.effective_status();
        let chip = button(
            row![
                animated_status_indicator(fstatus, 8.0),
                icons::protocol_icon::<Message>(facet.id.protocol, IconSize::Small),
                text(facet.id.protocol.display_name()).size(11),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        )
        .on_press(Message::SelectDevice(facet.id.clone()))
        .padding([2, 6])
        .style(iced::widget::button::secondary);
        facet_row = facet_row.push(tooltip(
            chip,
            container(text(format!("{} metrics", facet.metric_count)).size(11))
                .padding(6)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        ));
    }

    let mut card_content = column![header, facet_row.wrap()].spacing(6);
    if let Some(sparks) = sparks.filter(|s| !s.is_empty()) {
        let mut spark_col = Column::new().spacing(2);
        for spark in sparks {
            spark_col = spark_col.push(crate::view::trend::card_metric_spark::<Message>(spark));
        }
        card_content = card_content.push(spark_col);
    }

    let card_button = button(card_content)
        .on_press(Message::SelectDevice(primary.id.clone()))
        .padding(10)
        .width(Length::Fill)
        .style(iced::widget::button::secondary);

    mouse_area(card_button)
        .on_double_click(Message::SelectDevice(primary.id.clone()))
        .into()
}

/// Render devices as a table.
fn render_device_table(devices: Vec<&DeviceState>) -> Element<'_, Message> {
    use crate::view::theme;

    // Status column with animated indicator
    let status_column = table::column(
        text("Status").size(12),
        |device: &DeviceState| -> Element<'_, Message> {
            let status = device.effective_status();
            let label = match status {
                DeviceStatus::Online => "Online",
                DeviceStatus::Degraded => "Degraded",
                DeviceStatus::Offline => "Offline",
                DeviceStatus::Unknown => "Unknown",
            };

            row![
                animated_status_indicator(status, 10.0),
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
                text(device.id.protocol.display_name()).size(11)
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

/// Get the color for a device status.
fn status_color(status: DeviceStatus) -> Color {
    match status {
        DeviceStatus::Online => Color::from_rgb(0.2, 0.8, 0.2), // Green
        DeviceStatus::Degraded => Color::from_rgb(1.0, 0.6, 0.0), // Orange
        DeviceStatus::Offline => Color::from_rgb(0.9, 0.2, 0.2), // Red
        DeviceStatus::Unknown => Color::from_rgb(0.5, 0.5, 0.5), // Gray
    }
}

/// Render an animated status indicator dot.
/// The color smoothly transitions when the status changes.
fn animated_status_indicator<'a>(status: DeviceStatus, size: f32) -> Element<'a, Message> {
    let color = status_color(status);

    AnimationBuilder::new(color, move |animated_color| {
        container(text(""))
            .width(size)
            .height(size)
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(animated_color)),
                border: iced::Border::default().rounded(size / 2.0),
                ..Default::default()
            })
            .into()
    })
    .animation(Easing::EASE_IN_OUT.quick())
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

    fn device_with_status(source: &str, status: DeviceStatus) -> DeviceState {
        let id = DeviceId::new(Protocol::Sysinfo, source);
        let mut d = DeviceState::new(id);
        d.sensor_status = status;
        d
    }

    #[test]
    fn test_status_counts_and_problem_first_sort() {
        let mut state = DashboardState::default();
        for (src, st) in [
            ("a-online", DeviceStatus::Online),
            ("b-offline", DeviceStatus::Offline),
            ("c-degraded", DeviceStatus::Degraded),
            ("d-online", DeviceStatus::Online),
        ] {
            let d = device_with_status(src, st);
            state.devices.insert(d.id.clone(), d);
        }

        // (online, degraded, offline, unknown)
        assert_eq!(state.status_counts(), (2, 1, 1, 0));

        // Problem-first: offline, then degraded, then the two online.
        let order: Vec<String> = state
            .ordered_device_ids()
            .into_iter()
            .map(|id| id.source)
            .collect();
        assert_eq!(order[0], "b-offline");
        assert_eq!(order[1], "c-degraded");
    }

    #[test]
    fn test_status_counts_aggregates_by_host() {
        // #128: a single physical host with two sensor facets must count ONCE,
        // taking the worst facet's status — not once per protocol.
        let mut state = DashboardState::default();
        let mut sys = DeviceState::new(DeviceId::new(Protocol::Sysinfo, "host1"));
        sys.sensor_status = DeviceStatus::Online;
        let mut net = DeviceState::new(DeviceId::new(Protocol::Netlink, "host1"));
        net.sensor_status = DeviceStatus::Offline;
        state.devices.insert(sys.id.clone(), sys);
        state.devices.insert(net.id.clone(), net);

        // One host, worst facet Offline → (online, degraded, offline, unknown).
        assert_eq!(state.status_counts(), (0, 0, 1, 0));
    }

    #[test]
    fn test_evict_stale_devices() {
        let mut state = DashboardState::default();
        let mut fresh = device_with_status("fresh", DeviceStatus::Online);
        fresh.last_update = 10_000;
        let mut old = device_with_status("old", DeviceStatus::Offline);
        old.last_update = 1_000;
        state.devices.insert(fresh.id.clone(), fresh);
        state.devices.insert(old.id.clone(), old);

        // At now=60_000 with max_age=55_000: fresh age 50_000 (kept),
        // old age 59_000 (reaped).
        let removed = state.evict_stale_devices(60_000, 55_000);
        assert_eq!(removed, 1);
        assert_eq!(state.devices.len(), 1);
        assert!(state.devices.keys().any(|id| id.source == "fresh"));
    }

    #[test]
    fn test_status_filter_narrows_and_toggles() {
        let mut state = DashboardState::default();
        for (src, st) in [("a", DeviceStatus::Online), ("b", DeviceStatus::Offline)] {
            let d = device_with_status(src, st);
            state.devices.insert(d.id.clone(), d);
        }

        state.set_status_filter(Some(DeviceStatus::Offline));
        let ids = state.ordered_device_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].source, "b");

        // Clicking the same chip again clears the filter.
        state.set_status_filter(Some(DeviceStatus::Offline));
        assert!(state.status_filter.is_none());
        assert_eq!(state.ordered_device_ids().len(), 2);
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
