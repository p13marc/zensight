//! ZenSight Iced application.

use iced::widget::operation::focus;
use iced::widget::{Id, container};
use iced::{Element, Length, Subscription, Task, Theme};
// Note: iced_anim is available but AnimationBuilder requires Fn closures,
// which doesn't work well with view transitions. Consider using iced's
// built-in animation support or widget-level animations instead.
use std::sync::LazyLock;

use zensight_common::{
    CorrelationEntry, ErrorReport, HealthSnapshot, Protocol, SensorInfo, TelemetryPoint,
    TelemetryValue, ZenohConfig,
};

/// Flush the metric store to redb every this many 1s ticks (#22).
const STORE_FLUSH_EVERY_TICKS: u32 = 15;

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
use crate::view::device::{DeviceDetailState, device_view_with_syslog_filter};
use crate::view::groups::{GroupsState, groups_panel};
use crate::view::overview::OverviewState;
use crate::view::settings::{PersistentSettings, SettingsState, settings_view};
use crate::view::specialized::SyslogFilterState;
use crate::view::toast::{ToastSeverity, ToastState, toast_overlay};
use crate::view::topology::{TopologyState, topology_view};

/// Current view in the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CurrentView {
    #[default]
    Dashboard,
    #[serde(skip)]
    Device,
    #[serde(skip)]
    Settings,
    Alerts,
    Topology,
    Expectations,
    Security,
    Sensors,
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
    /// Groups state.
    groups: GroupsState,
    /// Overview state.
    overview: OverviewState,
    /// Topology state.
    topology: TopologyState,
    /// Syslog filter state.
    syslog_filter: SyslogFilterState,
    /// Current view.
    current_view: CurrentView,
    /// Stale threshold in milliseconds (devices not updated within this time are marked unhealthy).
    stale_threshold_ms: i64,
    /// Demo mode (use mock data instead of Zenoh).
    demo_mode: bool,
    /// Current theme.
    theme: AppTheme,
    /// Sensor health snapshots, keyed by sensor name.
    sensor_health: std::collections::HashMap<String, HealthSnapshot>,
    /// Recent error reports per sensor (bounded ring), for the Sensors view.
    recent_errors: std::collections::HashMap<String, std::collections::VecDeque<ErrorReport>>,
    /// Known sensors, keyed by sensor name.
    known_sensors: std::collections::HashMap<String, SensorInfo>,
    /// Device correlation entries, keyed by IP address.
    correlations: std::collections::HashMap<String, CorrelationEntry>,
    /// Toast notification state.
    toasts: ToastState,
    /// Live Zenoh session handle (set on connect) for sending commands to
    /// sensors. `None` while disconnected or in demo mode.
    session: Option<std::sync::Arc<zenoh::Session>>,
    /// Expectations authoring view state (netlink sentinel, Plan 08).
    expectations: crate::view::expectations::ExpectationsState,
    /// Local tiered time-series store (hot ring + redb), Plan v3-04 §A / #22.
    /// Telemetry writes through it; charts read from it so trends survive restart.
    store: crate::store::MetricStore,
    /// Ticks counted toward the next periodic store flush (flush every N ticks).
    ticks_since_flush: u32,
    /// Timestamp (epoch ms) of the most recently received telemetry point, for
    /// the global Live/Stale/Paused freshness indicator (#23). `None` until the
    /// first point arrives.
    last_telemetry_ms: Option<i64>,
    /// Global cross-device metric search panel state (#27).
    global_search: crate::view::search::GlobalSearchState,
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
            dashboard.connection_state = crate::view::dashboard::ConnectionState::Connected;
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
                    .insert(point.metric.clone(), point.clone());
                device_state.is_healthy = true;
            }
        }

        // Load theme preference
        let theme = if persistent.dark_theme {
            AppTheme::Dark
        } else {
            AppTheme::Light
        };

        // Create alerts state with configured max
        let mut alerts = AlertsState::with_max_alerts(persistent.max_alerts);
        // Load saved alert rules
        alerts.rules = persistent.alert_rules.clone();
        if demo_mode {
            use crate::demo::demo_alert_rules;
            // Add demo rules if none are saved
            if alerts.rules.is_empty() {
                for rule in demo_alert_rules() {
                    alerts.rules.push(rule);
                }
            }
            // Set shorter cooldown for demo (10 seconds instead of 60)
            alerts.alert_cooldown_ms = 10_000;
        }

        // Load groups from persistent settings
        let groups = persistent.groups.clone();

        // Load overview state from persistent settings
        let overview = OverviewState {
            selected_protocol: persistent.overview_selected_protocol,
            expanded: persistent.overview_expanded,
        };

        // Initialize topology state
        let topology = TopologyState::default();

        // Initialize syslog filter state
        let syslog_filter = SyslogFilterState::default();

        // Load last active view (only Dashboard, Alerts, Topology are persisted)
        let current_view = persistent.current_view;

        let app = Self {
            zenoh_config,
            dashboard,
            selected_device: None,
            settings,
            alerts,
            groups,
            overview,
            topology,
            syslog_filter,
            current_view,
            stale_threshold_ms,
            demo_mode,
            theme,
            sensor_health: std::collections::HashMap::new(),
            recent_errors: std::collections::HashMap::new(),
            known_sensors: std::collections::HashMap::new(),
            correlations: std::collections::HashMap::new(),
            toasts: ToastState::default(),
            session: None,
            expectations: crate::view::expectations::ExpectationsState::default(),
            // In demo mode keep history in-memory only (no disk churn / restart survival
            // for synthetic data); otherwise open the persistent tiered store.
            store: if demo_mode {
                crate::store::MetricStore::new(crate::store::DEFAULT_HOT_CAPACITY, None)
            } else {
                crate::store::MetricStore::with_default_persistence()
            },
            ticks_since_flush: 0,
            // Demo mode pre-loads mock points; treat the feed as fresh on boot.
            last_telemetry_ms: if demo_mode { Some(now_ms()) } else { None },
            global_search: crate::view::search::GlobalSearchState::default(),
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

            Message::HealthSnapshotReceived(snapshot) => {
                self.sensor_health.insert(snapshot.sensor.clone(), snapshot);
            }

            Message::DeviceLivenessReceived(protocol, liveness) => {
                self.handle_device_liveness(&protocol, liveness);
            }

            Message::ErrorReportReceived(sensor, report) => {
                tracing::warn!(
                    sensor = %sensor,
                    device = ?report.device,
                    error_type = ?report.error_type,
                    message = %report.message,
                    "Sensor error report received"
                );
                // Keep a bounded ring of recent errors per sensor for the
                // Sensors view (newest at the back).
                let ring = self.recent_errors.entry(sensor).or_default();
                ring.push_back(report);
                while ring.len() > 20 {
                    ring.pop_front();
                }
            }

            Message::SensorInfoReceived(info) => {
                self.known_sensors.insert(info.name.clone(), info);
            }

            Message::CorrelationReceived(entry) => {
                self.correlations.insert(entry.ip.clone(), entry);
            }

            Message::AlertReceived(alert) => {
                use crate::view::alerts::ExternalAlertOutcome;
                let summary = alert.summary.clone();
                let severity = alert.severity;
                match self.alerts.ingest_external(alert) {
                    ExternalAlertOutcome::New => {
                        self.toasts.push(alert_toast_severity(severity), summary);
                    }
                    ExternalAlertOutcome::Resolved => {
                        self.toasts
                            .push(ToastSeverity::Success, format!("Resolved: {summary}"));
                    }
                    ExternalAlertOutcome::Updated | ExternalAlertOutcome::Unknown => {}
                }
                if self.current_view == CurrentView::Topology {
                    self.topology.apply_alerts(&self.alerts.external);
                }
            }

            Message::AlertCleared { alert_key, .. } => {
                if let Some(alert) = self.alerts.clear_external(&alert_key) {
                    self.toasts.push(
                        ToastSeverity::Success,
                        format!("Resolved: {}", alert.summary),
                    );
                }
                if self.current_view == CurrentView::Topology {
                    self.topology.apply_alerts(&self.alerts.external);
                }
            }

            Message::AlertsSeed(alerts) => {
                // Late-joiner seed: populate the firing set without toasting (these
                // alerts fired before we connected).
                for alert in alerts {
                    self.alerts.ingest_external(alert);
                }
                if self.current_view == CurrentView::Topology {
                    self.topology.apply_alerts(&self.alerts.external);
                }
            }

            Message::Connecting => {
                tracing::info!("Connecting to Zenoh...");
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Connecting;
            }

            Message::Connected(session) => {
                tracing::info!("Connected to Zenoh");
                self.session = session;
                self.dashboard.connected = true;
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Connected;
                self.dashboard.last_error = None;
            }

            Message::Disconnected(error) => {
                tracing::warn!(error = %error, "Disconnected from Zenoh");
                self.session = None;
                self.dashboard.connected = false;
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Disconnected;
                self.dashboard.last_error = Some(error);
                // The feed is paused now; drop the freshness anchor so the
                // indicator reads "Paused", not a stale "as of" from before.
                self.last_telemetry_ms = None;
            }

            Message::SensorOnline(protocol) => {
                tracing::info!(protocol = %protocol, "Sensor online (liveliness)");
                // Sensor liveliness is informational - the sensor health system
                // already tracks sensor status via HealthSnapshot messages.
                // This provides instant notification when sensors appear.
            }

            Message::SensorOffline(protocol) => {
                tracing::warn!(protocol = %protocol, "Sensor offline (liveliness)");
                // Mark all devices from this protocol as potentially offline.
                // The health system will update their status on the next poll.
            }

            Message::DeviceOnline(protocol, device_id) => {
                tracing::debug!(protocol = %protocol, device = %device_id, "Device online (liveliness)");
                // Device came online - update its status if we're tracking it
                if let Ok(proto) = protocol.parse::<Protocol>() {
                    let dev_id = DeviceId::new(proto, &device_id);
                    if let Some(device) = self.dashboard.devices.get_mut(&dev_id) {
                        device.is_healthy = true;
                    }
                }
            }

            Message::DeviceOffline(protocol, device_id) => {
                tracing::debug!(protocol = %protocol, device = %device_id, "Device offline (liveliness)");
                // Device went offline - update its status if we're tracking it
                if let Ok(proto) = protocol.parse::<Protocol>() {
                    let dev_id = DeviceId::new(proto, &device_id);
                    if let Some(device) = self.dashboard.devices.get_mut(&dev_id) {
                        device.is_healthy = false;
                    }
                }
            }

            Message::SelectDevice(device_id) => {
                // Jumping to a device from a global-search result closes the panel.
                self.global_search.close();
                return self.select_device(device_id);
            }

            Message::InvestigateAlert { device, metric } => {
                // #35: alert → device → metric → chart in one hop.
                self.global_search.close();
                let task = self.select_device(device);
                if let Some(metric) = metric {
                    if let Some(d) = self.selected_device.as_mut() {
                        d.select_metric(metric);
                    }
                }
                return task;
            }

            Message::SelectAdjacentDevice { forward } => {
                // #35: cycle through the dashboard's current filtered set without
                // bouncing back to the dashboard each time.
                if let Some(current) = self.selected_device.as_ref().map(|d| d.device_id.clone()) {
                    let ids = self.dashboard.ordered_device_ids();
                    if let Some(pos) = ids.iter().position(|id| *id == current) {
                        if !ids.is_empty() {
                            let next = if forward {
                                (pos + 1) % ids.len()
                            } else {
                                (pos + ids.len() - 1) % ids.len()
                            };
                            if ids[next] != current {
                                return self.select_device(ids[next].clone());
                            }
                        }
                    }
                }
            }

            Message::ClearSelection => {
                self.selected_device = None;
                self.set_view(CurrentView::Dashboard);
            }

            Message::ToggleProtocolFilter(protocol) => {
                self.dashboard.toggle_filter(protocol);
            }

            Message::SetStatusFilter(status) => {
                self.dashboard.set_status_filter(status);
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

            Message::ToggleDashboardViewMode => {
                self.dashboard.toggle_view_mode();
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

            Message::AddMetricToChart(metric_name) => {
                if let Some(ref mut device) = self.selected_device {
                    device.add_metric_to_chart(metric_name);
                }
            }

            Message::RemoveMetricFromChart(metric_name) => {
                if let Some(ref mut device) = self.selected_device {
                    device.remove_metric_from_chart(&metric_name);
                }
            }

            Message::ToggleMetricVisibility(metric_name) => {
                if let Some(ref mut device) = self.selected_device {
                    device.toggle_metric_visibility(&metric_name);
                }
            }

            Message::SetChartTimeWindow(window) => {
                if let Some(ref mut device) = self.selected_device {
                    device.set_time_window(window);
                }
            }

            Message::SetChartCustomMinutes(input) => {
                if let Some(ref mut device) = self.selected_device {
                    device.set_chart_custom_minutes(input);
                }
            }

            Message::ToggleChartExpand => {
                if let Some(ref mut device) = self.selected_device {
                    device.toggle_chart_expand();
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

            Message::ChartPanLeft => {
                if let Some(ref mut device) = self.selected_device {
                    device.pan_left();
                }
            }

            Message::ChartPanRight => {
                if let Some(ref mut device) = self.selected_device {
                    device.pan_right();
                }
            }

            Message::ChartPanReset => {
                if let Some(ref mut device) = self.selected_device {
                    device.reset_pan();
                }
            }

            Message::ChartDragStart(x) => {
                if let Some(ref mut device) = self.selected_device {
                    device.start_drag(x);
                }
            }

            Message::ChartDragUpdate(x, width) => {
                if let Some(ref mut device) = self.selected_device {
                    device.update_drag(x, width);
                }
            }

            Message::ChartDragEnd => {
                if let Some(ref mut device) = self.selected_device {
                    device.end_drag();
                }
            }

            Message::SetMetricFilter(filter) => {
                if let Some(ref mut device) = self.selected_device {
                    device.set_metric_filter(filter);
                }
            }

            Message::Tick => {
                self.handle_tick();
                // Periodically flush downsampled buckets to redb off the UI thread
                // (every ~15 ticks ≈ 15s). Never block update()/view() on disk I/O.
                self.ticks_since_flush += 1;
                if self.ticks_since_flush >= STORE_FLUSH_EVERY_TICKS {
                    self.ticks_since_flush = 0;
                    if let Some((store, batch)) = self.store.take_flush_batch() {
                        return Task::future(async move {
                            // Map redb's large error to a String inside the blocking
                            // closure so the future's payload stays small.
                            let res = tokio::task::spawn_blocking(move || {
                                store.write_batch(&batch).map_err(|e| e.to_string())
                            })
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r);
                            Message::StoreFlushed(res)
                        });
                    }
                }
            }

            Message::StoreFlushed(res) => match res {
                Ok(n) => tracing::debug!(buckets = n, "Flushed metric history to store"),
                Err(e) => tracing::warn!(error = %e, "Metric store flush failed"),
            },

            Message::DeviceHistoryLoaded(device_id, series) => {
                if let Some(ref mut selected) = self.selected_device
                    && selected.device_id == device_id
                {
                    selected.seed_history(series);
                }
            }

            // Settings messages
            Message::OpenDashboard => {
                self.selected_device = None;
                self.set_view(CurrentView::Dashboard);
            }

            Message::OpenSensors => {
                self.set_view(CurrentView::Sensors);
            }

            Message::OpenSettings => {
                self.set_view(CurrentView::Settings);
            }

            Message::CloseSettings => {
                let target = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
                self.set_view(target);
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
                self.set_view(CurrentView::Alerts);
                self.save_current_view();
            }

            Message::CloseAlerts => {
                let target = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
                self.set_view(target);
                self.save_current_view();
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
                } else {
                    self.save_alert_rules();
                }
            }

            Message::TestAlertRule => {
                // Collect all current metrics from dashboard devices
                let metrics: Vec<(String, String, f64)> = self
                    .dashboard
                    .devices
                    .values()
                    .flat_map(|device| {
                        device.metrics.iter().filter_map(|(name, point)| {
                            // Extract numeric value from TelemetryPoint
                            let value = telemetry_to_f64(&point.value)?;
                            Some((device.id.source.clone(), name.clone(), value))
                        })
                    })
                    .collect();

                let _ = self.alerts.test_rule(&metrics);
            }

            Message::RemoveAlertRule(rule_id) => {
                self.alerts.remove_rule(rule_id);
                self.save_alert_rules();
            }

            Message::ToggleAlertRule(rule_id) => {
                self.alerts.toggle_rule(rule_id);
                self.save_alert_rules();
            }

            Message::AcknowledgeAlert(alert_id) => {
                self.alerts.acknowledge(alert_id);
            }

            Message::AcknowledgeAllAlerts => {
                self.alerts.acknowledge_all();
            }

            Message::AcknowledgeExternalSource(source) => {
                self.alerts.acknowledge_external_source(&source);
            }
            Message::AcknowledgeAllExternal => {
                self.alerts.acknowledge_all_external();
            }

            Message::SilenceSource(source, duration_ms) => {
                self.alerts.silence_source(&source, now_ms(), duration_ms);
                self.toasts.push(
                    ToastSeverity::Info,
                    format!("Silenced {source} for {}", fmt_duration_ms(duration_ms)),
                );
            }
            Message::UnsilenceSource(source) => {
                self.alerts.unsilence_source(&source);
                self.toasts
                    .push(ToastSeverity::Info, format!("Unsilenced {source}"));
            }

            Message::OpenGlobalSearch => {
                self.global_search.open();
                return iced::widget::operation::focus(
                    crate::view::search::GLOBAL_SEARCH_ID.clone(),
                );
            }
            Message::CloseGlobalSearch => {
                self.global_search.close();
            }
            Message::SetGlobalSearch(q) => {
                self.global_search.query = q;
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
                self.save_theme();
            }

            // Keyboard shortcuts
            Message::FocusSearch => {
                return self.focus_search();
            }

            Message::EscapePressed => {
                self.handle_escape();
            }

            // Group management messages
            Message::OpenGroupsPanel => {
                self.groups.open_panel();
            }

            Message::CloseGroupsPanel => {
                self.groups.close_panel();
            }

            Message::SetGroupFilter(group_id) => {
                self.groups.set_filter(group_id);
            }

            Message::SetNewGroupName(name) => {
                self.groups.new_group_name = name;
            }

            Message::SetNewGroupColor(index) => {
                self.groups.new_group_color = index;
            }

            Message::AddGroup => {
                self.groups.add_group_from_form();
                self.save_groups();
            }

            Message::EditGroup(group_id) => {
                self.groups.start_editing(group_id);
            }

            Message::SetEditGroupName(name) => {
                self.groups.edit_name = name;
            }

            Message::SetEditGroupColor(index) => {
                self.groups.edit_color = index;
            }

            Message::SaveGroupEdit => {
                self.groups.save_edit();
                self.save_groups();
            }

            Message::CancelGroupEdit => {
                self.groups.cancel_edit();
            }

            Message::DeleteGroup(group_id) => {
                self.groups.delete_group(group_id);
                self.save_groups();
            }

            Message::ToggleDeviceGroup(device_id, group_id) => {
                self.groups.toggle_assignment(&device_id, group_id);
                self.save_groups();
            }

            // Overview messages
            Message::SelectOverviewProtocol(protocol) => {
                self.overview.select_protocol(protocol);
                self.save_overview_state();
            }

            Message::ToggleOverviewExpanded => {
                self.overview.toggle_expanded();
                self.save_overview_state();
            }

            // Topology messages
            Message::OpenTopology => {
                // Update topology from current device data before showing
                self.topology.update_from_devices(&self.dashboard.devices);
                self.topology.apply_alerts(&self.alerts.external);
                self.topology.apply_correlations(&self.correlations);
                self.set_view(CurrentView::Topology);
                self.save_current_view();
                // Derive real edges from observed flows (#25): fetch the netring
                // flow detail; edges are applied when the reply arrives.
                return self.query_topology_flows();
            }

            Message::TopologyFlowsReceived(result) => match result {
                Ok(flows) => {
                    let ip_to_node = self.topology_ip_to_node();
                    self.topology
                        .apply_flow_edges(&flows, &ip_to_node, now_ms());
                }
                Err(e) => {
                    tracing::debug!(error = %e, "No netring flows for topology edges");
                }
            },

            Message::CloseTopology => {
                self.set_view(CurrentView::Dashboard);
                self.save_current_view();
            }

            Message::TopologySelectNode(node_id) => {
                // Select the node to show its info panel (don't navigate away)
                self.topology.select_node(node_id);
            }

            Message::TopologyViewDeviceDetail(node_id) => {
                // Navigate to device detail view
                if let Some(device_id) = self.topology.node_to_device_id(&node_id) {
                    return self.select_device(device_id);
                }
            }

            Message::TopologySelectEdge(edge_index) => {
                self.topology.select_edge(edge_index);
            }

            Message::TopologyClearSelection => {
                self.topology.clear_selection();
            }

            Message::TopologyDragNodeStart(node_id, _x, _y) => {
                self.topology.start_node_drag(&node_id);
            }

            Message::TopologyDragNodeUpdate(node_id, x, y) => {
                self.topology.update_node_drag(&node_id, x, y);
            }

            Message::TopologyDragNodeEnd(_node_id) => {
                // Node stays pinned after drag
            }

            Message::TopologyPanUpdate(dx, dy) => {
                self.topology.update_pan(dx, dy);
            }

            Message::TopologyZoomIn => {
                self.topology.zoom_in();
            }

            Message::TopologyZoomOut => {
                self.topology.zoom_out();
            }

            Message::TopologyZoomReset => {
                self.topology.reset_zoom();
            }

            Message::TopologyToggleAutoLayout => {
                self.topology.toggle_auto_layout();
            }

            Message::TopologySetSearch(query) => {
                self.topology.set_search(query);
            }

            // Syslog filter messages
            Message::ToggleSyslogFilterPanel => {
                self.syslog_filter.panel_open = !self.syslog_filter.panel_open;
            }

            Message::SetSyslogMinSeverity(severity) => {
                self.syslog_filter.set_min_severity(severity);
            }

            Message::ToggleSyslogFacility(facility) => {
                self.syslog_filter.toggle_facility(facility);
            }

            Message::SetSyslogAppFilter(filter) => {
                self.syslog_filter.set_app_filter(filter);
            }

            Message::SetSyslogMessageFilter(filter) => {
                self.syslog_filter.set_message_filter(filter);
            }

            Message::ApplySyslogFilters => {
                // Build a syslog filter command and push it to the sensor's
                // control channel. A stable filter id means re-applying replaces
                // the same dynamic filter rather than stacking duplicates.
                let f = &self.syslog_filter;
                let mut filter = serde_json::Map::new();
                if let Some(sev) = f.min_severity {
                    filter.insert("min_severity".into(), serde_json::json!(sev));
                }
                if !f.selected_facilities.is_empty() {
                    let facs: Vec<&String> = f.selected_facilities.iter().collect();
                    filter.insert("include_facilities".into(), serde_json::json!(facs));
                }
                if !f.app_filter.is_empty() {
                    filter.insert(
                        "include_app_patterns".into(),
                        serde_json::json!([{ "pattern": f.app_filter, "pattern_type": "glob" }]),
                    );
                }
                if !f.message_filter.is_empty() {
                    filter.insert(
                        "include_message_patterns".into(),
                        serde_json::json!([{ "pattern": f.message_filter, "pattern_type": "glob" }]),
                    );
                }
                let command = serde_json::json!({
                    "type": "add_filter",
                    "id": "frontend-panel",
                    "filter": serde_json::Value::Object(filter),
                });
                let key = zensight_common::command_key("zensight/syslog", "filter");
                self.syslog_filter.mark_applied();
                return self.send_command(key, &command, "Syslog filters applied".to_string());
            }

            Message::CommandFeedback { success, message } => {
                let severity = if success {
                    ToastSeverity::Success
                } else {
                    ToastSeverity::Error
                };
                self.toasts.push(severity, message);
            }

            Message::OpenExpectations => {
                self.set_view(CurrentView::Expectations);
                return self.query_expectations();
            }
            Message::CloseExpectations => {
                self.set_view(CurrentView::Dashboard);
            }
            Message::SetExpectationKind(kind) => {
                self.expectations.new_kind = kind;
            }
            Message::SetExpectationName(name) => {
                self.expectations.new_name = name;
            }
            Message::SetExpectationPort(port) => {
                self.expectations.new_port = port;
            }
            Message::SetExpectationSeverity(sev) => {
                self.expectations.new_severity = sev;
            }
            Message::SetExpectationMetric(metric) => {
                self.expectations.new_metric = metric;
            }
            Message::SetExpectationOp(op) => {
                self.expectations.new_op = op;
            }
            Message::SetExpectationValue(value) => {
                self.expectations.new_value = value;
            }
            Message::AddExpectation => {
                use crate::view::expectations::ExpKind;
                let e = &self.expectations;
                let sev = severity_str(e.new_severity);
                if e.new_name.trim().is_empty() {
                    self.toasts
                        .push(ToastSeverity::Error, "Name/interface is required");
                    return Task::none();
                }
                let command = match e.new_kind {
                    ExpKind::SocketListen | ExpKind::SocketForbid => {
                        let Ok(port) = e.new_port.trim().parse::<u16>() else {
                            self.toasts
                                .push(ToastSeverity::Error, "Port must be a number");
                            return Task::none();
                        };
                        let field = if e.new_kind == ExpKind::SocketListen {
                            "listen"
                        } else {
                            "forbid_listen"
                        };
                        serde_json::json!({
                            "type": "add_socket",
                            "name": e.new_name.trim(),
                            field: port,
                            "severity": sev,
                        })
                    }
                    ExpKind::LinkUp => serde_json::json!({
                        "type": "add_link",
                        "iface": e.new_name.trim(),
                        "up": true,
                        "severity": sev,
                    }),
                    ExpKind::MetricThreshold => {
                        if e.new_metric.trim().is_empty() {
                            self.toasts
                                .push(ToastSeverity::Error, "Metric path is required");
                            return Task::none();
                        }
                        let Ok(value) = e.new_value.trim().parse::<f64>() else {
                            self.toasts
                                .push(ToastSeverity::Error, "Value must be a number");
                            return Task::none();
                        };
                        serde_json::json!({
                            "type": "add_metric",
                            "name": e.new_name.trim(),
                            "metric": e.new_metric.trim(),
                            "op": e.new_op,
                            "value": value,
                            "severity": sev,
                        })
                    }
                };
                let key = zensight_common::command_key("zensight/netlink", "expectations");
                return self
                    .send_command(key, &command, "Expectation pushed".to_string())
                    .chain(self.query_expectations());
            }
            Message::RemoveExpectation(rule) => {
                let command = serde_json::json!({ "type": "remove", "rule": rule });
                let key = zensight_common::command_key("zensight/netlink", "expectations");
                return self
                    .send_command(key, &command, format!("Removed {rule}"))
                    .chain(self.query_expectations());
            }
            Message::RefreshExpectations => {
                return self.query_expectations();
            }
            Message::ExpectationStatusReceived(json) => {
                self.expectations.current = crate::view::expectations::parse_status(&json);
                self.expectations.status_note =
                    Some(format!("{} configured", self.expectations.current.len()));
            }

            Message::FetchNetlinkDetail(topic) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.loading(topic);
                }
                return self.query_netlink_detail(topic);
            }
            Message::NetlinkDetailReceived(topic, result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.apply(topic, result);
                }
            }

            Message::FetchNetringFlows => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading();
                }
                return self.query_netring_flows();
            }
            Message::NetringFlowsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply(result);
                }
            }
            Message::FetchNetringTls => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_tls();
                }
                return self.query_netring_tls();
            }
            Message::NetringTlsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_tls(result);
                }
            }

            Message::OpenSecurity => {
                self.set_view(CurrentView::Security);
            }
            Message::CloseSecurity => {
                self.set_view(CurrentView::Dashboard);
            }

            Message::ClearSyslogFilters => {
                self.syslog_filter.clear();
            }

            Message::SyslogFilterStatusReceived(status) => {
                self.syslog_filter.stats = Some(status);
            }

            Message::DismissToast(id) => {
                self.toasts.dismiss(id);
            }
        }

        Task::none()
    }

    /// Save groups to persistent settings.
    fn save_groups(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.groups = self.groups.clone();
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save groups: {}", e);
        }
    }

    /// Save alert rules to persistent settings.
    fn save_alert_rules(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.alert_rules = self.alerts.rules.clone();
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save alert rules: {}", e);
        }
    }

    /// Save overview state to persistent settings.
    fn save_overview_state(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.overview_selected_protocol = self.overview.selected_protocol;
        persistent.overview_expanded = self.overview.expanded;
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save overview state: {}", e);
        }
    }

    /// Save theme preference to persistent settings.
    fn save_theme(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.dark_theme = matches!(self.theme, AppTheme::Dark);
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save theme: {}", e);
        }
    }

    /// Save current view to persistent settings.
    fn save_current_view(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.current_view = self.current_view;
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save current view: {}", e);
        }
    }

    /// Set the current view.
    fn set_view(&mut self, view: CurrentView) {
        self.current_view = view;
    }

    /// Focus the appropriate search input based on current view.
    fn focus_search(&self) -> Task<Message> {
        match self.current_view {
            CurrentView::Dashboard => focus(DASHBOARD_SEARCH_ID.clone()),
            CurrentView::Device => focus(DEVICE_SEARCH_ID.clone()),
            _ => Task::none(),
        }
    }

    /// Send a command to a sensor's control channel over Zenoh.
    ///
    /// `key` is the full command key (build with
    /// [`zensight_common::command_key`]); `body` is serialized as JSON. Returns
    /// a [`Task`] that publishes asynchronously and reports the outcome via
    /// [`Message::CommandFeedback`]. No-op feedback if disconnected.
    fn send_command<T: serde::Serialize>(
        &self,
        key: String,
        body: &T,
        ok_message: String,
    ) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::done(Message::CommandFeedback {
                success: false,
                message: "Not connected to Zenoh".to_string(),
            });
        };
        let payload = match serde_json::to_vec(body) {
            Ok(p) => p,
            Err(e) => {
                return Task::done(Message::CommandFeedback {
                    success: false,
                    message: format!("Failed to encode command: {e}"),
                });
            }
        };
        Task::future(async move {
            match session.put(&key, payload).await {
                Ok(()) => Message::CommandFeedback {
                    success: true,
                    message: ok_message,
                },
                Err(e) => Message::CommandFeedback {
                    success: false,
                    message: format!("Command failed: {e}"),
                },
            }
        })
    }

    /// Query the netlink sentinel's current expectation set (status queryable).
    fn query_expectations(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let key = zensight_common::status_key("zensight/netlink", "expectations");
        Task::future(async move {
            match session.get(&key).await {
                Ok(replies) => {
                    if let Ok(reply) = replies.recv_async().await
                        && let Ok(sample) = reply.result()
                    {
                        let body =
                            String::from_utf8_lossy(&sample.payload().to_bytes()).to_string();
                        return Message::ExpectationStatusReceived(body);
                    }
                    Message::CommandFeedback {
                        success: false,
                        message: "No sentinel responded".to_string(),
                    }
                }
                Err(e) => Message::CommandFeedback {
                    success: false,
                    message: format!("Status query failed: {e}"),
                },
            }
        })
    }

    /// Fetch an on-demand netlink detail table from the sensor's query channel.
    fn query_netlink_detail(
        &self,
        topic: crate::view::specialized::netlink_detail::NetlinkDetailTopic,
    ) -> Task<Message> {
        use crate::view::specialized::netlink_detail::{
            NetlinkDetailData, NetlinkDetailTopic, fetch_records,
        };
        let Some(session) = self.session.clone() else {
            return Task::done(Message::NetlinkDetailReceived(
                topic,
                Err("Not connected to Zenoh".to_string()),
            ));
        };
        let key = topic.key();
        Task::future(async move {
            let data = match topic {
                NetlinkDetailTopic::Sockets => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Sockets),
                NetlinkDetailTopic::Routes => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Routes),
                NetlinkDetailTopic::Neighbors => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Neighbors),
            };
            let result =
                data.ok_or_else(|| format!("No netlink sensor responded for {}", topic.label()));
            Message::NetlinkDetailReceived(topic, result)
        })
    }

    /// Fetch the on-demand netring flow detail from the sensor's query channel.
    fn query_netring_flows(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_flows;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::CommandFeedback {
                success: false,
                message: "Not connected to Zenoh".to_string(),
            });
        };
        Task::future(async move {
            let result = fetch_flows(session)
                .await
                .ok_or_else(|| "No netring sensor responded".to_string());
            Message::NetringFlowsReceived(result)
        })
    }

    /// Fetch netring flows for deriving real topology edges (#25). Routes to
    /// `TopologyFlowsReceived` so the device flow panel is untouched.
    fn query_topology_flows(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_flows;
        let Some(session) = self.session.clone() else {
            // Not connected (or demo): leave edges as-is, no error toast.
            return Task::none();
        };
        Task::future(async move {
            let result = fetch_flows(session)
                .await
                .ok_or_else(|| "No netring sensor responded".to_string());
            Message::TopologyFlowsReceived(result)
        })
    }

    /// Build a map from endpoint IP to topology node id (#25). A node's `source`
    /// that is itself an IP maps directly; correlation entries map additional IPs
    /// to a hostname that matches a node.
    fn topology_ip_to_node(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        // Direct: a node whose id looks like an IP maps that IP to itself.
        for node_id in self.topology.nodes.keys() {
            map.insert(node_id.clone(), node_id.clone());
        }
        // Indirect: correlation IP -> any of its hostnames that is a known node.
        for entry in self.correlations.values() {
            if let Some(node) = entry
                .hostnames
                .iter()
                .find(|h| self.topology.nodes.contains_key(*h))
            {
                map.insert(entry.ip.clone(), node.clone());
            }
        }
        map
    }

    /// Fetch the on-demand netring TLS asset inventory.
    fn query_netring_tls(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_tls;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::CommandFeedback {
                success: false,
                message: "Not connected to Zenoh".to_string(),
            });
        };
        Task::future(async move {
            let result = fetch_tls(session)
                .await
                .ok_or_else(|| "No netring sensor responded".to_string());
            Message::NetringTlsReceived(result)
        })
    }

    /// Handle Escape key - close dialogs or go back.
    fn handle_escape(&mut self) {
        // The global search overlay takes priority: Escape closes it first (#27).
        if self.global_search.open {
            self.global_search.close();
            return;
        }
        match self.current_view {
            CurrentView::Settings => {
                let target = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
                self.set_view(target);
            }
            CurrentView::Alerts => {
                let target = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
                self.set_view(target);
            }
            CurrentView::Topology => {
                // If something is selected, clear selection; otherwise go back to dashboard
                if self.topology.selected_node.is_some() || self.topology.selected_edge.is_some() {
                    self.topology.clear_selection();
                } else {
                    self.set_view(CurrentView::Dashboard);
                }
            }
            CurrentView::Device => {
                // If charting, close chart; otherwise go back to dashboard
                if let Some(ref mut device) = self.selected_device {
                    if device.selected_metric.is_some() {
                        device.clear_chart_selection();
                    } else {
                        self.selected_device = None;
                        self.set_view(CurrentView::Dashboard);
                    }
                }
            }
            CurrentView::Expectations | CurrentView::Security | CurrentView::Sensors => {
                self.set_view(CurrentView::Dashboard);
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
        use iced::widget::{row, stack};

        // Badge counts both unacknowledged rule alerts and active sensor-pushed
        // alerts (anomalies + expectation violations).
        let unack = self.alerts.unacknowledged_count + self.alerts.external_count();

        // Precompute per-card sparkline + trend previews from the store's hot ring
        // (cheap, in-memory) only when a dashboard grid will actually render (#24).
        let on_dashboard = matches!(
            self.current_view,
            CurrentView::Dashboard | CurrentView::Device
        ) && !(self.current_view == CurrentView::Device
            && self.selected_device.is_some());
        let sparks = if on_dashboard {
            crate::view::trend::build_device_sparks(&self.store, self.dashboard.devices.keys(), 2)
        } else {
            crate::view::trend::DeviceSparks::new()
        };

        let main_view: Element<'_, Message> = match self.current_view {
            CurrentView::Settings => settings_view(&self.settings),
            CurrentView::Alerts => alerts_view(&self.alerts),
            CurrentView::Topology => topology_view(&self.topology, self.theme),
            CurrentView::Expectations => {
                crate::view::expectations::expectations_view(&self.expectations)
            }
            CurrentView::Security => crate::view::security::security_view(&self.alerts),
            CurrentView::Sensors => {
                crate::view::sensors::sensors_view(&self.sensor_health, &self.recent_errors)
            }
            CurrentView::Device => {
                if let Some(ref device_state) = self.selected_device {
                    device_view_with_syslog_filter(device_state, &self.syslog_filter)
                } else {
                    dashboard_view(
                        &self.dashboard,
                        self.theme,
                        unack,
                        &self.groups,
                        &self.overview,
                        &self.sensor_health,
                        sparks,
                    )
                }
            }
            CurrentView::Dashboard => dashboard_view(
                &self.dashboard,
                self.theme,
                unack,
                &self.groups,
                &self.overview,
                &self.sensor_health,
                sparks,
            ),
        };

        // Wrap the page in the persistent shell (left nav rail + top bar with
        // breadcrumb, alert badge, and connection status visible on every screen).
        let device_name = self
            .selected_device
            .as_ref()
            .filter(|_| self.current_view == CurrentView::Device)
            .map(|d| d.device_id.source.as_str());
        let shelled = crate::view::shell::app_shell(
            self.current_view,
            device_name,
            self.dashboard.connection_state,
            unack,
            self.last_telemetry_ms,
            now_ms(),
            main_view,
        );

        let view_container: Element<'_, Message> = container(shelled)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        // Show groups panel as a sidebar if open
        let mut base_view: Element<'_, Message> = if self.groups.panel_open {
            row![view_container, groups_panel(&self.groups)].into()
        } else {
            view_container
        };

        // Global metric search overlay (#27), centered over the current view.
        if self.global_search.open {
            let hits = crate::view::search::search(
                self.dashboard.devices.values(),
                &self.global_search.query,
            );
            let panel = container(crate::view::search::global_search_panel(
                &self.global_search,
                hits,
            ))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill);
            base_view = stack![base_view, panel].into();
        }

        // Overlay toast notifications in the bottom-right corner
        if !self.toasts.is_empty() {
            let toasts = container(toast_overlay(&self.toasts))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_right(Length::Shrink)
                .align_bottom(Length::Shrink)
                .padding(20);

            stack![base_view, toasts].into()
        } else {
            base_view
        }
    }

    /// Get the application theme.
    pub fn theme(&self) -> Theme {
        self.theme.to_iced_theme()
    }

    /// Handle device liveness update from a sensor.
    fn handle_device_liveness(
        &mut self,
        protocol_str: &str,
        liveness: zensight_common::DeviceLiveness,
    ) {
        // Parse protocol from string
        let protocol = match protocol_str {
            "snmp" => Protocol::Snmp,
            "syslog" => Protocol::Syslog,
            "gnmi" => Protocol::Gnmi,
            "netflow" => Protocol::Netflow,
            "opcua" => Protocol::Opcua,
            "modbus" => Protocol::Modbus,
            "sysinfo" => Protocol::Sysinfo,
            _ => return, // Unknown protocol, ignore
        };

        let device_id = DeviceId::new(protocol, &liveness.device);

        // Update the device state if it exists
        if let Some(device_state) = self.dashboard.devices.get_mut(&device_id) {
            device_state.update_from_liveness(
                liveness.status,
                liveness.consecutive_failures,
                liveness.last_error,
            );
        }
        // Note: We don't create new devices from liveness data alone
        // They should be created when telemetry arrives
    }

    /// Handle incoming telemetry.
    fn handle_telemetry(&mut self, point: TelemetryPoint) {
        // Write through to the local tiered store (O(1) hot-ring append; numeric
        // values only). Charts/trends read back from here so history survives restart.
        self.store.record(&point);

        // Track the newest point for the global freshness verdict (#23).
        self.last_telemetry_ms = Some(
            self.last_telemetry_ms
                .map_or(point.timestamp, |prev| prev.max(point.timestamp)),
        );

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
            .insert(point.metric.clone(), point.clone());
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

        // Update topology if we're viewing it
        if self.current_view == CurrentView::Topology {
            self.topology.update_from_devices(&self.dashboard.devices);
        }
    }

    /// Select a device to view in detail. Returns a task that pre-loads this
    /// device's restart-survived history from the local store off the UI thread
    /// (#22), so the detail chart opens pre-populated with persisted trends.
    fn select_device(&mut self, device_id: DeviceId) -> Task<Message> {
        tracing::info!(device = %device_id, "Selected device");
        // We don't have the full TelemetryPoints in the dashboard,
        // so the detail view will populate as new data arrives
        let max_history = self.settings.max_history_value();
        let detail_state = DeviceDetailState::with_max_history(device_id.clone(), max_history);
        self.selected_device = Some(detail_state);
        self.set_view(CurrentView::Device);

        // Resolve the persisted metric ids for this device, then query the warm
        // (minute) tier off-thread. Last 24h of minute buckets is plenty to
        // pre-populate a chart without blocking the UI.
        let Some(store) = self.store.persistent() else {
            return Task::none();
        };
        let protocol = device_id.protocol.to_string();
        let metric_ids = self.store.device_metric_ids(&protocol, &device_id.source);
        if metric_ids.is_empty() {
            return Task::none();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let from = now - 24 * 3_600_000; // 24h window
        Task::future(async move {
            let series = tokio::task::spawn_blocking(move || {
                metric_ids
                    .into_iter()
                    .filter_map(|(name, id)| {
                        store
                            .query(id, crate::store::Tier::Minute, from, now)
                            .ok()
                            .filter(|s| !s.is_empty())
                            .map(|samples| (name, samples))
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            Message::DeviceHistoryLoaded(device_id, series)
        })
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

        // Update the Zenoh config. The live subscription is keyed on this config
        // (`Subscription::run_with(zenoh_config, …)`), so changing it makes Iced
        // tear down the current session and reconnect with the new settings — no
        // restart needed. We surface that to the user instead of doing it
        // silently (#38).
        let new_mode = self.settings.zenoh_mode.as_str().to_string();
        let new_connect = self.settings.connect_endpoints();
        let new_listen = self.settings.listen_endpoints();
        let connection_changed = self.zenoh_config.mode != new_mode
            || self.zenoh_config.connect != new_connect
            || self.zenoh_config.listen != new_listen;
        self.zenoh_config.mode = new_mode;
        self.zenoh_config.connect = new_connect;
        self.zenoh_config.listen = new_listen;

        if connection_changed && !self.demo_mode {
            // Reflect the impending reconnect immediately; the restarted
            // subscription will drive Connecting → Connected/Disconnected.
            self.dashboard.connection_state =
                crate::view::dashboard::ConnectionState::Connecting;
            self.dashboard.connected = false;
            self.toasts.push(
                ToastSeverity::Info,
                "Reconnecting to Zenoh with new connection settings…",
            );
        }

        // Persist settings to disk (include all app state)
        let mut persistent = PersistentSettings::from_state(&self.settings);
        persistent.groups = self.groups.clone();
        persistent.alert_rules = self.alerts.rules.clone();
        persistent.overview_selected_protocol = self.overview.selected_protocol;
        persistent.overview_expanded = self.overview.expanded;
        if let Err(error) = persistent.save() {
            self.settings.set_error(error);
            return;
        }

        self.settings.mark_saved();
        tracing::info!("Settings saved");
        self.toasts.push(ToastSeverity::Success, "Settings saved");
    }

    /// Reset settings to defaults.
    fn reset_settings(&mut self) {
        self.settings = SettingsState::default();
        self.settings.modified = true;
    }

    /// Export current device metrics to CSV file.
    fn export_to_csv(&mut self) {
        if let Some(ref device) = self.selected_device {
            // Prefer the full time series (the trend on screen, #37); fall back
            // to the latest-value snapshot only when no history exists yet.
            let csv = if device.has_history() {
                device.export_history_to_csv()
            } else {
                device.export_to_csv()
            };
            let filename = format!(
                "zensight_{}_{}.csv",
                device.device_id.source,
                chrono_timestamp()
            );
            self.write_export(&filename, csv);
        }
    }

    /// Export current device time series to JSON file.
    fn export_to_json(&mut self) {
        if let Some(ref device) = self.selected_device {
            let json = if device.has_history() {
                device.export_history_to_json()
            } else {
                device.export_to_json()
            };
            let filename = format!(
                "zensight_{}_{}.json",
                device.device_id.source,
                chrono_timestamp()
            );
            self.write_export(&filename, json);
        }
    }

    /// Write an export to a discoverable directory and toast the absolute path
    /// (#37) — no more blind writes to the process CWD where files get lost.
    fn write_export(&mut self, filename: &str, contents: String) {
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let path = dir.join(filename);
        match std::fs::write(&path, contents) {
            Ok(()) => {
                let shown = path.display().to_string();
                tracing::info!(path = %shown, "Exported device data");
                self.toasts
                    .push(ToastSeverity::Success, format!("Exported to {shown}"));
            }
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "Export failed");
                self.toasts
                    .push(ToastSeverity::Error, format!("Export failed: {e}"));
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

        // Expire alert silences whose window has passed (#26).
        self.alerts.prune_silences(now);

        // Apply debounced search filter
        self.dashboard.apply_pending_search();

        // Update chart time for selected device
        if let Some(ref mut device) = self.selected_device {
            device.update_chart_time();
        }

        // Clean up expired toasts
        self.toasts.cleanup_expired();

        // Update topology when viewing it
        if self.current_view == CurrentView::Topology {
            self.topology.update_from_devices(&self.dashboard.devices);
            // Run layout algorithm if not stable
            if !self.topology.layout_stable {
                self.topology.run_layout_step();
            }
        }
    }
}

/// Current wall-clock time in epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Human duration for silence toasts: "1h" / "4h" / "24h" / "30m".
fn fmt_duration_ms(ms: i64) -> String {
    let mins = ms / 60_000;
    if mins % 60 == 0 {
        format!("{}h", mins / 60)
    } else {
        format!("{mins}m")
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

/// Lowercase wire string for a frontend severity (matches common::AlertSeverity).
fn severity_str(s: crate::view::alerts::Severity) -> &'static str {
    use crate::view::alerts::Severity;
    match s {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Critical => "critical",
    }
}

/// Map a sensor alert severity onto a toast severity.
fn alert_toast_severity(severity: zensight_common::AlertSeverity) -> ToastSeverity {
    use zensight_common::AlertSeverity;
    match severity {
        AlertSeverity::Info => ToastSeverity::Info,
        AlertSeverity::Warning => ToastSeverity::Warning,
        AlertSeverity::Critical => ToastSeverity::Error,
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
