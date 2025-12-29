//! ZenSight Iced application.

use iced::widget::Id;
use iced::widget::operation::focus;
use iced::{Element, Subscription, Task, Theme};
use std::sync::LazyLock;

use zensight_common::{
    BridgeInfo, CorrelationEntry, HealthSnapshot, Protocol, TelemetryPoint, TelemetryValue,
    ZenohConfig,
};

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
use crate::view::topology::{TopologyState, topology_view};

/// Current view in the application.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CurrentView {
    #[default]
    Dashboard,
    #[serde(skip)]
    Device,
    #[serde(skip)]
    Settings,
    Alerts,
    Topology,
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
    /// Bridge health snapshots, keyed by bridge name.
    bridge_health: std::collections::HashMap<String, HealthSnapshot>,
    /// Known bridges, keyed by bridge name.
    known_bridges: std::collections::HashMap<String, BridgeInfo>,
    /// Device correlation entries, keyed by IP address.
    correlations: std::collections::HashMap<String, CorrelationEntry>,
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
        let current_view = persistent.current_view.clone();

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
            bridge_health: std::collections::HashMap::new(),
            known_bridges: std::collections::HashMap::new(),
            correlations: std::collections::HashMap::new(),
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
                self.bridge_health.insert(snapshot.bridge.clone(), snapshot);
            }

            Message::DeviceLivenessReceived(protocol, liveness) => {
                self.handle_device_liveness(&protocol, liveness);
            }

            Message::ErrorReportReceived(report) => {
                // Log the error for now; could add an error log view later
                tracing::warn!(
                    bridge = ?report.device,
                    error_type = ?report.error_type,
                    message = %report.message,
                    "Bridge error report received"
                );
            }

            Message::BridgeInfoReceived(info) => {
                self.known_bridges.insert(info.name.clone(), info);
            }

            Message::CorrelationReceived(entry) => {
                self.correlations.insert(entry.ip.clone(), entry);
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

            Message::BridgeOnline(protocol) => {
                tracing::info!(protocol = %protocol, "Bridge online (liveliness)");
                // Bridge liveliness is informational - the bridge health system
                // already tracks bridge status via HealthSnapshot messages.
                // This provides instant notification when bridges appear.
            }

            Message::BridgeOffline(protocol) => {
                tracing::warn!(protocol = %protocol, "Bridge offline (liveliness)");
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
                self.save_current_view();
            }

            Message::CloseAlerts => {
                self.current_view = if self.selected_device.is_some() {
                    CurrentView::Device
                } else {
                    CurrentView::Dashboard
                };
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
                self.current_view = CurrentView::Topology;
                self.save_current_view();
            }

            Message::CloseTopology => {
                self.current_view = CurrentView::Dashboard;
                self.save_current_view();
            }

            Message::TopologySelectNode(node_id) => {
                // Select the node to show its info panel (don't navigate away)
                self.topology.select_node(node_id);
            }

            Message::TopologyViewDeviceDetail(node_id) => {
                // Navigate to device detail view
                if let Some(device_id) = self.topology.node_to_device_id(&node_id) {
                    self.select_device(device_id);
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
                // TODO: Send filter command to bridge via Zenoh
                // For now, just mark as applied
                self.syslog_filter.mark_applied();
                tracing::info!("Syslog filters applied (local only for now)");
            }

            Message::ClearSyslogFilters => {
                self.syslog_filter.clear();
            }

            Message::SyslogFilterStatusReceived(status) => {
                self.syslog_filter.stats = Some(status);
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
        persistent.current_view = self.current_view.clone();
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save current view: {}", e);
        }
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
            CurrentView::Topology => {
                // If something is selected, clear selection; otherwise go back to dashboard
                if self.topology.selected_node.is_some() || self.topology.selected_edge.is_some() {
                    self.topology.clear_selection();
                } else {
                    self.current_view = CurrentView::Dashboard;
                }
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
        use iced::widget::row;

        let unack = self.alerts.unacknowledged_count;

        let main_view: Element<'_, Message> = match self.current_view {
            CurrentView::Settings => settings_view(&self.settings),
            CurrentView::Alerts => alerts_view(&self.alerts),
            CurrentView::Topology => topology_view(&self.topology, self.theme),
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
                        &self.bridge_health,
                    )
                }
            }
            CurrentView::Dashboard => dashboard_view(
                &self.dashboard,
                self.theme,
                unack,
                &self.groups,
                &self.overview,
                &self.bridge_health,
            ),
        };

        // Show groups panel as a sidebar if open
        if self.groups.panel_open {
            row![main_view, groups_panel(&self.groups)].into()
        } else {
            main_view
        }
    }

    /// Get the application theme.
    pub fn theme(&self) -> Theme {
        self.theme.to_iced_theme()
    }

    /// Handle device liveness update from a bridge.
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
