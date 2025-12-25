//! Zensight Iced application.

use iced::{Element, Subscription, Task, Theme};

use zensight_common::{TelemetryPoint, TelemetryValue, ZenohConfig};

use crate::message::{DeviceId, Message};
use crate::subscription::{tick_subscription, zenoh_subscription};
use crate::view::alerts::{AlertsState, alerts_view};
use crate::view::dashboard::{DashboardState, DeviceState, dashboard_view};
use crate::view::device::{DeviceDetailState, device_view};
use crate::view::settings::{SettingsState, settings_view};

/// Current view in the application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CurrentView {
    #[default]
    Dashboard,
    Device,
    Settings,
    Alerts,
}

/// The main Zensight application.
pub struct Zensight {
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
}

impl Zensight {
    /// Boot the Zensight application (called by iced::application).
    pub fn boot() -> (Self, Task<Message>) {
        // Default Zenoh configuration (peer mode, connect to default router)
        let zenoh_config = ZenohConfig {
            mode: "peer".to_string(),
            connect: vec![], // Will use default discovery
            listen: vec![],
        };

        let stale_threshold_ms = 120_000; // 2 minutes

        let settings = SettingsState::from_config(
            &zenoh_config.mode,
            &zenoh_config.connect,
            &zenoh_config.listen,
            stale_threshold_ms,
        );

        let app = Self {
            zenoh_config,
            dashboard: DashboardState::default(),
            selected_device: None,
            settings,
            alerts: AlertsState::new(),
            current_view: CurrentView::default(),
            stale_threshold_ms,
        };

        (app, Task::none())
    }

    /// Get the window title.
    pub fn title(&self) -> String {
        let device_count = self.dashboard.devices.len();
        if device_count > 0 {
            format!("Zensight - {} devices", device_count)
        } else {
            "Zensight".to_string()
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
        }

        Task::none()
    }

    /// Create subscriptions for Zenoh telemetry and periodic updates.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            zenoh_subscription(self.zenoh_config.clone()),
            tick_subscription(),
        ])
    }

    /// Render the view.
    pub fn view(&self) -> Element<'_, Message> {
        match self.current_view {
            CurrentView::Settings => settings_view(&self.settings),
            CurrentView::Alerts => alerts_view(&self.alerts),
            CurrentView::Device => {
                if let Some(ref device_state) = self.selected_device {
                    device_view(device_state)
                } else {
                    dashboard_view(&self.dashboard)
                }
            }
            CurrentView::Dashboard => dashboard_view(&self.dashboard),
        }
    }

    /// Get the application theme.
    pub fn theme(&self) -> Theme {
        Theme::Dark
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
        if let Some(numeric_value) = telemetry_to_f64(&point.value) {
            if let Some(alert) =
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
        }

        // Update selected device if this telemetry is for it
        if let Some(ref mut selected) = self.selected_device {
            if selected.device_id == device_id {
                selected.update(point);
            }
        }
    }

    /// Select a device to view in detail.
    fn select_device(&mut self, device_id: DeviceId) {
        tracing::info!(device = %device_id, "Selected device");
        // We don't have the full TelemetryPoints in the dashboard,
        // so the detail view will populate as new data arrives
        let detail_state = DeviceDetailState::new(device_id);
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

        // Update Zenoh config (will require restart to take effect)
        self.zenoh_config.mode = self.settings.zenoh_mode.as_str().to_string();
        self.zenoh_config.connect = self.settings.connect_endpoints();
        self.zenoh_config.listen = self.settings.listen_endpoints();

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
