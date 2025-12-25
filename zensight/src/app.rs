//! Zensight Iced application.

use iced::{Element, Subscription, Task, Theme};

use zensight_common::{TelemetryPoint, ZenohConfig};

use crate::message::{DeviceId, Message};
use crate::subscription::{tick_subscription, zenoh_subscription};
use crate::view::dashboard::{DashboardState, DeviceState, dashboard_view};
use crate::view::device::{DeviceDetailState, device_view};

/// The main Zensight application.
pub struct Zensight {
    /// Zenoh configuration.
    zenoh_config: ZenohConfig,
    /// Dashboard state.
    dashboard: DashboardState,
    /// Currently selected device (if any).
    selected_device: Option<DeviceDetailState>,
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

        let app = Self {
            zenoh_config,
            dashboard: DashboardState::default(),
            selected_device: None,
            stale_threshold_ms: 120_000, // 2 minutes
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
        match &self.selected_device {
            Some(device_state) => device_view(device_state),
            None => dashboard_view(&self.dashboard),
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
fn format_telemetry_value(value: &zensight_common::TelemetryValue) -> String {
    use zensight_common::TelemetryValue;

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
