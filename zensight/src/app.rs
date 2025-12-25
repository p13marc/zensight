//! ZenSight Iced application.

use iced::widget::Id;
use iced::widget::operation::focus;
use iced::{Element, Subscription, Task, Theme};
use std::sync::LazyLock;

use zensight_common::{TelemetryPoint, TelemetryValue, ZenohConfig};

/// Text input ID for dashboard search.
pub static DASHBOARD_SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("dashboard-search"));

/// Text input ID for device metric search.
pub static DEVICE_SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("device-search"));

use crate::message::{DeviceId, Message};
use crate::mock;
use crate::subscription::{
    demo_subscription, keyboard_subscription, tick_subscription, zenoh_subscription,
};
use crate::view::alerts::{AlertsState, alerts_view};
use crate::view::dashboard::{DashboardState, DeviceState, dashboard_view};
use crate::view::device::{DeviceDetailState, device_view};
use crate::view::settings::{PersistentSettings, SettingsState, settings_view};

/// Current view in the application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CurrentView {
    #[default]
    Dashboard,
    Device,
    Settings,
    Alerts,
}

/// Application theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppTheme {
    #[default]
    Dark,
    Light,
}

impl AppTheme {
    /// Toggle between light and dark theme.
    pub fn toggle(self) -> Self {
        match self {
            AppTheme::Dark => AppTheme::Light,
            AppTheme::Light => AppTheme::Dark,
        }
    }

    /// Convert to Iced theme.
    pub fn to_iced_theme(self) -> Theme {
        match self {
            AppTheme::Dark => Theme::Dark,
            AppTheme::Light => Theme::Light,
        }
    }
}

/// The main ZenSight application.
pub struct ZenSight {
    /// Zenoh configuration.
    zenoh_config: ZenohConfig,
    /// Dashboard state.
    dashboard: DashboardState,
    /// Currently selected device (if any).
    selected_device: Option<DeviceDetailState>,
    /// Settings state.
    settings: SettingsState,
    /// Alerts state.
    alerts: AlertsState,
    /// Current view.
    current_view: CurrentView,
    /// Stale threshold in milliseconds (devices not updated within this time are marked unhealthy).
    stale_threshold_ms: i64,
    /// Demo mode (use mock data instead of Zenoh).
    demo_mode: bool,
    /// Current theme.
    theme: AppTheme,
}

impl ZenSight {
    /// Boot the ZenSight application (called by iced::application).
    pub fn boot(demo_mode: bool) -> (Self, Task<Message>) {
        // Load persistent settings from disk
        let persistent = PersistentSettings::load();

        // Build Zenoh configuration from loaded settings
        let zenoh_config = ZenohConfig {
            mode: persistent.zenoh_mode.clone(),
            connect: persistent.zenoh_connect.clone(),
            listen: persistent.zenoh_listen.clone(),
        };

        let stale_threshold_ms = (persistent.stale_threshold_secs * 1000) as i64;

        let settings = persistent.to_state();

        let mut dashboard = DashboardState::default();

        // In demo mode, pre-populate with mock data and mark as connected
        if demo_mode {
            dashboard.connected = true;
            for point in mock::mock_environment() {
                let device_id = DeviceId::from_telemetry(&point);
                let device_state = dashboard
                    .devices
                    .entry(device_id.clone())
                    .or_insert_with(|| DeviceState::new(device_id.clone()));

                device_state.last_update = point.timestamp;
                device_state.metric_count = device_state.metrics.len() + 1;
                device_state
                    .metrics
                    .insert(point.metric.clone(), format_telemetry_value(&point.value));
                device_state.is_healthy = true;
            }
        }

        // Load theme preference
        let theme = if persistent.dark_theme {
            AppTheme::Dark
        } else {
            AppTheme::Light
        };

        // Create alerts state with configured max, with demo rules if in demo mode
        let mut alerts = AlertsState::with_max_alerts(persistent.max_alerts);
        if demo_mode {
            use crate::demo::demo_alert_rules;
            for rule in demo_alert_rules() {
                alerts.rules.push(rule);
            }
            // Set shorter cooldown for demo (10 seconds instead of 60)
            alerts.alert_cooldown_ms = 10_000;
        }

        let app = Self {
            zenoh_config,
            dashboard,
            selected_device: None,
            settings,
            alerts,
            current_view: CurrentView::default(),
            stale_threshold_ms,
            demo_mode,
            theme,
        };

        (app, Task::none())
    }

    /// Get the window title.
    pub fn title(&self) -> String {
        let device_count = self.dashboard.devices.len();
        if device_count > 0 {
            format!("ZenSight - {} devices", device_count)
        } else {
            "ZenSight".to_string()
        }
    }

    /// Handle incoming messages.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TelemetryReceived(point) => {
                self.handle_telemetry(point);
            }

            Message::Connected => {
                tracing::info!("Connected to Zenoh");
                self.dashboard.connected = true;
                self.dashboard.last_error = None;
            }

            Message::Disconnected(error) => {
                tracing::warn!(error = %error, "Disconnected from Zenoh");
                self.dashboard.connected = false;
                self.dashboard.last_error = Some(error);
            }

            Message::SelectDevice(device_id) => {
                self.select_device(device_id);
            }

            Message::ClearSelection => {
                self.selected_device = None;
                self.current_view = CurrentView::Dashboard;
            }

            Message::ToggleProtocolFilter(protocol) => {
                self.dashboard.toggle_filter(protocol);
            }

            Message::SetDeviceSearchFilter(filter) => {
                self.dashboard.set_search_filter(filter);
            }

            Message::NextPage => {
                self.dashboard.next_page();
            }

            Message::PrevPage => {
                self.dashboard.prev_page();
            }

            Message::GoToPage(page) => {
                self.dashboard.go_to_page(page);
            }

            Message::SelectMetricForChart(metric_name) => {
                if let Some(ref mut device) = self.selected_device {
                    device.select_metric(metric_name);
                }
            }

            Message::ClearChartSelection => {
                if let Some(ref mut device) = self.selected_device {
                    device.clear_chart_selection();
                }
            }

            Message::SetChartTimeWindow(window) => {
                if let Some(ref mut device) = self.selected_device {
                    device.set_time_window(window);
                }
            }

            Message::ChartZoomIn => {
                if let Some(ref mut device) = self.selected_device {
                    device.zoom_in();
                }
            }

            Message::ChartZoomOut => {
                if let Some(ref mut device) = self.selected_device {
                    device.zoom_out();
                }
            }

            Message::ChartZoomReset => {
                if let Some(ref mut device) = self.selected_device {
                    device.reset_zoom();
                }
            }

            Message::SetMetricFilter(filter) => {
                if let Some(ref mut device) = self.selected_device {
                    device.set_metric_filter(filter);
                }
            }

            Message::Tick => {
                self.handle_tick();
            }

            // Settings messages
            Message::OpenSettings => {
                self.current_view = CurrentView::Settings;
            }

            Message::CloseSettings => {
                self.current_view = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
            }

            Message::SetZenohMode(mode) => {
                self.settings.set_mode(mode);
            }

            Message::SetZenohConnect(endpoints) => {
                self.settings.set_connect(endpoints);
            }

            Message::SetZenohListen(endpoints) => {
                self.settings.set_listen(endpoints);
            }

            Message::SetStaleThreshold(threshold) => {
                self.settings.set_stale_threshold(threshold);
            }

            Message::SetMaxHistory(max_history) => {
                self.settings.set_max_history(max_history);
            }

            Message::SetMaxAlerts(max_alerts) => {
                self.settings.set_max_alerts(max_alerts);
            }

            Message::SaveSettings => {
                self.save_settings();
            }

            Message::ResetSettings => {
                self.reset_settings();
            }

            // Alert messages
            Message::OpenAlerts => {
                self.current_view = CurrentView::Alerts;
            }

            Message::CloseAlerts => {
                self.current_view = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
            }

            Message::SetAlertRuleName(name) => {
                self.alerts.set_new_rule_name(name);
            }

            Message::SetAlertRuleMetric(metric) => {
                self.alerts.set_new_rule_metric(metric);
            }

            Message::SetAlertRuleThreshold(threshold) => {
                self.alerts.set_new_rule_threshold(threshold);
            }

            Message::SetAlertRuleOperator(op) => {
                self.alerts.set_new_rule_operator(op);
            }

            Message::SetAlertRuleSeverity(severity) => {
                self.alerts.set_new_rule_severity(severity);
            }

            Message::AddAlertRule => {
                if let Err(e) = self.alerts.add_rule() {
                    tracing::warn!(error = %e, "Failed to add alert rule");
                }
            }

            Message::RemoveAlertRule(rule_id) => {
                self.alerts.remove_rule(rule_id);
            }

            Message::ToggleAlertRule(rule_id) => {
                self.alerts.toggle_rule(rule_id);
            }

            Message::AcknowledgeAlert(alert_id) => {
                self.alerts.acknowledge(alert_id);
            }

            Message::AcknowledgeAllAlerts => {
                self.alerts.acknowledge_all();
            }

            Message::ClearAlerts => {
                self.alerts.clear_alerts();
            }

            // Export messages
            Message::ExportToCsv => {
                self.export_to_csv();
            }

            Message::ExportToJson => {
                self.export_to_json();
            }

            Message::ToggleTheme => {
                self.theme = self.theme.toggle();
                // Persist the theme preference
                self.settings.dark_theme = matches!(self.theme, AppTheme::Dark);
                self.settings.modified = true;
            }

            // Keyboard shortcuts
            Message::FocusSearch => {
                return self.focus_search();
            }

            Message::EscapePressed => {
                self.handle_escape();
            }
        }

        Task::none()
    }

    /// Focus the appropriate search input based on current view.
    fn focus_search(&self) -> Task<Message> {
        match self.current_view {
            CurrentView::Dashboard => focus(DASHBOARD_SEARCH_ID.clone()),
            CurrentView::Device => focus(DEVICE_SEARCH_ID.clone()),
            _ => Task::none(),
        }
    }

    /// Handle Escape key - close dialogs or go back.
    fn handle_escape(&mut self) {
        match self.current_view {
            CurrentView::Settings => {
                self.current_view = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
            }
            CurrentView::Alerts => {
                self.current_view = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
            }
            CurrentView::Device => {
                // If charting, close chart; otherwise go back to dashboard
                if let Some(ref mut device) = self.selected_device {
                    if device.selected_metric.is_some() {
                        device.clear_chart_selection();
                    } else {
                        self.selected_device = None;
                        self.current_view = CurrentView::Dashboard;
                    }
                }
            }
            CurrentView::Dashboard => {
                // Clear search filter if set
                if !self.dashboard.search_filter.is_empty() {
                    self.dashboard.search_filter.clear();
                    self.dashboard.pending_search.clear();
                }
            }
        }
    }

    /// Create subscriptions for Zenoh telemetry and periodic updates.
    pub fn subscription(&self) -> Subscription<Message> {
        if self.demo_mode {
            // In demo mode, use mock data generator instead of Zenoh
            Subscription::batch([
                demo_subscription(),
                tick_subscription(),
                keyboard_subscription(),
            ])
        } else {
            Subscription::batch([
                zenoh_subscription(self.zenoh_config.clone()),
                tick_subscription(),
                keyboard_subscription(),
            ])
        }
    }

    /// Render the view.
    pub fn view(&self) -> Element<'_, Message> {
        let unack = self.alerts.unacknowledged_count;
        match self.current_view {
            CurrentView::Settings => settings_view(&self.settings),
            CurrentView::Alerts => alerts_view(&self.alerts),
            CurrentView::Device => {
                if let Some(ref device_state) = self.selected_device {
                    device_view(device_state)
                } else {
                    dashboard_view(&self.dashboard, self.theme, unack)
                }
            }
            CurrentView::Dashboard => dashboard_view(&self.dashboard, self.theme, unack),
        }
    }

    /// Get the application theme.
    pub fn theme(&self) -> Theme {
        self.theme.to_iced_theme()
    }

    /// Handle incoming telemetry.
    fn handle_telemetry(&mut self, point: TelemetryPoint) {
        let device_id = DeviceId::from_telemetry(&point);

        // Update dashboard device state
        let device_state = self
            .dashboard
            .devices
            .entry(device_id.clone())
            .or_insert_with(|| DeviceState::new(device_id.clone()));

        device_state.last_update = point.timestamp;
        device_state.metric_count = device_state.metrics.len() + 1;
        device_state
            .metrics
            .insert(point.metric.clone(), format_telemetry_value(&point.value));
        device_state.is_healthy = true;

        // Check alert rules for numeric values
        if let Some(numeric_value) = telemetry_to_f64(&point.value)
            && let Some(alert) =
                self.alerts
                    .check_metric(&device_id, &point.metric, numeric_value, point.timestamp)
        {
            tracing::warn!(
                rule = %alert.rule_name,
                device = %alert.device_id,
                metric = %alert.metric,
                value = %alert.value,
                threshold = %alert.threshold,
                "Alert triggered"
            );
        }

        // Update selected device if this telemetry is for it
        if let Some(ref mut selected) = self.selected_device
            && selected.device_id == device_id
        {
            selected.update(point);
        }
    }

    /// Select a device to view in detail.
    fn select_device(&mut self, device_id: DeviceId) {
        tracing::info!(device = %device_id, "Selected device");
        // We don't have the full TelemetryPoints in the dashboard,
        // so the detail view will populate as new data arrives
        let max_history = self.settings.max_history_value();
        let detail_state = DeviceDetailState::with_max_history(device_id, max_history);
        self.selected_device = Some(detail_state);
        self.current_view = CurrentView::Device;
    }

    /// Save settings.
    fn save_settings(&mut self) {
        // Validate settings first
        if let Err(error) = self.settings.validate() {
            self.settings.set_error(error);
            return;
        }

        // Apply stale threshold immediately
        self.stale_threshold_ms = self.settings.stale_threshold_ms();

        // Apply max alerts setting
        self.alerts.set_max_alerts(self.settings.max_alerts_value());

        // Apply max history to current device view if any
        if let Some(ref mut device) = self.selected_device {
            device.set_max_history(self.settings.max_history_value());
        }

        // Update Zenoh config (will require restart to take effect)
        self.zenoh_config.mode = self.settings.zenoh_mode.as_str().to_string();
        self.zenoh_config.connect = self.settings.connect_endpoints();
        self.zenoh_config.listen = self.settings.listen_endpoints();

        // Persist settings to disk
        let persistent = PersistentSettings::from_state(&self.settings);
        if let Err(error) = persistent.save() {
            self.settings.set_error(error);
            return;
        }

        self.settings.mark_saved();
        tracing::info!("Settings saved");
    }

    /// Reset settings to defaults.
    fn reset_settings(&mut self) {
        self.settings = SettingsState::default();
        self.settings.modified = true;
    }

    /// Export current device metrics to CSV file.
    fn export_to_csv(&self) {
        if let Some(ref device) = self.selected_device {
            let csv = device.export_to_csv();
            let filename = format!(
                "zensight_{}_{}.csv",
                device.device_id.source,
                chrono_timestamp()
            );

            if let Err(e) = std::fs::write(&filename, csv) {
                tracing::error!(error = %e, filename = %filename, "Failed to export CSV");
            } else {
                tracing::info!(filename = %filename, "Exported metrics to CSV");
            }
        }
    }

    /// Export current device metrics to JSON file.
    fn export_to_json(&self) {
        if let Some(ref device) = self.selected_device {
            let json = device.export_to_json();
            let filename = format!(
                "zensight_{}_{}.json",
                device.device_id.source,
                chrono_timestamp()
            );

            if let Err(e) = std::fs::write(&filename, json) {
                tracing::error!(error = %e, filename = %filename, "Failed to export JSON");
            } else {
                tracing::info!(filename = %filename, "Exported metrics to JSON");
            }
        }
    }

    /// Handle periodic tick (update health status, etc.).
    fn handle_tick(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        for device in self.dashboard.devices.values_mut() {
            device.update_health(now, self.stale_threshold_ms);
        }

        // Apply debounced search filter
        self.dashboard.apply_pending_search();

        // Update chart time for selected device
        if let Some(ref mut device) = self.selected_device {
            device.update_chart_time();
        }
    }
}

/// Format a telemetry value as a string for the dashboard preview.
fn format_telemetry_value(value: &TelemetryValue) -> String {
    match value {
        TelemetryValue::Counter(v) => format!("{}", v),
        TelemetryValue::Gauge(v) => format!("{:.2}", v),
        TelemetryValue::Text(s) => {
            if s.len() > 30 {
                format!("{}...", &s[..27])
            } else {
                s.clone()
            }
        }
        TelemetryValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        TelemetryValue::Binary(data) => format!("<{} bytes>", data.len()),
    }
}

/// Convert a telemetry value to f64 for alert checking.
fn telemetry_to_f64(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        _ => None,
    }
}

/// Generate a timestamp string for filenames.
fn chrono_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}", now)
}
