//! ZenSight Iced application.

use iced::widget::operation::focus;
use iced::widget::{Id, container};
use iced::{Element, Length, Subscription, Task, Theme};
// Note: iced_anim is available but AnimationBuilder requires Fn closures,
// which doesn't work well with view transitions. Consider using iced's
// built-in animation support or widget-level animations instead.
use std::ops::ControlFlow;
use std::sync::LazyLock;

use zensight_common::{
    ErrorReport, HealthSnapshot, HealthStatus, Protocol, SensorInfo, TelemetryPoint,
    TelemetryValue, ZenohConfig,
};

/// Flush the metric store to redb every this many 1s ticks (#22).
const STORE_FLUSH_EVERY_TICKS: u32 = 15;

/// Re-issue the topology edge/asset queries every N ticks (~seconds) while the
/// topology view is open (#391), so edge rates stay live instead of freezing
/// at whatever the view-open fetch saw.
const TOPOLOGY_REFRESH_TICKS: u8 = 10;

/// Evict aged-out buckets every this many flushes (~10 min at 15s/flush, #131).
/// Pruning scans the whole table, so it runs far less often than flushing.
const STORE_PRUNE_EVERY_FLUSHES: u32 = 40;

/// Reduce an `ip:port` (or bracketed `[ipv6]:port`, or bare `ip`) endpoint to its
/// bare IP, for matching a flow endpoint against an anomaly's source (#119).
fn endpoint_ip(endpoint: &str) -> String {
    if let Ok(sa) = endpoint.parse::<std::net::SocketAddr>() {
        return sa.ip().to_string();
    }
    if let Ok(ip) = endpoint.parse::<std::net::IpAddr>() {
        return ip.to_string();
    }
    match endpoint.rsplit_once(':') {
        Some((host, _port)) => host.trim_matches(['[', ']']).to_string(),
        None => endpoint.to_string(),
    }
}

/// Cap on the rolling log buffer feeding the top-level Logs view.
const MAX_RECENT_LOGS: usize = 5000;

/// Minimum gap between `@rpc/logs/events` fetches while a logs surface is open
/// (#358). A log *viewer* cadence — not tail -f; tune here if needed.
const LOG_REFRESH_SECS: i64 = 5;

/// Fetch overlap subtracted from the newest-seen event timestamp when building
/// the `since=` selector (#358): 2× the refresh period, so sensor/GUI clock
/// skew or a slow tick never opens a gap. Overlapping records de-dup on merge.
const LOG_FETCH_OVERLAP_MS: i64 = 10_000;

/// Reply cap requested per `@rpc/logs/events` fetch (#358).
const LOG_FETCH_MAX: usize = 500;

/// Text input ID for dashboard search.
pub static DASHBOARD_SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("dashboard-search"));

/// Text input ID for device metric search.
pub static DEVICE_SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("device-search"));

use crate::entity::EntityStore;
use crate::message::{DeviceId, Message};
use crate::mock;
use crate::subscription::{
    demo_subscription, keyboard_subscription, tick_subscription, zenoh_subscription,
};
use crate::view::alerts::{AlertsState, alerts_view};
use crate::view::dashboard::{DashboardState, DeviceState, dashboard_view};
use crate::view::device::DeviceDetailState;
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
    Logs,
    Inventory,
    Incidents,
    Bandwidth,
    Fleet,
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
    /// Zenoh connection + subscription scope + link profile (#364). The live
    /// subscription is keyed on this whole value, so any change restarts it.
    link: crate::subscription::LinkConfig,
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
    /// Correlator host-entity view model (#306): groups per-protocol devices
    /// into physical hosts. Empty ⇒ degraded per-source path.
    entities: EntityStore,
    /// Syslog filter state.
    syslog_filter: SyslogFilterState,
    /// Rolling buffer of recent log lines (all syslog/journald sources) for the
    /// top-level Logs view. Bounded to [`MAX_RECENT_LOGS`].
    recent_logs: std::collections::VecDeque<crate::view::specialized::SyslogMessage>,
    /// Newest event timestamp seen from `@rpc/logs/events` (#358) — the `since=`
    /// watermark for incremental fetches (minus [`LOG_FETCH_OVERLAP_MS`]).
    last_log_event_ms: Option<i64>,
    /// When the last `@rpc/logs/events` fetch was issued (#358) — cadence gate.
    last_log_fetch_ms: Option<i64>,
    /// An `@rpc/logs/events` fetch is in flight (#358) — never stack fetches.
    log_fetch_inflight: bool,
    /// Whether the host identity details (facts + resolution group) are
    /// expanded in the merged host nav bar (#350). Persisted.
    identity_expanded: bool,
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
    /// source (payload host label, e.g. hostname) → v1 origin id (`h-<12hex>`),
    /// learned from health/registration/entity docs. The wire payloads keep
    /// human-readable sources while keys are origin-scoped — this map is how
    /// drill-down fetches target the right host's @rpc/@media keys.
    origins: std::collections::HashMap<String, String>,
    /// Recent error reports per sensor (bounded ring), for the Sensors view.
    recent_errors: std::collections::HashMap<String, std::collections::VecDeque<ErrorReport>>,
    /// Known sensor instances, keyed by `<name>@<source>` (one sensor can run
    /// on many hosts).
    known_sensors: std::collections::HashMap<String, SensorInfo>,
    /// Toast notification state.
    toasts: ToastState,
    /// Live Zenoh session handle (set on connect) for sending commands to
    /// sensors. `None` while disconnected or in demo mode.
    session: Option<std::sync::Arc<zenoh::Session>>,
    /// Declared-publisher registry for outbound commands (declare-on-first-use +
    /// cache per key) — set on connect, so command sends never use a one-shot
    /// `session.put`. `None` while disconnected or in demo mode.
    command_registry: Option<std::sync::Arc<zensight_common::PublisherRegistry>>,
    /// In-flight artifact download state (report / snapshot / capture).
    artifact_fetch: crate::view::artifact_fetch::ArtifactFetch,
    /// The in-flight download's identity (key prefix, kind, id, delivery, dest).
    artifact_job: Option<crate::view::artifact_fetch::ArtifactJob>,
    /// Per-sensor advertised artifact kinds (`producer` → kinds + bounds/adverts).
    artifact_kinds: std::collections::HashMap<String, Vec<zensight_common::KindStatus>>,
    /// Per-sensor on-demand capture form state (`producer` → form), shared by
    /// the sensor card and the netring Capture tab (#333).
    capture_forms: std::collections::HashMap<String, crate::view::artifact_fetch::CaptureForm>,
    /// Expectations authoring view state (netlink sentinel, Plan 08).
    expectations: crate::view::expectations::ExpectationsState,
    /// Security view state: severity filter + expanded anomaly (#48).
    security: crate::view::security::SecurityState,
    /// Netring detection-tuning panel state (#121), shown in the Security view.
    detection_tuning: crate::view::detection_tuning::DetectionTuningState,
    /// First-class passive inventory + fingerprint explorer state (#120).
    inventory: crate::view::inventory::InventoryState,
    /// Incidents triage view state (#129): which incident is expanded.
    incidents: crate::view::incident::IncidentsState,
    /// Bandwidth live-monitor state (#319, epic #320): per-process (queried) and
    /// per-service (streamed) network rate.
    bandwidth: crate::view::bandwidth::BandwidthState,
    /// Fleet capabilities: what each host's build says it serves (#469).
    fleet: crate::view::fleet::FleetState,
    /// Local tiered time-series store (hot ring + redb), Plan v3-04 §A / #22.
    /// Telemetry writes through it; charts read from it so trends survive restart.
    store: crate::store::MetricStore,
    /// Ticks counted toward the next periodic store flush (flush every N ticks).
    ticks_since_flush: u32,
    /// Ticks since the last topology query refresh (#391).
    topology_refresh_ticks: u8,
    /// Topology prefs changed but not yet written to settings.json5 (#440):
    /// flushed by the 1 Hz tick / on leaving the view, never per interaction.
    topology_prefs_dirty: bool,
    /// Flushes counted toward the next store prune (#131).
    flushes_since_prune: u32,
    /// Timestamp (epoch ms) of the most recently received telemetry point, for
    /// the global Live/Stale/Paused freshness indicator (#23). `None` until the
    /// first point arrives.
    last_telemetry_ms: Option<i64>,
    /// Global cross-device metric search panel state (#27).
    global_search: crate::view::search::GlobalSearchState,
    /// Command palette overlay state (#28).
    command_palette: crate::view::palette::CommandPaletteState,
    /// Whether the keyboard-shortcuts help overlay is open (#28).
    help_open: bool,
    /// Favorited metrics (#27), keyed `protocol/source/metric`. Persisted; the
    /// per-device projection is pushed into the device detail state on selection.
    favorites: std::collections::HashSet<String>,
    /// Cached dashboard-card sparklines, rebuilt once per tick (not per frame).
    /// `view()` used to call `build_device_sparks` on every render; under a high
    /// telemetry rate that pegged the single UI thread (the startup freeze).
    /// Now the (indexed, O(device)) build runs at 1 Hz in `handle_tick`, and the
    /// render just clones this small truncated result.
    dashboard_sparks: crate::view::trend::DeviceSparks,
    /// Firing external-alert counts keyed by source, for the dashboard host-card
    /// alert rollup (#306). Rebuilt at 1 Hz in `handle_tick`.
    firing_by_source: std::collections::HashMap<String, usize>,
}

impl ZenSight {
    /// Boot the ZenSight application (called by iced::application).
    pub fn boot(demo_mode: bool) -> (Self, Task<Message>) {
        // Load persistent settings from disk
        let persistent = PersistentSettings::load();

        // Build Zenoh configuration from loaded settings, then apply
        // `ZENSIGHT_ZENOH_*` env overrides so a launcher (e.g. `just run`) can
        // pin explicit local endpoints instead of relying on multicast discovery.
        let zenoh_config = ZenohConfig {
            mode: persistent.zenoh_mode.clone(),
            connect: persistent.zenoh_connect.clone(),
            listen: persistent.zenoh_listen.clone(),
            scouting: true,
        }
        .with_env_overrides();
        let link = crate::subscription::LinkConfig {
            zenoh: zenoh_config,
            scope: persistent.subscription_scope.clone(),
            profile: persistent.link_profile,
            // Focus is runtime-only: a GUI that came up already focused would
            // look like a GUI that had lost the fleet.
            focus: None,
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
        // Load saved alert-filter presets (#27)
        alerts.alert_filter_presets = persistent.alert_filter_presets.clone();
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

        // Initialize topology state with persisted presentation prefs (#392).
        let topology = {
            let mut t = TopologyState::default();
            t.prefs.lens = persistent.topology_lens;
            t.prefs.grouping = persistent.topology_grouping;
            t.prefs.edge_label = persistent.topology_edge_label;
            t.prefs.filters = persistent.topology_filters;
            t.prefs.layout = persistent.topology_layout;
            t.saved_positions = persistent.topology_positions.clone();
            t.saved_pins = persistent.topology_pinned.iter().cloned().collect();
            t
        };

        // Initialize syslog filter state
        let syslog_filter = SyslogFilterState::default();

        // Load last active view (only Dashboard, Alerts, Topology are persisted)
        let current_view = persistent.current_view;

        let app = Self {
            link,
            dashboard,
            selected_device: None,
            settings,
            alerts,
            groups,
            overview,
            topology,
            entities: {
                // Demo mode: seed correlator host entities so the dashboard shows
                // merged host cards immediately (the demo feed re-emits them).
                let mut store = EntityStore::default();
                if demo_mode {
                    store.seed(mock::host_entities());
                }
                store
            },
            syslog_filter,
            recent_logs: std::collections::VecDeque::new(),
            last_log_event_ms: None,
            last_log_fetch_ms: None,
            log_fetch_inflight: false,
            identity_expanded: persistent.identity_expanded,
            current_view,
            stale_threshold_ms,
            demo_mode,
            theme,
            sensor_health: std::collections::HashMap::new(),
            origins: std::collections::HashMap::new(),
            recent_errors: std::collections::HashMap::new(),
            known_sensors: std::collections::HashMap::new(),
            toasts: ToastState::default(),
            session: None,
            command_registry: None,
            artifact_fetch: crate::view::artifact_fetch::ArtifactFetch::default(),
            artifact_job: None,
            artifact_kinds: std::collections::HashMap::new(),
            capture_forms: std::collections::HashMap::new(),
            expectations: crate::view::expectations::ExpectationsState::default(),
            security: crate::view::security::SecurityState::default(),
            detection_tuning: crate::view::detection_tuning::DetectionTuningState::default(),
            inventory: crate::view::inventory::InventoryState::default(),
            incidents: crate::view::incident::IncidentsState::default(),
            bandwidth: crate::view::bandwidth::BandwidthState::default(),
            fleet: crate::view::fleet::FleetState::default(),
            // In demo mode keep history in-memory only (no disk churn / restart survival
            // for synthetic data); otherwise open the persistent tiered store.
            store: if demo_mode {
                crate::store::MetricStore::new(crate::store::DEFAULT_HOT_CAPACITY, None)
            } else {
                crate::store::MetricStore::with_default_persistence()
            },
            ticks_since_flush: 0,
            topology_refresh_ticks: 0,
            topology_prefs_dirty: false,
            flushes_since_prune: 0,
            // Demo mode pre-loads mock points; treat the feed as fresh on boot.
            last_telemetry_ms: if demo_mode { Some(now_ms()) } else { None },
            global_search: crate::view::search::GlobalSearchState::default(),
            command_palette: crate::view::palette::CommandPaletteState::default(),
            help_open: false,
            favorites: persistent.favorite_metrics.iter().cloned().collect(),
            dashboard_sparks: crate::view::trend::DeviceSparks::new(),
            firing_by_source: std::collections::HashMap::new(),
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
    /// #132: chart / metric-selection interactions, all scoped to the selected device.
    ///
    /// Returns `Err(message)` for anything it does not own so [`Self::update`]
    /// can fall through to the next handler.
    fn update_chart(&mut self, message: Message) -> ControlFlow<Task<Message>, Message> {
        match message {
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

            Message::ToggleMetricFavorite(metric) => {
                if let Some(ref mut device) = self.selected_device {
                    let now_fav = device.toggle_favorite(&metric);
                    let key = fav_key(&device.device_id, &metric);
                    if now_fav {
                        self.favorites.insert(key);
                    } else {
                        self.favorites.remove(&key);
                    }
                    self.save_favorites();
                }
            }

            Message::PromoteMetricToAlert {
                device,
                metric,
                value,
            } => {
                // #50: netlink has a sentinel that evaluates metric thresholds,
                // so promote into the expectations authoring form. Other sensors
                // have no command channel, so seed the local rule engine instead.
                if device.protocol == zensight_common::Protocol::Netlink {
                    use crate::view::expectations::ExpKind;
                    self.expectations.new_kind = ExpKind::MetricThreshold;
                    self.expectations.new_metric = metric.clone();
                    self.expectations.new_value = format!("{value}");
                    self.expectations.new_name = format!("{} threshold", metric);
                    self.set_view(CurrentView::Expectations);
                } else {
                    self.alerts.set_new_rule_name(format!("{metric} alert"));
                    self.alerts.set_new_rule_metric(metric);
                    self.alerts.set_new_rule_threshold(format!("{value}"));
                    self.set_view(CurrentView::Alerts);
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

            Message::SetChartRangeFrom(input) => {
                if let Some(ref mut device) = self.selected_device {
                    device.chart_from_input = input;
                }
            }

            Message::SetChartRangeTo(input) => {
                if let Some(ref mut device) = self.selected_device {
                    device.chart_to_input = input;
                }
            }

            Message::ApplyChartRange => {
                // Pin the absolute window, then range-query the store so the chart
                // shows that exact slice (not just whatever the hot ring holds).
                if let Some((from, to)) = self
                    .selected_device
                    .as_mut()
                    .and_then(|d| d.apply_chart_range())
                {
                    let device_id = self.selected_device.as_ref().unwrap().device_id.clone();
                    return ControlFlow::Break(self.load_device_history_range(device_id, from, to));
                }
                self.toasts.push(
                    ToastSeverity::Warning,
                    "Enter a valid from/to range (YYYY-MM-DD HH:MM, from before to)".to_string(),
                );
            }

            Message::ClearChartRange => {
                if let Some(ref mut device) = self.selected_device {
                    device.clear_chart_range();
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
            other => return ControlFlow::Continue(other),
        }
        ControlFlow::Break(Task::none())
    }

    /// #132: device-group management.
    ///
    /// Returns `Err(message)` for anything it does not own so [`Self::update`]
    /// can fall through to the next handler.
    fn update_groups(&mut self, message: Message) -> ControlFlow<Task<Message>, Message> {
        match message {
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
            other => return ControlFlow::Continue(other),
        }
        ControlFlow::Break(Task::none())
    }

    /// #132: topology canvas interactions plus flow / neighbor edge replies.
    ///
    /// Returns `Err(message)` for anything it does not own so [`Self::update`]
    /// can fall through to the next handler.
    fn update_topology_msg(&mut self, message: Message) -> ControlFlow<Task<Message>, Message> {
        match message {
            Message::TopologyBatchReceived(batch) => {
                // One reply set → one edge rebuild + one redraw (#440); the
                // four sources used to land as separate messages, each
                // clearing the canvas cache in turn.
                tracing::debug!(
                    flows = batch.flows.is_some(),
                    neighbors = batch.neighbors.is_some(),
                    matrix = batch.matrix.is_some(),
                    assets = batch.assets.is_some(),
                    "Topology batch received"
                );
                let ip_to_node = self.topology_ip_to_node();
                let mac_to_node = self.topology_mac_to_node();
                self.topology
                    .apply_batch(batch, &mac_to_node, &ip_to_node, now_ms());
            }

            Message::CloseTopology => {
                // Leaving the view: land any debounced pref changes now (#440).
                self.flush_topology_prefs();
                self.set_view(CurrentView::Dashboard);
                self.save_current_view();
            }

            Message::TopologySelectNode(node_id) => {
                // Select the node to show its info panel (don't navigate away)
                self.topology.select_node(node_id.clone());
                return ControlFlow::Break(self.query_topology_listen_sockets(node_id));
            }

            Message::TopologyViewDeviceDetail(node_id) => {
                // Navigate to device detail view
                if let Some(device_id) = self.topology.node_to_device_id(&node_id) {
                    return ControlFlow::Break(self.select_device(device_id));
                }
            }

            Message::TopologySelectEdge(edge_index) => {
                self.topology.select_edge(edge_index);
                return ControlFlow::Break(self.query_topology_edge_flows(edge_index));
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
                // Node stays pinned after drag; persist the arrangement (#394).
                self.save_topology_prefs();
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

            Message::TopologySetLens(lens) => {
                self.topology.set_lens(lens);
                self.save_topology_prefs();
            }

            Message::TopologySetEdgeLabel(mode) => {
                self.topology.set_edge_label(mode);
                self.save_topology_prefs();
            }

            Message::TopologySetGrouping(mode) => {
                self.topology.set_grouping(mode);
                self.save_topology_prefs();
            }

            Message::TopologyExpandGroup(group_id) => {
                self.topology.expand_group(group_id);
            }

            Message::TopologyRegroup => {
                self.topology.regroup();
            }

            Message::TopologyFocusNode(node_id) => {
                self.topology.focus_node(node_id);
            }

            Message::TopologySetFocusHops(hops) => {
                self.topology.set_focus_hops(hops);
            }

            Message::TopologyExitFocus => {
                self.topology.exit_focus();
            }

            Message::TopologyToggleHideIdle => {
                self.topology.toggle_hide_idle();
                self.save_topology_prefs();
            }

            Message::TopologyToggleHidePassive => {
                self.topology.toggle_hide_passive();
                self.save_topology_prefs();
            }

            Message::TopologyToggleHideExternal => {
                self.topology.toggle_hide_external();
                self.save_topology_prefs();
            }

            Message::TopologySetTopN(n) => {
                self.topology.set_top_n(n);
                self.save_topology_prefs();
            }

            Message::TopologyListenSocketsReceived(node_id, result) => {
                use crate::view::specialized::fetch::Fetch;
                // Staleness guard (#393): drop replies for a stale selection.
                if self.topology.selected_node.as_ref() == Some(&node_id) {
                    self.topology.panel.listen = match result {
                        Ok(rows) => {
                            let node_ips: std::collections::HashSet<&str> = self
                                .topology
                                .nodes
                                .get(&node_id)
                                .map(|n| n.ips.iter().map(String::as_str).collect())
                                .unwrap_or_default();
                            // Keep rows bound to this host's addresses, plus
                            // wildcard listeners (0.0.0.0 / [::]) — those are
                            // usually what you're looking for, but on a
                            // multi-host mesh they may belong to any netlink
                            // host; the panel says so.
                            let filtered: Vec<_> = rows
                                .into_iter()
                                .filter(|s| {
                                    let ip =
                                        crate::view::topology::endpoint_ip(&s.local).to_string();
                                    node_ips.contains(ip.as_str())
                                        || ip == "0.0.0.0"
                                        || ip == "::"
                                        || ip == "*"
                                })
                                .collect();
                            Fetch::Ready(filtered)
                        }
                        Err(e) => Fetch::Error(e),
                    };
                }
            }

            Message::TopologyEdgeFlowsReceived(edge_index, result) => {
                use crate::view::specialized::fetch::Fetch;
                if self.topology.selected_edge == Some(edge_index) {
                    self.topology.panel.edge_flows = match result {
                        Ok(flows) => {
                            let filtered = self.filter_flows_to_edge(edge_index, flows);
                            Fetch::Ready(filtered)
                        }
                        Err(e) => Fetch::Error(e),
                    };
                }
            }

            Message::TopologyCopyText(text) => {
                return ControlFlow::Break(iced::clipboard::write(text));
            }

            Message::TopologySetLayout(mode) => {
                self.topology.set_layout(mode);
                self.save_topology_prefs();
            }

            Message::TopologyTogglePin(node_id) => {
                self.topology.toggle_pin(&node_id);
                self.save_topology_prefs();
            }

            Message::TopologyFitApplied { zoom, pan } => {
                self.topology.apply_fit(zoom, pan);
            }

            Message::TopologyHover(node_id) => {
                self.topology.set_hover(node_id);
            }

            Message::TopologyAnimTick => {
                self.topology.advance_animation();
            }

            Message::TopologyLayoutFrame => {
                if self.topology.tween_active() {
                    self.topology.step_tween(now_ms());
                } else {
                    self.topology.run_layout_step();
                }
            }

            Message::TopologyToggleLegend => {
                self.topology.toggle_legend();
            }

            Message::TopologyOpenFlows => {
                // Pivot to the netring flow table (#393): the first netring
                // device's detail view, Flows tab.
                let netring_device = self
                    .dashboard
                    .devices
                    .keys()
                    .find(|d| d.protocol == zensight_common::Protocol::Netring)
                    .cloned();
                if let Some(device_id) = netring_device {
                    let task = self.select_device(device_id);
                    if let Some(ref mut device) = self.selected_device {
                        device.specialized_tab = crate::view::specialized::SpecializedTab::Flows;
                    }
                    return ControlFlow::Break(task);
                }
                self.toasts.push(
                    crate::view::toast::ToastSeverity::Info,
                    "No netring sensor available for the flow table",
                );
            }
            other => return ControlFlow::Continue(other),
        }
        ControlFlow::Break(Task::none())
    }

    /// #132: syslog/journald filter panel and its apply-to-sensor command.
    ///
    /// Returns `Err(message)` for anything it does not own so [`Self::update`]
    /// can fall through to the next handler.
    fn update_syslog(&mut self, message: Message) -> ControlFlow<Task<Message>, Message> {
        match message {
            // Syslog filter messages
            Message::ToggleSyslogFilterPanel => {
                self.syslog_filter.panel_open = !self.syslog_filter.panel_open;
            }

            Message::ToggleLogStatsPanel => {
                self.syslog_filter.stats_open = !self.syslog_filter.stats_open;
            }

            Message::ToggleLogStatsAllUnits => {
                self.syslog_filter.stats_all_units = !self.syslog_filter.stats_all_units;
            }

            Message::SetSyslogMinSeverity(severity) => {
                self.syslog_filter.set_min_severity(severity);
            }

            Message::ToggleSyslogFacility(facility) => {
                self.syslog_filter.toggle_facility(facility);
            }

            Message::ToggleSyslogUnit(unit) => {
                self.syslog_filter.toggle_unit(unit);
            }

            Message::ToggleSyslogBoot(boot) => {
                self.syslog_filter.toggle_boot(boot);
            }

            Message::ToggleLogRow(key) => {
                self.syslog_filter.toggle_row(key);
            }

            Message::ToggleLogFollow => {
                self.syslog_filter.toggle_follow(now_ms());
            }

            Message::LogsJumpToNow => {
                self.syslog_filter.resume();
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
                let key = zensight_common::fleet_command_key("logs", "filter");
                self.syslog_filter.mark_applied();
                return ControlFlow::Break(self.send_command(
                    key,
                    &command,
                    "Syslog filters applied".to_string(),
                ));
            }
            other => return ControlFlow::Continue(other),
        }
        ControlFlow::Break(Task::none())
    }

    /// #132: per-device specialized detail fetch/apply (netlink / netring / sysinfo).
    ///
    /// Returns `Err(message)` for anything it does not own so [`Self::update`]
    /// can fall through to the next handler.
    fn update_detail(&mut self, message: Message) -> ControlFlow<Task<Message>, Message> {
        match message {
            Message::FetchSystemdDetail(topic) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.loading(topic);
                }
                return ControlFlow::Break(self.query_systemd_detail(topic));
            }
            Message::SystemdDetailReceived(topic, result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.apply(topic, result);
                }
            }
            Message::SystemdSetUnitFilter(filter) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.unit_state_filter = filter;
                }
            }
            Message::SystemdUnitActionArm { verb, unit } => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.pending_action = Some((verb, unit));
                }
            }
            Message::SystemdUnitActionCancel => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.pending_action = None;
                }
            }
            Message::SystemdUnitActionConfirm => {
                if let Some((verb, unit)) = self
                    .selected_device
                    .as_mut()
                    .and_then(|d| d.systemd_detail.pending_action.take())
                {
                    let key = zensight_common::fleet_command_key("systemd", "action");
                    let command = serde_json::json!({ "verb": verb, "unit": unit });
                    return ControlFlow::Break(
                        self.send_command(key, &command, format!("Sent {verb} {unit}"))
                            .chain(self.query_systemd_action_status()),
                    );
                }
            }

            // ── Cross-view identity pivots (#313) ────────────────────────────
            Message::SystemdSelectUnit(unit) => {
                use crate::view::specialized::fetch::Fetch;
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.selected_unit = unit.clone();
                    device.systemd_detail.unit_detail = match &unit {
                        Some(_) => Fetch::Loading,
                        None => Fetch::Idle,
                    };
                }
                if let Some(unit) = unit {
                    return ControlFlow::Break(self.query_systemd_unit_detail(unit));
                }
            }
            Message::SystemdUnitDetailReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.systemd_detail.unit_detail =
                        crate::view::specialized::fetch::Fetch::from_result(result);
                }
            }
            Message::PivotToUnit { host, unit } => {
                return ControlFlow::Break(self.pivot_to_unit(host, unit));
            }
            Message::PivotToProcess {
                host,
                pid,
                start_time,
            } => {
                return ControlFlow::Break(self.pivot_to_process(host, pid, start_time));
            }
            Message::ClearSysinfoPidFilter => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.sysinfo_detail.pid_filter = None;
                }
            }
            Message::OpenLogsForInvocation {
                unit,
                invocation_id,
            } => {
                // Pre-filter the Logs view to exactly this unit run, then open it
                // through the normal path (cold-store search-back included).
                self.syslog_filter.invocation_id = Some(invocation_id);
                self.syslog_filter.selected_units.clear();
                self.syslog_filter.selected_units.insert(unit);
                return ControlFlow::Break(Task::done(Message::OpenLogs));
            }
            Message::ClearLogsInvocationFilter => {
                self.syslog_filter.invocation_id = None;
            }

            Message::FetchNetlinkDetail(topic) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.loading(topic);
                }
                return ControlFlow::Break(self.query_netlink_detail(topic));
            }
            Message::NetlinkDetailReceived(topic, result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.apply(topic, result);
                }
            }

            Message::SetNetlinkSocketStateFilter(state_filter) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.socket_state_filter = state_filter;
                    // Changing the filter resets pagination so matches aren't hidden.
                    device.netlink_detail.sockets_table.limit =
                        crate::view::components::data_table::DEFAULT_LIMIT;
                }
            }
            Message::SetNetlinkSocketPortFilter(port) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.socket_port_filter = port;
                    device.netlink_detail.sockets_table.limit =
                        crate::view::components::data_table::DEFAULT_LIMIT;
                }
            }
            Message::SetNetlinkSocketSort(sort) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.socket_sort = sort;
                }
            }
            Message::NetlinkSocketsMore => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.sockets_table.load_more();
                }
            }
            Message::NetlinkTableSort(which, col) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.table_mut(which).toggle_sort(col);
                }
            }
            Message::NetlinkTableFilter(which, filter) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.table_mut(which).set_filter(filter);
                }
            }
            Message::NetlinkTableMore(which) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netlink_detail.table_mut(which).load_more();
                }
            }

            Message::SelectSpecializedTab(device_id, tab) => {
                if let Some(device) = self.selected_device.as_mut()
                    && device.device_id == device_id
                {
                    device.specialized_tab = tab;
                }
                // Prefetch the newly-activated tab's on-demand channel(s) so it
                // isn't empty until a manual fetch.
                let prefetch = match device_id.protocol {
                    zensight_common::Protocol::Netring => self.prefetch_netring_tab(tab),
                    zensight_common::Protocol::Netlink => self.prefetch_netlink_tab(tab),
                    zensight_common::Protocol::Systemd => self.prefetch_systemd_tab(tab),
                    _ => None,
                };
                if let Some(task) = prefetch {
                    return ControlFlow::Break(task);
                }
            }
            Message::NetringTableSort(which, col) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.table_mut(which).toggle_sort(col);
                }
            }
            Message::NetringTableFilter(which, filter) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.table_mut(which).set_filter(filter);
                }
            }
            Message::NetringTableMore(which) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.table_mut(which).load_more();
                }
            }
            Message::NetringPivotToFlows(device_id, endpoint) => {
                use crate::view::specialized::SpecializedTab;
                use crate::view::specialized::fetch::Fetch;
                use crate::view::specialized::netring_detail::NetringTable;
                let mut fetch_needed = false;
                if let Some(device) = self.selected_device.as_mut()
                    && device.device_id == device_id
                {
                    device.specialized_tab = SpecializedTab::Flows;
                    device
                        .netring_detail
                        .table_mut(NetringTable::Flows)
                        .set_filter(endpoint);
                    if matches!(device.netring_detail.flows, Fetch::Idle) {
                        device.netring_detail.loading();
                        fetch_needed = true;
                    }
                }
                if fetch_needed {
                    return ControlFlow::Break(self.query_netring_flows());
                }
            }
            Message::NetringAssetToTopology { ip, hostname } => {
                // Asset → topology node (#252). Seed the graph the same way
                // OpenTopology does so the lookup sees current nodes.
                self.refresh_topology_nodes();
                self.topology.apply_alerts(&self.alerts.external);
                let node = hostname
                    .filter(|h| self.topology.nodes.contains_key(h))
                    .or_else(|| self.topology_ip_to_node().get(&ip).cloned());
                if let Some(node_id) = node {
                    self.topology.select_node(node_id);
                    self.set_view(CurrentView::Topology);
                    self.save_current_view();
                    return ControlFlow::Break(self.query_topology_batch());
                }
                self.toasts.push(
                    ToastSeverity::Info,
                    format!("No topology node found for asset {ip}"),
                );
            }
            Message::FetchNetringFlows => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading();
                }
                return ControlFlow::Break(self.query_netring_flows());
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
                return ControlFlow::Break(self.query_netring_tls());
            }
            Message::NetringTlsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_tls(result);
                }
            }
            Message::FetchNetringQuic => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_quic();
                }
                return ControlFlow::Break(self.query_netring_quic());
            }
            Message::NetringQuicReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_quic(result);
                }
            }
            Message::FetchNetringSsh => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_ssh();
                }
                return ControlFlow::Break(self.query_netring_ssh());
            }
            Message::NetringSshReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_ssh(result);
                }
            }
            Message::FetchNetringJa4h => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_ja4h();
                }
                return ControlFlow::Break(self.query_netring_ja4h());
            }
            Message::NetringJa4hReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_ja4h(result);
                }
            }
            Message::FetchNetringAssets => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_assets();
                }
                return ControlFlow::Break(self.query_netring_assets());
            }
            Message::NetringAssetsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_assets(result);
                }
            }
            Message::FetchNetringTalkers => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_talkers();
                }
                return ControlFlow::Break(self.query_netring_talkers());
            }
            Message::NetringTalkersReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_talkers(result);
                }
            }
            Message::FetchNetringMatrix => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_matrix();
                }
                return ControlFlow::Break(self.query_netring_matrix());
            }
            Message::NetringMatrixReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_matrix(result);
                }
            }
            Message::FetchNetringElephants => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_elephants();
                }
                return ControlFlow::Break(self.query_netring_elephants());
            }
            Message::NetringElephantsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_elephants(result);
                }
            }
            Message::FetchNetringDns => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_dns();
                }
                return ControlFlow::Break(self.query_netring_dns());
            }
            Message::NetringDnsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_dns(result);
                }
            }
            Message::FetchNetringEncryptedDns => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_encrypted_dns();
                }
                return ControlFlow::Break(self.query_netring_encrypted_dns());
            }
            Message::NetringEncryptedDnsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_encrypted_dns(result);
                }
            }
            Message::FetchNetringHttp => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_http();
                }
                return ControlFlow::Break(self.query_netring_http());
            }
            Message::FetchNetringCaptures => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_captures();
                }
                return ControlFlow::Break(self.query_netring_captures());
            }
            Message::NetringCapturesReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_captures(result);
                }
            }
            Message::NetringCaptureNow => {
                let key = zensight_common::fleet_command_key("netring", "capture_disk");
                let command = serde_json::json!({ "type": "capture_now" });
                return ControlFlow::Break(self.send_command(
                    key,
                    &command,
                    "Capture triggered".to_string(),
                ));
            }
            Message::NetringSetCaptureDiskMode(mode) => {
                let key = zensight_common::fleet_command_key("netring", "capture_disk");
                let command = serde_json::json!({ "type": "set_capture", "mode": mode });
                return ControlFlow::Break(self.send_command(
                    key,
                    &command,
                    format!("Capture-to-disk mode → {mode}"),
                ));
            }
            Message::NetringHttpReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.apply_http(result);
                }
            }
            Message::FetchSysinfoProcesses(sort) => {
                let host = self.selected_device.as_mut().map(|device| {
                    device.sysinfo_detail.loading(sort);
                    device.device_id.source.clone()
                });
                if let Some(host) = host {
                    return ControlFlow::Break(self.query_sysinfo_processes(host, sort));
                }
            }
            Message::SysinfoProcessesReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.sysinfo_detail.apply(result);
                }
            }
            Message::FetchNetflowFlows => {
                let host = self.selected_device.as_mut().map(|device| {
                    device.netflow_detail.loading();
                    device.device_id.source.clone()
                });
                if let Some(host) = host {
                    return ControlFlow::Break(self.query_netflow_flows(host));
                }
            }
            Message::NetflowFlowsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netflow_detail.apply(result);
                }
            }
            Message::NetflowTableSort(col) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netflow_detail.table.toggle_sort(col);
                }
            }
            Message::NetflowTableFilter(q) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netflow_detail.table.set_filter(q);
                }
            }
            Message::NetflowTableMore => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netflow_detail.table.load_more();
                }
            }
            Message::FetchSysinfoLatency => {
                let host = self.selected_device.as_mut().map(|device| {
                    device.sysinfo_detail.loading_latency();
                    device.device_id.source.clone()
                });
                if let Some(host) = host {
                    return ControlFlow::Break(self.query_sysinfo_latency(host));
                }
            }
            Message::SysinfoLatencyReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.sysinfo_detail.apply_latency(result);
                }
            }
            Message::FetchParallaxStreams => {
                let host = self.selected_device.as_mut().and_then(|device| {
                    (device.device_id.protocol == zensight_common::Protocol::Parallax).then(|| {
                        device.parallax_detail.loading();
                        device.device_id.source.clone()
                    })
                });
                if let Some(host) = host {
                    return ControlFlow::Break(self.query_parallax_streams(host));
                }
            }
            Message::ParallaxStreamsReceived(result) => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.parallax_detail.apply(result);
                }
            }
            Message::ParallaxOpenTile { stream } => {
                return ControlFlow::Break(self.open_parallax_tile(stream));
            }
            Message::ParallaxCloseTile { stream } => {
                return ControlFlow::Break(self.close_parallax_tile(stream));
            }
            Message::ParallaxFrame {
                stream,
                generation,
                seq,
                handle,
            } => {
                if let Some(device) = self.selected_device.as_mut() {
                    device
                        .parallax_detail
                        .apply_frame(&stream, generation, seq, handle);
                }
            }
            Message::ParallaxTileEnded {
                stream,
                generation,
                error,
            } => {
                if let Some(device) = self.selected_device.as_mut() {
                    device.parallax_detail.end_tile(&stream, generation, error);
                }
            }
            Message::ParallaxStreamStatus { source, status } => {
                // A definitive `open: false` transition for a tile still
                // waiting on its first frame = the open failed on the sensor;
                // surface it instead of "waiting for frames…" forever.
                if let Some(device) = self.selected_device.as_mut()
                    && device.device_id.protocol == zensight_common::Protocol::Parallax
                    && device.device_id.source == source
                {
                    device.parallax_detail.apply_stream_status(&status);
                }
            }
            Message::ParallaxOpenVideoTile { stream } => {
                return ControlFlow::Break(self.open_parallax_video_tile(stream));
            }
            Message::ParallaxRequestKeyframe { stream } => {
                return ControlFlow::Break(self.request_parallax_keyframe(stream));
            }
            Message::ParallaxExpandTile { stream } => {
                return ControlFlow::Break(self.expand_parallax_tile(stream));
            }
            Message::ParallaxCollapseTile => {
                return ControlFlow::Break(self.collapse_parallax_tile());
            }
            other => return ControlFlow::Continue(other),
        }
        ControlFlow::Break(Task::none())
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        // Live parallax tiles only render on the device-detail view: leaving
        // it (Sensors/Logs/Settings/…, from ANY message) is the single choke
        // point that stops their subscribers, so the sensor stops encoding
        // for nobody. Paths that also clear `selected_device` tear down
        // before clearing (the tile state lives on it) — for those this
        // wrapper is a no-op.
        let was_device_view = self.current_view == CurrentView::Device;
        let task = self.update_inner(message);
        if was_device_view && self.current_view != CurrentView::Device {
            let teardown = self.teardown_parallax_tiles();
            return Task::batch([task, teardown]);
        }
        task
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        // #132: per-domain handlers — each consumes the message and returns a
        // Task, or hands the message back (Err) for the next handler / the match.
        let message = match self.update_chart(message) {
            ControlFlow::Break(t) => return t,
            ControlFlow::Continue(m) => m,
        };
        let message = match self.update_groups(message) {
            ControlFlow::Break(t) => return t,
            ControlFlow::Continue(m) => m,
        };
        let message = match self.update_topology_msg(message) {
            ControlFlow::Break(t) => return t,
            ControlFlow::Continue(m) => m,
        };
        let message = match self.update_syslog(message) {
            ControlFlow::Break(t) => return t,
            ControlFlow::Continue(m) => m,
        };
        let message = match self.update_detail(message) {
            ControlFlow::Break(t) => return t,
            ControlFlow::Continue(m) => m,
        };
        match message {
            Message::TelemetryReceived(point) => {
                self.handle_telemetry(point);
            }

            Message::TelemetryBatch(points) => {
                for point in points {
                    self.handle_telemetry(point);
                }
            }

            Message::HealthSnapshotReceived(snapshot) => {
                // One entry per sensor INSTANCE (`sensor@source`), not per
                // protocol — N hosts running the same sensor each keep a card.
                let key = sensor_instance_key(&snapshot.sensor, snapshot.source.as_deref());
                if let (Some(hid), Some(src)) = (&snapshot.host_id, &snapshot.source) {
                    self.origins.insert(src.clone(), hid.clone());
                    // The Focus control (#476) is disabled until the origin is
                    // known; this is where it usually becomes known.
                    self.refresh_focus_state();
                }
                self.sensor_health.insert(key, snapshot);
            }

            Message::DeviceLivenessReceived(protocol, liveness) => {
                self.handle_device_liveness(&protocol, liveness);
            }

            Message::ErrorReportReceived(sensor, source, report) => {
                tracing::warn!(
                    sensor = %sensor,
                    source = ?source,
                    device = ?report.device,
                    error_type = ?report.error_type,
                    message = %report.message,
                    "Sensor error report received"
                );
                // Keep a bounded ring of recent errors per sensor instance for
                // the Sensors view (newest at the back).
                let key = sensor_instance_key(&sensor, source.as_deref());
                let ring = self.recent_errors.entry(key).or_default();
                ring.push_back(report);
                while ring.len() > 20 {
                    ring.pop_front();
                }
            }

            Message::SensorInfoReceived(info) => {
                if let Some(hid) = &info.host_id {
                    self.origins.insert(info.source.clone(), hid.clone());
                    self.refresh_focus_state();
                }
                self.known_sensors
                    .insert(format!("{}@{}", info.name, info.source), info);
            }

            Message::AlertReceived(alert) => {
                use crate::view::alerts::ExternalAlertOutcome;
                let summary = alert.summary.clone();
                let severity = alert.severity;
                match self.alerts.ingest_external(alert) {
                    ExternalAlertOutcome::New => {
                        self.toasts
                            .push(alert_toast_severity(severity), summary.clone());
                        // Opt-in desktop notification, CRITICAL firing only (#26).
                        if self.settings.desktop_notifications
                            && severity == zensight_common::AlertSeverity::Critical
                        {
                            notify_critical(summary);
                        }
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
                self.refresh_netring_anomalies();
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
                self.refresh_netring_anomalies();
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
                self.refresh_netring_anomalies();
            }

            Message::EntitySeed(entities) => {
                self.entities.seed(entities);
                self.rederive_entities();
            }

            Message::EntityReceived(entity) => {
                if let Some(hid) = &entity.host_id {
                    for member in &entity.members {
                        self.origins.insert(member.source.clone(), hid.clone());
                    }
                    self.refresh_focus_state();
                }
                self.entities.upsert(entity);
                self.rederive_entities();
            }

            Message::EntityRemoved(entity_id) => {
                self.entities.remove(&entity_id);
                self.rederive_entities();
            }

            Message::LookupNamesForIp(ip) => {
                use crate::view::specialized::fetch::Fetch;
                self.global_search.names_lookup = Some((ip.clone(), Fetch::Loading));
                let Some(session) = self.session.clone() else {
                    self.global_search.names_lookup =
                        Some((ip, Fetch::Error("Not connected to Zenoh".into())));
                    return Task::none();
                };
                return Task::future(async move {
                    let key = format!("{}?ip={ip}", zensight_common::names_query_key());
                    let result =
                        crate::view::specialized::netlink_detail::fetch_records(session, key)
                            .await
                            .ok_or_else(|| "no correlator responded".to_string());
                    Message::NamesLookupReceived(ip, result)
                });
            }
            Message::NamesLookupReceived(ip, result) => {
                use crate::view::specialized::fetch::Fetch;
                // Ignore a stale reply if another IP was asked about since.
                if self
                    .global_search
                    .names_lookup
                    .as_ref()
                    .is_some_and(|(cur, _)| *cur == ip)
                {
                    self.global_search.names_lookup = Some((ip, Fetch::from_result(result)));
                }
            }

            Message::Connecting => {
                tracing::info!("Connecting to Zenoh...");
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Connecting;
            }

            Message::Connected(session) => {
                tracing::info!("Connected to Zenoh");
                // Preview tiles ride the old session's subscribers — abort
                // them; the sensor's matching listener reaps their streams.
                let _ = self.teardown_parallax_tiles();
                self.command_registry = session.as_ref().map(|s| {
                    std::sync::Arc::new(zensight_common::PublisherRegistry::new(s.clone()))
                });
                self.session = session;
                self.dashboard.connected = true;
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Connected;
                self.dashboard.last_error = None;

                // Constrained profile (#364): there is no AdvancedSubscriber
                // history burst on (re)connect, so seed the logs buffer from
                // the local redb store instead (same task as OpenLogs; the
                // (ts, message) de-dup in merge_log_history makes double
                // seeding harmless). Metric charts already read MetricStore.
                if self.link.profile == zensight_common::LinkProfile::Constrained
                    && let Some(store) = self.store.persistent()
                {
                    let now_ms = zensight_common::current_timestamp_millis();
                    let from = now_ms - 24 * 3_600_000;
                    return Task::future(async move {
                        let logs = tokio::task::spawn_blocking(move || {
                            store
                                .query_logs(from, now_ms, MAX_RECENT_LOGS)
                                .unwrap_or_default()
                        })
                        .await
                        .unwrap_or_default();
                        Message::LogHistoryLoaded(logs)
                    });
                }
            }

            Message::Disconnected(error) => {
                tracing::warn!(error = %error, "Disconnected from Zenoh");
                let _ = self.teardown_parallax_tiles();
                self.session = None;
                self.command_registry = None;
                self.dashboard.connected = false;
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Disconnected;
                self.dashboard.last_error = Some(error);
                // The feed is paused now; drop the freshness anchor so the
                // indicator reads "Paused", not a stale "as of" from before.
                self.last_telemetry_ms = None;
            }

            Message::SensorOnline(protocol, source) => {
                tracing::info!(protocol = %protocol, source = ?source, "Sensor online (liveliness)");
                // A fresh HealthSnapshot follows shortly; meanwhile lift any
                // Offline badge left from a previous run so the card doesn't
                // read dead while the sensor is already back.
                self.set_sensor_liveliness(&protocol, source.as_deref(), true);
            }

            Message::SensorOffline(protocol, source) => {
                tracing::warn!(protocol = %protocol, source = ?source, "Sensor offline (liveliness)");
                // A dead sensor publishes no further HealthSnapshots, so its
                // last snapshot would sit at "Healthy" forever — flip it here.
                // Its devices carry their own liveliness tokens and get their
                // DeviceOffline events independently.
                self.set_sensor_liveliness(&protocol, source.as_deref(), false);
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
                if let (Some(metric), Some(d)) = (metric, self.selected_device.as_mut()) {
                    d.select_metric(metric);
                }
                return task;
            }

            Message::SelectAdjacentDevice { forward } => {
                // #35: cycle through the dashboard's current filtered set without
                // bouncing back to the dashboard each time.
                if let Some(current) = self.selected_device.as_ref().map(|d| d.device_id.clone()) {
                    let ids = self.dashboard.ordered_device_ids();
                    // position() returning Some guarantees ids is non-empty.
                    if let Some(pos) = ids.iter().position(|id| *id == current) {
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

            Message::ClearSelection => {
                let teardown = self.teardown_parallax_tiles();
                self.selected_device = None;
                self.set_view(CurrentView::Dashboard);
                return teardown;
            }

            Message::SetFocusHost(origin) => {
                // Re-keying `self.link` is the whole mechanism: Iced hashes it,
                // so the subscription tears the Zenoh session down and
                // re-declares against the narrowed selectors (#476). The fleet's
                // devices will age out of the dashboard while focused — that is
                // the point, and the shell shows a banner saying so.
                if self.link.focus == origin {
                    return Task::none();
                }
                match &origin {
                    Some(o) => {
                        let host = self
                            .origins
                            .iter()
                            .find(|(_, v)| *v == o)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| o.clone());
                        self.toasts.push(
                            ToastSeverity::Info,
                            format!(
                                "Focused on {host} — subscribing to that host only; \
                                 fleet data is paused"
                            ),
                        );
                    }
                    None => self.toasts.push(
                        ToastSeverity::Info,
                        "Focus cleared — back to the fleet".to_string(),
                    ),
                }
                self.link.focus = origin;
                self.refresh_focus_state();
                // Reconnecting: the restarted subscription drives
                // Connecting → Connected.
                self.dashboard.connection_state =
                    crate::view::dashboard::ConnectionState::Connecting;
                self.dashboard.connected = false;
            }

            Message::ForgetDevice(id) => {
                // Facets are in-memory only — removal is a pure view-model
                // operation; the device reappears if telemetry resumes.
                if self.dashboard.devices.remove(&id).is_some() {
                    self.toasts.push(
                        ToastSeverity::Info,
                        format!("Forgot {} · {}", id.protocol.display_name(), id.source),
                    );
                }
                if self
                    .selected_device
                    .as_ref()
                    .is_some_and(|d| d.device_id == id)
                {
                    // Reuse the back-to-dashboard choke point (parallax tile
                    // teardown etc.) instead of duplicating its logic.
                    return self.update(Message::ClearSelection);
                }
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

            Message::ToggleGroupByHost => {
                self.settings.group_by_host = !self.settings.group_by_host;
                self.save_group_by_host();
            }

            Message::ToggleIdentityDetails => {
                self.identity_expanded = !self.identity_expanded;
                self.save_identity_expanded();
            }

            Message::Tick => {
                self.handle_tick();
                // Log-events refresh (#358): while a logs surface is open, pull
                // fresh per-line events on a slow cadence (piggybacked on the
                // 1 Hz tick — no dedicated timer).
                let log_fetch = self.maybe_refresh_logs();
                // Topology refresh (#391): while the map is open, re-pull
                // matrix/flows/neighbors every ~10 s so edge rates stay live.
                let topo_fetch = self.maybe_refresh_topology();
                let log_fetch = match (log_fetch, topo_fetch) {
                    (Some(a), Some(b)) => Some(Task::batch([a, b])),
                    (Some(a), None) | (None, Some(a)) => Some(a),
                    (None, None) => None,
                };
                // Periodically flush downsampled buckets to redb off the UI thread
                // (every ~15 ticks ≈ 15s). Never block update()/view() on disk I/O.
                self.ticks_since_flush += 1;
                if self.ticks_since_flush >= STORE_FLUSH_EVERY_TICKS {
                    self.ticks_since_flush = 0;
                    let metric_batch = self.store.take_flush_batch();
                    let log_batch = self.store.take_log_flush_batch();
                    // Only schedule the off-thread write if there's something to do.
                    if metric_batch.is_some() || log_batch.is_some() {
                        // Prune aged-out buckets/log rows every Nth flush (#131,
                        // #107) so the redb file doesn't grow unbounded — bundled
                        // into the same off-thread task as the write.
                        self.flushes_since_prune += 1;
                        let prune = self.flushes_since_prune >= STORE_PRUNE_EVERY_FLUSHES;
                        if prune {
                            self.flushes_since_prune = 0;
                        }
                        // Either batch carries a clone of the same redb handle.
                        let store = metric_batch
                            .as_ref()
                            .map(|(s, _)| s.clone())
                            .or_else(|| log_batch.as_ref().map(|(s, _)| s.clone()))
                            .expect("at least one batch is Some");
                        let batch = metric_batch.map(|(_, b)| b).unwrap_or_default();
                        let logs = log_batch.map(|(_, l)| l).unwrap_or_default();
                        let now_ms = zensight_common::current_timestamp_millis();
                        let flush = Task::future(async move {
                            // Map redb's large error to a String inside the blocking
                            // closure so the future's payload stays small.
                            let res = tokio::task::spawn_blocking(move || {
                                let n = store.write_batch(&batch).map_err(|e| e.to_string())?;
                                store.write_logs(&logs).map_err(|e| e.to_string())?;
                                if prune {
                                    let evicted = store.prune(now_ms).map_err(|e| e.to_string())?;
                                    let log_evicted = store
                                        .prune_logs(crate::store::LOG_STORE_MAX_ROWS)
                                        .map_err(|e| e.to_string())?;
                                    if evicted > 0 || log_evicted > 0 {
                                        tracing::debug!(
                                            evicted,
                                            log_evicted,
                                            "Pruned aged-out store rows"
                                        );
                                    }
                                }
                                Ok::<usize, String>(n)
                            })
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r);
                            Message::StoreFlushed(res)
                        });
                        return match log_fetch {
                            Some(fetch) => Task::batch([flush, fetch]),
                            None => flush,
                        };
                    }
                }
                if let Some(fetch) = log_fetch {
                    return fetch;
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
                let teardown = self.teardown_parallax_tiles();
                self.selected_device = None;
                self.set_view(CurrentView::Dashboard);
                return teardown;
            }

            Message::OpenSensors => {
                self.set_view(CurrentView::Sensors);
                // Discover each sensor's advertised artifact kinds (+ adverts).
                if let Some(task) = self.load_artifact_kinds() {
                    return task;
                }
            }

            Message::OpenLogs => {
                self.set_view(CurrentView::Logs);
                let mut tasks: Vec<Task<Message>> = Vec::new();
                // Search-back (#107, C9): pull persisted logs from the cold store
                // off-thread so the Logs view opens with history that survived a
                // restart, not just what's arrived this session.
                if let Some(store) = self.store.persistent() {
                    let now_ms = zensight_common::current_timestamp_millis();
                    let from = now_ms - 24 * 3_600_000; // last 24h
                    tasks.push(Task::future(async move {
                        let logs = tokio::task::spawn_blocking(move || {
                            store
                                .query_logs(from, now_ms, MAX_RECENT_LOGS)
                                .unwrap_or_default()
                        })
                        .await
                        .unwrap_or_default();
                        Message::LogHistoryLoaded(logs)
                    }));
                }
                // On-demand seed (#358): per-line events no longer stream, so
                // pull the sensors' current rings immediately on open. The
                // periodic tick refresh keeps the view live afterwards.
                if !self.demo_mode && self.session.is_some() && !self.log_fetch_inflight {
                    self.log_fetch_inflight = true;
                    self.last_log_fetch_ms = Some(now_ms());
                    tasks.push(self.query_log_events(None));
                }
                if !tasks.is_empty() {
                    return Task::batch(tasks);
                }
            }

            Message::LogHistoryLoaded(logs) => {
                self.merge_log_history(logs);
            }

            Message::LogEventsLoaded(result) => {
                self.log_fetch_inflight = false;
                match result {
                    Ok(records) => {
                        let mut msgs = Vec::with_capacity(records.len());
                        for rec in &records {
                            self.last_log_event_ms = Some(
                                self.last_log_event_ms
                                    .map_or(rec.ts, |prev| prev.max(rec.ts)),
                            );
                            let point = rec.to_point();
                            // Persist for search-back (#107): redb keys by uid,
                            // so overlap-window re-fetches are idempotent.
                            if let Some(log) = crate::store::StoredLog::from_point(&point) {
                                self.store.record_log(log);
                            }
                            msgs.push(crate::view::specialized::syslog_message_from_point(
                                &point,
                                &point.source,
                            ));
                        }
                        self.merge_log_messages(msgs);
                    }
                    Err(e) => tracing::debug!(error = %e, "log-events fetch failed"),
                }
            }

            Message::OpenIncidents => {
                self.set_view(CurrentView::Incidents);
            }
            Message::SelectIncident(id) => {
                self.incidents.selected = id;
            }

            Message::OpenInventory => {
                self.set_view(CurrentView::Inventory);
                self.inventory.loading();
                return self.query_inventory();
            }
            Message::InventoryLoaded(result) => {
                self.inventory.apply(result);
            }
            Message::SetInventoryAssetSort(sort) => {
                self.inventory.asset_sort = sort;
            }
            Message::SetInventoryAssetRole(role) => {
                self.inventory.asset_role_filter = role;
            }
            Message::SetInventoryFpFilter(kind) => {
                self.inventory.fp_filter = kind;
            }

            Message::OpenBandwidth => {
                // Nav-rail entry = unscoped (#351); the per-host pivot below
                // sets the scope instead.
                self.bandwidth.host_filter = None;
                self.set_view(CurrentView::Bandwidth);
                let rows = self.bandwidth_service_rows();
                self.bandwidth.set_services(rows);
                // Only the Processes mode needs a fetch; Services reads the stream.
                if self.bandwidth.mode == crate::view::bandwidth::BandwidthMode::Processes {
                    self.bandwidth.loading();
                    return self.query_bandwidth();
                }
            }

            Message::OpenBandwidthForHost(host) => {
                // Contextual pivot from a device view (#351): same open flow,
                // pre-scoped to the host.
                self.bandwidth.host_filter = Some(host);
                self.set_view(CurrentView::Bandwidth);
                let rows = self.bandwidth_service_rows();
                self.bandwidth.set_services(rows);
                // Always refetch processes: the scope is applied at fold time.
                self.bandwidth.loading();
                return self.query_bandwidth();
            }

            Message::ClearBandwidthHostFilter => {
                self.bandwidth.host_filter = None;
                let rows = self.bandwidth_service_rows();
                self.bandwidth.set_services(rows);
                if self.bandwidth.mode == crate::view::bandwidth::BandwidthMode::Processes {
                    self.bandwidth.loading();
                    return self.query_bandwidth();
                }
            }
            Message::RefreshBandwidth => {
                self.bandwidth.loading();
                return self.query_bandwidth();
            }
            Message::BandwidthLoaded(result) => {
                self.bandwidth.apply(result);
            }
            Message::SetBandwidthMode(mode) => {
                self.bandwidth.mode = mode;
                self.bandwidth.table = crate::view::components::TableState::default();
                match mode {
                    crate::view::bandwidth::BandwidthMode::Services => {
                        let rows = self.bandwidth_service_rows();
                        self.bandwidth.set_services(rows);
                    }
                    // Fetch per-process rows the first time that mode is shown.
                    crate::view::bandwidth::BandwidthMode::Processes => {
                        if matches!(
                            self.bandwidth.processes,
                            crate::view::specialized::fetch::Fetch::Idle
                        ) {
                            self.bandwidth.loading();
                            return self.query_bandwidth();
                        }
                    }
                }
            }
            Message::BandwidthTableSort(col) => {
                self.bandwidth.table.toggle_sort(col);
            }
            Message::BandwidthTableFilter(q) => {
                self.bandwidth.table.set_filter(q);
            }

            Message::OpenFleet => {
                self.set_view(CurrentView::Fleet);
                self.save_current_view();
                // Ask once on open; the answer is a build property, so it does
                // not change until something on the fleet is redeployed.
                if matches!(
                    self.fleet.rows,
                    crate::view::specialized::fetch::Fetch::Idle
                ) {
                    self.fleet.loading();
                    return self.query_fleet();
                }
            }
            Message::RefreshFleet => {
                self.fleet.loading();
                return self.query_fleet();
            }
            Message::FleetLoaded(result) => {
                let alive = self.alive_producers();
                self.fleet.apply(result, &alive);
            }
            Message::ToggleFleetFindings(id) => {
                self.fleet.expanded = if self.fleet.expanded.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id)
                };
            }
            Message::FleetTableSort(col) => {
                self.fleet.table.toggle_sort(col);
            }
            Message::FleetTableFilter(q) => {
                self.fleet.table.set_filter(q);
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

            Message::SetLinkProfile(profile) => {
                self.settings.set_link_profile(profile);
            }

            Message::SubscriptionScopeChanged(scope) => {
                self.settings.set_subscription_scope(scope);
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

            Message::SetAlertSeverityFilter(sev) => {
                self.alerts.external_severity_filter = sev;
            }
            Message::SetAlertSourceFilter(source) => {
                self.alerts.external_source_filter = source;
            }
            Message::SaveAlertFilterPreset => {
                if self.alerts.save_current_filter_preset() {
                    self.save_alert_filter_presets();
                }
            }
            Message::ApplyAlertFilterPreset(index) => {
                self.alerts.apply_filter_preset(index);
            }
            Message::DeleteAlertFilterPreset(index) => {
                self.alerts.delete_filter_preset(index);
                self.save_alert_filter_presets();
            }

            Message::ToggleHelp => {
                self.help_open = !self.help_open;
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

            Message::OpenCommandPalette => {
                self.command_palette.open();
                return focus(crate::view::palette::COMMAND_PALETTE_ID.clone());
            }
            Message::CloseCommandPalette => {
                self.command_palette.close();
            }
            Message::SetCommandPaletteQuery(q) => {
                self.command_palette.query = q;
            }
            Message::RunPaletteCommand(index) => {
                let filtered = crate::view::palette::filter(&self.command_palette.query);
                if let Some(cmd) = filtered.get(index) {
                    let msg = cmd.message.clone();
                    self.command_palette.close();
                    return Task::done(msg);
                }
            }

            Message::ClearAlerts => {
                self.alerts.clear_alerts();
            }

            // Export messages
            Message::ExportToCsv => {
                if let Some(task) = self.export_to_csv() {
                    return task;
                }
            }

            Message::ExportToJson => {
                if let Some(task) = self.export_to_json() {
                    return task;
                }
            }

            Message::ExportFinished(result) => match result {
                Ok(Some(path)) => {
                    tracing::info!(path = %path, "Exported device data");
                    self.toasts
                        .push(ToastSeverity::Success, format!("Exported to {path}"));
                }
                // User cancelled the save dialog — silent, no toast.
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, "Export failed");
                    self.toasts
                        .push(ToastSeverity::Error, format!("Export failed: {e}"));
                }
            },

            // Unified artifact download (report / snapshot / capture) via the artifact channel.
            Message::LoadArtifactKinds => {
                if let Some(task) = self.load_artifact_kinds() {
                    return task;
                }
            }

            Message::ArtifactKindsLoaded { producer, kinds } => {
                // Seed a default capture form for a sensor that advertises the
                // Capture kind, so the form renders before the operator edits it.
                if kinds
                    .iter()
                    .any(|k| matches!(k.advert, zensight_common::KindAdvert::Capture { .. }))
                {
                    self.capture_forms.entry(producer.clone()).or_default();
                }
                self.artifact_kinds.insert(producer, kinds);
            }

            Message::CaptureFormEdited {
                producer,
                field,
                value,
            } => {
                use crate::view::artifact_fetch::CaptureField;
                let form = self.capture_forms.entry(producer).or_default();
                match field {
                    CaptureField::Duration => form.duration_secs = value,
                    CaptureField::Filter => form.filter = value,
                    CaptureField::MaxMib => form.max_mib = value,
                }
            }

            Message::CaptureFormToggled { producer, field } => {
                use crate::view::artifact_fetch::CaptureToggle;
                let form = self.capture_forms.entry(producer).or_default();
                match field {
                    CaptureToggle::Compress => form.compress = !form.compress,
                    CaptureToggle::DecompressOnSave => {
                        form.decompress_on_save = !form.decompress_on_save
                    }
                }
            }

            Message::StartArtifact {
                producer,
                kind,
                target_source,
            } => {
                if let Some(task) = self.start_artifact(producer, kind, target_source) {
                    return task;
                }
            }

            Message::DownloadCaptureBlob {
                producer,
                artifact_id,
                filename,
            } => {
                if let Some(task) = self.download_capture_blob(producer, artifact_id, filename) {
                    return task;
                }
            }

            Message::ArtifactDestChosen {
                producer,
                kind,
                target_source,
                dest,
            } => {
                if let Some(dest) = dest
                    && let Some(task) =
                        self.start_artifact_with_dest(producer, kind, target_source, dest)
                {
                    return task;
                }
            }

            Message::ArtifactRequested(result) => {
                if let Some(task) = self.on_artifact_requested(result) {
                    return task;
                }
            }

            Message::ArtifactGenerating { detail, progress } => {
                // Only update while the produce phase is running (ignore a stale
                // poll landing after Ready flipped the state to Downloading).
                if matches!(
                    self.artifact_fetch,
                    crate::view::artifact_fetch::ArtifactFetch::Requesting
                        | crate::view::artifact_fetch::ArtifactFetch::Generating { .. }
                ) {
                    self.artifact_fetch =
                        crate::view::artifact_fetch::ArtifactFetch::Generating { detail, progress };
                }
            }

            Message::ArtifactProgress { got, total } => {
                // Only update while actively downloading (ignore stale progress
                // from a paused/cancelled job).
                if matches!(
                    self.artifact_fetch,
                    crate::view::artifact_fetch::ArtifactFetch::Downloading { .. }
                ) {
                    self.artifact_fetch =
                        crate::view::artifact_fetch::ArtifactFetch::Downloading { got, total };
                }
            }

            Message::ArtifactDownloaded(result) => {
                if let Some(task) = self.on_artifact_downloaded(result) {
                    return task;
                }
            }

            Message::ArtifactSaved(result) => match result {
                Ok(Some(path)) => {
                    self.artifact_fetch =
                        crate::view::artifact_fetch::ArtifactFetch::Saved(path.clone());
                    self.toasts
                        .push(ToastSeverity::Success, format!("Artifact saved to {path}"));
                }
                Ok(None) => {
                    // User cancelled the save dialog — discard, back to idle.
                    self.artifact_fetch = crate::view::artifact_fetch::ArtifactFetch::Idle;
                    self.artifact_job = None;
                }
                Err(e) => {
                    self.artifact_fetch =
                        crate::view::artifact_fetch::ArtifactFetch::Failed(e.clone());
                    self.toasts
                        .push(ToastSeverity::Error, format!("Save failed: {e}"));
                }
            },

            Message::PauseArtifact => {
                if let crate::view::artifact_fetch::ArtifactFetch::Downloading { got, total } =
                    self.artifact_fetch
                {
                    // Signal the in-flight stream to stop; the partial persists.
                    if let Some(job) = &self.artifact_job {
                        job.cancel.cancel();
                    }
                    self.artifact_fetch =
                        crate::view::artifact_fetch::ArtifactFetch::Paused { got, total };
                }
            }

            Message::ResumeArtifact => {
                if let Some(task) = self.resume_artifact() {
                    return task;
                }
            }

            Message::CancelArtifact => {
                let task = self.cancel_artifact();
                self.artifact_fetch = crate::view::artifact_fetch::ArtifactFetch::Idle;
                self.artifact_job = None;
                if let Some(task) = task {
                    return task;
                }
            }

            Message::ToggleTheme => {
                self.theme = self.theme.toggle();
                // Persist the theme preference
                self.settings.dark_theme = matches!(self.theme, AppTheme::Dark);
                self.save_theme();
            }

            Message::ToggleDesktopNotifications => {
                self.settings.desktop_notifications = !self.settings.desktop_notifications;
                self.save_notification_pref();
            }

            // Keyboard shortcuts
            Message::FocusSearch => {
                return self.focus_search();
            }

            Message::EscapePressed => {
                return self.handle_escape();
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
                self.refresh_topology_nodes();
                self.topology.apply_alerts(&self.alerts.external);
                self.set_view(CurrentView::Topology);
                self.save_current_view();
                // Derive real edges from observed flows (#25) and netlink
                // neighbor adjacency (#49); edges are merged as replies arrive.
                return self.query_topology_batch();
            }

            Message::CommandFeedback { success, message } => {
                // An empty message means "quiet on success" (automatic
                // commands like resync keyframe requests); errors always
                // carry text and always surface.
                if !message.is_empty() {
                    let severity = if success {
                        ToastSeverity::Success
                    } else {
                        ToastSeverity::Error
                    };
                    self.toasts.push(severity, message);
                }
            }

            Message::OpenExpectations => {
                self.set_view(CurrentView::Expectations);
                return self.query_expectations();
            }
            Message::CloseExpectations => {
                self.set_view(CurrentView::Dashboard);
            }
            Message::SetExpTarget(target) => {
                use crate::view::expectations::ExpTarget;
                self.expectations.target = target;
                self.expectations.status_note = None;
                return match target {
                    ExpTarget::Netlink => self.query_expectations(),
                    ExpTarget::Systemd => self.query_systemd_expectations(),
                };
            }
            Message::SetSystemdExpKind(kind) => {
                self.expectations.systemd_kind = kind;
            }
            Message::SystemdExpectationsReceived(json) => {
                self.expectations.systemd =
                    crate::view::expectations::SystemdExpDraft::from_status(&json);
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
                use crate::view::expectations::{ExpKind, ExpTarget, SystemdExpKind};
                // Systemd sentinel (#278): mutate the accumulated draft, then push
                // the full set via SetExpectations.
                if self.expectations.target == ExpTarget::Systemd {
                    let name = self.expectations.new_name.trim().to_string();
                    let kind = self.expectations.systemd_kind;
                    if kind != SystemdExpKind::ForbidFailed && name.is_empty() {
                        self.toasts
                            .push(ToastSeverity::Error, "Unit/target/timer name is required");
                        return Task::none();
                    }
                    let val = self.expectations.new_value.trim().to_string();
                    let win = self.expectations.new_port.trim().to_string();
                    let draft = &mut self.expectations.systemd;
                    match kind {
                        SystemdExpKind::ServiceActive => {
                            if !draft.services.contains(&name) {
                                draft.services.push(name);
                            }
                        }
                        SystemdExpKind::TargetActive => {
                            if !draft.targets.contains(&name) {
                                draft.targets.push(name);
                            }
                        }
                        SystemdExpKind::TimerWithin => {
                            let Ok(within) = val.parse::<u64>() else {
                                self.toasts
                                    .push(ToastSeverity::Error, "within (secs) must be a number");
                                return Task::none();
                            };
                            draft.timers.retain(|(t, _)| t != &name);
                            draft.timers.push((name, within));
                        }
                        SystemdExpKind::RestartRate => {
                            let (Ok(max), Ok(window)) = (val.parse::<u32>(), win.parse::<u64>())
                            else {
                                self.toasts.push(
                                    ToastSeverity::Error,
                                    "max restarts + window (secs) must be numbers",
                                );
                                return Task::none();
                            };
                            draft.restart_rates.retain(|(u, _, _)| u != &name);
                            draft.restart_rates.push((name, max, window));
                        }
                        SystemdExpKind::ForbidFailed => draft.forbid_failed = true,
                    }
                    let command = self.expectations.systemd.to_command_json();
                    let key = zensight_common::fleet_command_key("systemd", "expectations");
                    return self
                        .send_command(key, &command, "systemd expectations pushed".to_string())
                        .chain(self.query_systemd_expectations());
                }
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
                let key = zensight_common::fleet_command_key("netlink", "expectations");
                return self
                    .send_command(key, &command, "Expectation pushed".to_string())
                    .chain(self.query_expectations());
            }
            Message::RemoveExpectation(rule) => {
                use crate::view::expectations::ExpTarget;
                if self.expectations.target == ExpTarget::Systemd {
                    self.expectations.systemd.remove_rule(&rule);
                    let command = self.expectations.systemd.to_command_json();
                    let key = zensight_common::fleet_command_key("systemd", "expectations");
                    return self
                        .send_command(key, &command, format!("Removed {rule}"))
                        .chain(self.query_systemd_expectations());
                }
                let command = serde_json::json!({ "type": "remove", "rule": rule });
                let key = zensight_common::fleet_command_key("netlink", "expectations");
                return self
                    .send_command(key, &command, format!("Removed {rule}"))
                    .chain(self.query_expectations());
            }
            Message::RefreshExpectations => {
                use crate::view::expectations::ExpTarget;
                return match self.expectations.target {
                    ExpTarget::Netlink => self.query_expectations(),
                    ExpTarget::Systemd => self.query_systemd_expectations(),
                };
            }
            Message::ExpectationStatusReceived(json) => {
                self.expectations.current = crate::view::expectations::parse_status(&json);
                self.expectations.status_note =
                    Some(format!("{} configured", self.expectations.current.len()));
            }

            // Netring detection-tuning (#121).
            Message::RefreshDetectorConfig => {
                return self
                    .query_detector_status()
                    .chain(self.query_capture_filter_status())
                    .chain(self.query_threat_intel_status());
            }
            Message::DetectorConfigReceived(result) => match result {
                Ok(json) => self.detection_tuning.apply_status(&json),
                Err(e) => {
                    self.detection_tuning.status_note = Some(e);
                }
            },
            Message::ToggleNetringDetector(detector) => {
                let enabled = !self.detection_tuning.is_enabled(&detector).unwrap_or(false);
                let command = serde_json::json!({ "type": "set_enabled", "detector": detector, "enabled": enabled });
                let key = zensight_common::fleet_command_key("netring", "detectors");
                return self
                    .send_command(
                        key,
                        &command,
                        format!("{detector} {}", if enabled { "enabled" } else { "muted" }),
                    )
                    .chain(self.query_detector_status());
            }
            Message::SetNetringThresholdInput { detector, value } => {
                if let Some(row) = self
                    .detection_tuning
                    .detectors
                    .iter_mut()
                    .find(|d| d.name == detector)
                {
                    row.threshold_input = value;
                }
            }
            Message::ApplyNetringThreshold(detector) => {
                let input = self
                    .detection_tuning
                    .detectors
                    .iter()
                    .find(|d| d.name == detector)
                    .map(|d| d.threshold_input.clone())
                    .unwrap_or_default();
                let Ok(value) = input.trim().parse::<f64>() else {
                    self.toasts
                        .push(ToastSeverity::Error, "Threshold must be a number");
                    return Task::none();
                };
                let command = serde_json::json!({ "type": "set_threshold", "detector": detector, "value": value });
                let key = zensight_common::fleet_command_key("netring", "detectors");
                return self
                    .send_command(key, &command, format!("{detector} threshold = {value}"))
                    .chain(self.query_detector_status());
            }
            Message::SetNetringAllowlistInput(value) => {
                self.detection_tuning.new_entry = value;
            }
            Message::AddNetringAllowlist => {
                let entry = self.detection_tuning.new_entry.trim().to_string();
                if entry.is_empty() {
                    return Task::none();
                }
                self.detection_tuning.new_entry.clear();
                let command = serde_json::json!({ "type": "add_allowlist", "entry": entry });
                let key = zensight_common::fleet_command_key("netring", "detectors");
                return self
                    .send_command(key, &command, format!("Allowlisted {entry}"))
                    .chain(self.query_detector_status());
            }
            Message::AddNetringAllowlistEntry(entry) => {
                let entry = entry.trim().to_string();
                if entry.is_empty() {
                    return Task::none();
                }
                let command = serde_json::json!({ "type": "add_allowlist", "entry": entry });
                let key = zensight_common::fleet_command_key("netring", "detectors");
                return self
                    .send_command(key, &command, format!("Allowlisted {entry}"))
                    .chain(self.query_detector_status());
            }
            Message::RemoveNetringAllowlist(entry) => {
                let command = serde_json::json!({ "type": "remove_allowlist", "entry": entry });
                let key = zensight_common::fleet_command_key("netring", "detectors");
                return self
                    .send_command(key, &command, format!("Removed {entry}"))
                    .chain(self.query_detector_status());
            }

            // Netring capture-focus (#225/#228): hot-swap the reloadable packet
            // filter. Validation happens sensor-side — a bad expr comes back as a
            // `last_error` on `@rpc/netring/capture_filter`, surfaced inline.
            Message::SetPacketFilterInput(value) => {
                self.detection_tuning.packet_filter_input = value;
            }
            Message::ApplyPacketFilter => {
                let expr = self.detection_tuning.packet_filter_input.trim().to_string();
                if expr.is_empty() {
                    self.toasts
                        .push(ToastSeverity::Error, "Capture filter cannot be empty");
                    return Task::none();
                }
                let command = serde_json::json!({ "type": "set_packet_filter", "expr": expr });
                let key = zensight_common::fleet_command_key("netring", "capture_filter");
                return self
                    .send_command(key, &command, format!("Capture filter → {expr}"))
                    .chain(self.query_capture_filter_status());
            }
            Message::ClearPacketFilter => {
                let command = serde_json::json!({ "type": "clear_packet_filter" });
                let key = zensight_common::fleet_command_key("netring", "capture_filter");
                return self
                    .send_command(key, &command, "Capture filter cleared".to_string())
                    .chain(self.query_capture_filter_status());
            }
            Message::CaptureFilterStatusReceived(result) => match result {
                Ok(json) => {
                    self.detection_tuning.apply_capture_filter_status(&json);
                    // Surface a sensor-side validation rejection as a toast too,
                    // so it's not missed if the panel isn't on screen.
                    if let Some(err) = self
                        .detection_tuning
                        .capture_filter
                        .as_ref()
                        .and_then(|c| c.last_error.clone())
                    {
                        self.toasts
                            .push(ToastSeverity::Error, format!("Filter rejected: {err}"));
                    }
                }
                Err(_) => {
                    self.detection_tuning.capture_filter = None;
                }
            },

            // Netring threat-intel (#328): hot-swap IOC / YARA matchers. Sensor
            // validates YARA; a compile error comes back as `last_reload` on
            // `@rpc/netring/threat_intel`, surfaced inline + as a toast.
            Message::SetThreatIocInput(value) => {
                self.detection_tuning.threat_ioc_input = value;
            }
            Message::ApplyThreatIoc => {
                let (ips, domains) = crate::view::detection_tuning::split_ioc_paste(
                    &self.detection_tuning.threat_ioc_input,
                );
                if ips.is_empty() && domains.is_empty() {
                    self.toasts
                        .push(ToastSeverity::Error, "No indicators to apply");
                    return Task::none();
                }
                let n = ips.len() + domains.len();
                let command = serde_json::json!({
                    "type": "set_ioc", "ips": ips, "domains": domains, "ja4": [], "ja3": [],
                });
                let key = zensight_common::fleet_command_key("netring", "threat_intel");
                return self
                    .send_command(key, &command, format!("Pushed {n} IOC indicators"))
                    .chain(self.query_threat_intel_status());
            }
            Message::ReloadThreatIocFiles => {
                let command = serde_json::json!({ "type": "reload_ioc_files" });
                let key = zensight_common::fleet_command_key("netring", "threat_intel");
                return self
                    .send_command(key, &command, "Reloading indicator files".to_string())
                    .chain(self.query_threat_intel_status());
            }
            Message::ClearThreatIoc => {
                let command = serde_json::json!({ "type": "clear_ioc" });
                let key = zensight_common::fleet_command_key("netring", "threat_intel");
                return self
                    .send_command(key, &command, "Cleared IOC indicators".to_string())
                    .chain(self.query_threat_intel_status());
            }
            Message::SetThreatYaraInput(value) => {
                self.detection_tuning.threat_yara_input = value;
            }
            Message::ApplyThreatYara => {
                let rules = self.detection_tuning.threat_yara_input.trim().to_string();
                if rules.is_empty() {
                    self.toasts
                        .push(ToastSeverity::Error, "YARA rules cannot be empty");
                    return Task::none();
                }
                let command = serde_json::json!({ "type": "set_yara", "rules": rules });
                let key = zensight_common::fleet_command_key("netring", "threat_intel");
                return self
                    .send_command(key, &command, "Applying YARA rules".to_string())
                    .chain(self.query_threat_intel_status());
            }
            Message::ThreatIntelStatusReceived(result) => match result {
                Ok(json) => {
                    self.detection_tuning.apply_threat_intel_status(&json);
                    // Surface a sensor-side reload error (e.g. bad YARA) as a toast.
                    if let Some(last) = self
                        .detection_tuning
                        .threat_intel
                        .as_ref()
                        .and_then(|t| t.last_reload.clone())
                        && last.starts_with("error")
                    {
                        self.toasts.push(ToastSeverity::Error, last);
                    }
                }
                Err(_) => {
                    self.detection_tuning.threat_intel = None;
                }
            },

            Message::FetchAnomalyFlows { key, src } => {
                self.security.flows_for = Some(key.clone());
                self.security.flows = crate::view::specialized::fetch::Fetch::Loading;
                return self.query_anomaly_flows(key, src);
            }
            Message::AnomalyFlowsReceived(key, result) => {
                // Ignore a stale reply if the user has since pivoted elsewhere.
                if self.security.flows_for.as_deref() == Some(key.as_str()) {
                    self.security.flows =
                        crate::view::specialized::fetch::Fetch::from_result(result);
                }
            }
            Message::FetchFlowAttribution {
                target,
                key,
                src,
                dst,
            } => {
                use crate::view::specialized::fetch::Fetch;
                let slot = Some((key.clone(), Fetch::Loading));
                match target {
                    crate::message::AttributionTarget::Security => {
                        self.security.attribution = slot;
                    }
                    crate::message::AttributionTarget::Device => {
                        if let Some(device) = self.selected_device.as_mut() {
                            device.netring_detail.attribution = slot;
                        }
                    }
                    crate::message::AttributionTarget::Topology => {
                        self.topology.panel.attribution = slot;
                    }
                }
                return self.query_flow_attribution(target, key, src, dst);
            }
            Message::FlowAttributionReceived {
                target,
                key,
                result,
            } => {
                use crate::view::specialized::fetch::Fetch;
                let slot = match target {
                    crate::message::AttributionTarget::Security => {
                        Some(&mut self.security.attribution)
                    }
                    crate::message::AttributionTarget::Device => self
                        .selected_device
                        .as_mut()
                        .map(|d| &mut d.netring_detail.attribution),
                    crate::message::AttributionTarget::Topology => {
                        Some(&mut self.topology.panel.attribution)
                    }
                };
                // Ignore a stale reply if another row was asked about since.
                if let Some(slot) = slot
                    && slot.as_ref().is_some_and(|(k, _)| *k == key)
                {
                    *slot = Some((key, Fetch::from_result(result)));
                }
            }
            Message::OpenSecurity => {
                self.set_view(CurrentView::Security);
                // Pull the netring detector config so the tuning panel is ready.
                return self.query_detector_status();
            }
            Message::CloseSecurity => {
                self.set_view(CurrentView::Dashboard);
            }
            Message::ToggleSecurityHideInfo => {
                self.security.hide_info = !self.security.hide_info;
            }
            Message::SelectAnomaly(key) => {
                let expanded = key.is_some();
                self.security.selected = key;
                // Pull the capture index once (#327) so an expanded anomaly can
                // offer its matching triggered capture for download.
                if expanded
                    && matches!(
                        self.security.captures,
                        crate::view::specialized::fetch::Fetch::Idle
                    )
                {
                    self.security.captures = crate::view::specialized::fetch::Fetch::Loading;
                    return self.query_anomaly_captures();
                }
            }
            Message::AnomalyCapturesReceived(result) => {
                // A missing index is the normal case (capture.to_disk off) — keep
                // the drill-down quiet rather than surfacing an error line.
                self.security.captures = match result {
                    Ok(records) => crate::view::specialized::fetch::Fetch::Ready(records),
                    Err(_) => crate::view::specialized::fetch::Fetch::Ready(Vec::new()),
                };
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

            // #132: every variant claimed by an `update_*` handler returned above,
            // so nothing else reaches here. Flag a stray (e.g. a new variant whose
            // handler wiring was forgotten) loudly in debug rather than silently.
            other => {
                debug_assert!(false, "update(): unrouted message {other:?}");
                tracing::warn!(message = ?other, "unrouted message in update()");
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

    /// Persist the saved alert-filter presets (#27).
    fn save_alert_filter_presets(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.alert_filter_presets = self.alerts.alert_filter_presets.clone();
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save alert filter presets: {}", e);
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

    /// Persist the "group by host" dashboard preference (#306).
    fn save_group_by_host(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.group_by_host = self.settings.group_by_host;
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save group-by-host preference: {}", e);
        }
    }

    /// Persist the identity-details expansion state (#350).
    fn save_identity_expanded(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.identity_expanded = self.identity_expanded;
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save identity-expanded preference: {}", e);
        }
    }

    /// Persist the opt-in desktop-notifications preference (#26).
    fn save_notification_pref(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.desktop_notifications = self.settings.desktop_notifications;
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save notification preference: {}", e);
        }
    }

    /// The favorited metric names for `device_id` (#27), projected out of the
    /// global `protocol/source/metric` favorites set.
    fn device_favorites(&self, device_id: &DeviceId) -> std::collections::HashSet<String> {
        let prefix = fav_prefix(device_id);
        self.favorites
            .iter()
            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// Persist the favorited-metrics set (#27).
    fn save_favorites(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.favorite_metrics = self.favorites.iter().cloned().collect();
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save favorites: {}", e);
        }
    }

    /// Mark the topology prefs dirty (#440). The actual settings.json5
    /// load-modify-save is synchronous disk I/O on the UI thread, so it's
    /// debounced: the 1 Hz tick flushes at most once per second (and
    /// [`Message::CloseTopology`] flushes eagerly) instead of writing on
    /// every drag-end and toggle.
    fn save_topology_prefs(&mut self) {
        self.topology_prefs_dirty = true;
    }

    /// Flush pending topology-pref changes to disk, if any (#440).
    fn flush_topology_prefs(&mut self) {
        if self.topology_prefs_dirty {
            self.topology_prefs_dirty = false;
            self.write_topology_prefs();
        }
    }

    /// Persist the topology presentation prefs (#392): lens, grouping, edge
    /// labels, filters. Load-modify-save so unrelated settings are untouched
    /// (same pattern as [`Self::save_current_view`]). Focus and group
    /// expansions are session-transient by design.
    fn write_topology_prefs(&self) {
        let mut persistent = PersistentSettings::load();
        persistent.topology_lens = self.topology.prefs.lens;
        persistent.topology_grouping = self.topology.prefs.grouping;
        persistent.topology_edge_label = self.topology.prefs.edge_label;
        persistent.topology_filters = self.topology.prefs.filters;
        persistent.topology_layout = self.topology.prefs.layout;
        // Manual arrangement (#394): pinned nodes only, pruned to what exists.
        let (pins, positions) = self.topology.pinned_positions();
        persistent.topology_pinned = pins;
        persistent.topology_positions = positions;
        if let Err(e) = persistent.save() {
            tracing::error!("Failed to save topology prefs: {}", e);
        }
    }

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
        // Populate card sparklines immediately on entering a grid view so they
        // don't blink empty for up to a tick (they're otherwise rebuilt at 1 Hz).
        if self.on_dashboard_grid() {
            self.dashboard_sparks = crate::view::trend::build_device_sparks(
                &self.store,
                self.dashboard.devices.keys(),
                2,
            );
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

    /// Send a command to a sensor's control channel over Zenoh.
    ///
    /// `key` is the full command key (build with
    /// [`zensight_common::command_key`]); `body` is serialized as JSON. Returns
    /// a [`Task`] that publishes asynchronously and reports the outcome via
    /// [`Message::CommandFeedback`]. No-op feedback if disconnected. An empty
    /// `ok_message` suppresses the success toast (automatic commands);
    /// failures always toast.
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
        // v1 (RFC 05 §3): a command is a write procedure — GET with a body.
        // A value reply is the ack; a refusal arrives as a reply error
        // carrying a namespaced { error, message } payload.
        Task::future(async move {
            let replies = match session
                .get(&key)
                .payload(payload)
                .target(zenoh::query::QueryTarget::All)
                .timeout(std::time::Duration::from_secs(5))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Message::CommandFeedback {
                        success: false,
                        message: format!("Command failed: {e}"),
                    };
                }
            };
            match replies.recv_async().await {
                Ok(reply) => match reply.result() {
                    Ok(_) => Message::CommandFeedback {
                        success: true,
                        message: ok_message,
                    },
                    Err(err) => {
                        let detail =
                            serde_json::from_slice::<serde_json::Value>(&err.payload().to_bytes())
                                .ok()
                                .and_then(|v| {
                                    Some(format!(
                                        "{}: {}",
                                        v.get("error")?.as_str()?,
                                        v.get("message")?.as_str()?
                                    ))
                                })
                                .unwrap_or_else(|| "refused".to_string());
                        Message::CommandFeedback {
                            success: false,
                            message: format!("Command refused — {detail}"),
                        }
                    }
                },
                Err(_) => Message::CommandFeedback {
                    success: false,
                    message: "Command unanswered — target offline or procedure not served"
                        .to_string(),
                },
            }
        })
    }

    /// Begin an artifact request. A tree kind (snapshot) needs a destination
    /// folder, so it opens a folder picker first (→ `ArtifactDestChosen`); a blob
    /// kind (report/capture) goes straight to a temp dir + Requesting.
    fn start_artifact(
        &mut self,
        producer: String,
        kind: zensight_common::ArtifactKind,
        target_source: Option<String>,
    ) -> Option<Task<Message>> {
        if self.session.is_none() {
            self.toasts
                .push(ToastSeverity::Error, "Not connected to Zenoh".to_string());
            return None;
        }
        match &kind {
            zensight_common::ArtifactKind::Snapshot { .. } => {
                // Pick a destination folder first, then start the download.
                Some(Task::future(async move {
                    let dest = rfd::AsyncFileDialog::new()
                        .pick_folder()
                        .await
                        .map(|h| h.path().to_path_buf());
                    Message::ArtifactDestChosen {
                        producer,
                        kind,
                        target_source,
                        dest,
                    }
                }))
            }
            _ => {
                let dest = std::env::temp_dir().join("zensight-downloads");
                self.start_artifact_with_dest(producer, kind, target_source, dest)
            }
        }
    }

    /// Build the job with the resolved `dest`, set Requesting, and spawn the
    /// request/poll-to-`Ready` stream (produce-phase progress arrives as
    /// `ArtifactGenerating`, the outcome as `ArtifactRequested`).
    fn start_artifact_with_dest(
        &mut self,
        producer: String,
        kind: zensight_common::ArtifactKind,
        target_source: Option<String>,
        dest: std::path::PathBuf,
    ) -> Option<Task<Message>> {
        let session = self.session.clone()?;
        let registry = self.command_registry.clone()?;
        // A tree is reconstructed into a clearly-named subfolder of the picked dir.
        let dest = match &kind {
            zensight_common::ArtifactKind::Snapshot { dir } => {
                let sensor = producer.as_str();
                dest.join(format!("{sensor}-{dir}-snapshot"))
            }
            _ => dest,
        };
        let job =
            crate::view::artifact_fetch::ArtifactJob::new(producer.clone(), kind.clone(), dest);
        let id = job.id;
        self.artifact_job = Some(job);
        self.artifact_fetch = crate::view::artifact_fetch::ArtifactFetch::Requesting;
        Some(Task::stream(
            crate::view::artifact_fetch::request_and_stream_ready(
                session,
                registry,
                producer,
                kind,
                id,
                target_source,
            ),
        ))
    }

    /// Download an already-registered triggered-capture blob by id (#327). Skips
    /// the request/produce phase entirely (the file exists on the sensor); the
    /// stream reuses the shared ArtifactProgress/ArtifactDownloaded lifecycle so
    /// the finished file goes through the normal Save-as dialog. Pause/resume is
    /// not offered on this path (no `Delivery` is stored); Cancel works.
    fn download_capture_blob(
        &mut self,
        producer: String,
        artifact_id: String,
        filename: String,
    ) -> Option<Task<Message>> {
        if self.artifact_fetch.is_busy() {
            self.toasts.push(
                ToastSeverity::Info,
                "Another artifact download is in flight".to_string(),
            );
            return None;
        }
        let Some(session) = self.session.clone() else {
            self.toasts
                .push(ToastSeverity::Error, "Not connected to Zenoh".to_string());
            return None;
        };
        let dest = std::env::temp_dir().join("zensight-downloads");
        let mut job = crate::view::artifact_fetch::ArtifactJob::new(
            producer.clone(),
            zensight_common::ArtifactKind::Capture {
                duration_secs: 0,
                max_bytes: None,
                filter: None,
                snaplen: None,
                compress: filename.ends_with(".zst"),
            },
            dest.clone(),
        );
        if let Ok(id) = artifact_id.parse::<ulid::Ulid>() {
            job.id = id;
        }
        job.filename = Some(filename);
        let cancel = job.cancel.clone();
        self.artifact_job = Some(job);
        self.artifact_fetch =
            crate::view::artifact_fetch::ArtifactFetch::Downloading { got: 0, total: 0 };
        // The artifact id is globally unique and one host owns it — the
        // `*`-origin blob prefix reaches that owner without deriving an origin
        // locally (the old artifact_blob_prefix call built the GUI's OWN
        // origin, which is never where the capture lives).
        let blob_prefix = zensight_common::fleet_blob_prefix();
        Some(Task::stream(
            crate::view::artifact_fetch::download_blob_direct(
                session,
                blob_prefix,
                artifact_id,
                dest,
                cancel,
            ),
        ))
    }

    /// Handle the `Ready`/error outcome of an artifact request and, on `Ready`,
    /// pick the transfer client off the delivery tag and kick off the stream.
    fn on_artifact_requested(
        &mut self,
        result: Result<zensight_common::ArtifactState, String>,
    ) -> Option<Task<Message>> {
        use zensight_common::{ArtifactState, Delivery};
        match result {
            Ok(ArtifactState::Ready { delivery, .. }) => {
                let job = self.artifact_job.as_mut()?;
                job.delivery = Some(delivery.clone());
                // Total & filename depend on the delivery type (chunk count for a
                // blob, file count for a tree — matching the old per-tier behavior).
                let total = match &delivery {
                    Delivery::Blob { manifest, .. } => {
                        job.filename = Some(manifest.filename.clone());
                        manifest.chunk_count as u64
                    }
                    Delivery::Tree { summary, .. } => summary.file_count.max(1),
                };
                let id = job.id.to_string();
                let dest = job.dest.clone();
                let cancel = job.cancel.clone();
                self.artifact_fetch =
                    crate::view::artifact_fetch::ArtifactFetch::Downloading { got: 0, total };
                let session = self.session.clone()?;
                let store = self.content_store();
                Some(Task::stream(crate::view::artifact_fetch::download_stream(
                    session, delivery, id, dest, store, cancel,
                )))
            }
            Ok(_) => None, // request helper only returns Ready on success
            Err(e) => {
                self.artifact_fetch = crate::view::artifact_fetch::ArtifactFetch::Failed(e.clone());
                self.toasts
                    .push(ToastSeverity::Error, format!("Artifact failed: {e}"));
                None
            }
        }
    }

    /// Handle a finished artifact download. A blob lands in a temp file, so open a
    /// "Save as…" dialog; a tree is already reconstructed into the chosen folder,
    /// so just record `Saved`.
    fn on_artifact_downloaded(
        &mut self,
        result: Result<std::path::PathBuf, String>,
    ) -> Option<Task<Message>> {
        // Ignore if the user cancelled (job cleared). Extract the delivery-shape +
        // filename up front so no borrow of the job outlives the state mutation.
        let (is_tree, filename, producer) = {
            let job = self.artifact_job.as_ref()?;
            (
                matches!(job.delivery, Some(zensight_common::Delivery::Tree { .. })),
                job.filename.clone(),
                job.producer.clone(),
            )
        };
        // A capture (.pcap.zst) can be decompressed back to .pcap on save when the
        // sensor's form asked for it (#333).
        let decompress_on_save = self
            .capture_forms
            .get(&producer)
            .is_some_and(|f| f.decompress_on_save);
        match result {
            Ok(path) if is_tree => {
                // Tree already reconstructed into the chosen folder.
                let shown = path.display().to_string();
                self.artifact_fetch =
                    crate::view::artifact_fetch::ArtifactFetch::Saved(shown.clone());
                self.toasts
                    .push(ToastSeverity::Success, format!("Snapshot saved to {shown}"));
                None
            }
            Ok(temp_path) => {
                // Blob: move the verified temp file via a Save-as dialog.
                self.artifact_fetch = crate::view::artifact_fetch::ArtifactFetch::Verifying;
                let default_name =
                    filename.unwrap_or_else(|| "zensight-debug-report.tar.zst".to_string());
                Some(save_blob_dialog(
                    default_name,
                    temp_path,
                    decompress_on_save,
                ))
            }
            // A paused download reports Cancelled — that's expected, keep Paused.
            Err(_)
                if matches!(
                    self.artifact_fetch,
                    crate::view::artifact_fetch::ArtifactFetch::Paused { .. }
                ) =>
            {
                None
            }
            Err(e) => {
                self.artifact_fetch = crate::view::artifact_fetch::ArtifactFetch::Failed(e.clone());
                self.toasts
                    .push(ToastSeverity::Error, format!("Download failed: {e}"));
                None
            }
        }
    }

    /// Resume a paused download (a fresh stream + token; a blob resumes from its
    /// on-disk partial, a tree from the chunks already in the local content store).
    fn resume_artifact(&mut self) -> Option<Task<Message>> {
        let crate::view::artifact_fetch::ArtifactFetch::Paused { got, total } = self.artifact_fetch
        else {
            return None;
        };
        let session = self.session.clone()?;
        let store = self.content_store();
        let job = self.artifact_job.as_mut()?;
        let delivery = job.delivery.clone()?;
        let id = job.id.to_string();
        let dest = job.dest.clone();
        let cancel = job.reset_cancel();
        self.artifact_fetch =
            crate::view::artifact_fetch::ArtifactFetch::Downloading { got, total };
        Some(Task::stream(crate::view::artifact_fetch::download_stream(
            session, delivery, id, dest, store, cancel,
        )))
    }

    /// Cancel the in-flight download: stop the stream, delete a blob partial, and
    /// hint the sensor to free its ready artifact early.
    fn cancel_artifact(&mut self) -> Option<Task<Message>> {
        let job = self.artifact_job.as_ref()?;
        job.cancel.cancel();
        let session = self.session.clone()?;
        let producer = job.producer.clone();
        let id = job.id.to_string();
        // Only a blob delivery leaves an on-disk partial to clean up.
        let blob = match &job.delivery {
            Some(zensight_common::Delivery::Blob { blob_prefix, .. }) => {
                Some((blob_prefix.clone(), job.dest.clone()))
            }
            _ => None,
        };
        Some(Task::future(async move {
            if let Some((bp, dir)) = blob {
                let client =
                    zenoh_blob::BlobClient::new(session.clone(), bp, zenoh_blob::Format::Json);
                client.delete_partial(&id, &dir).await;
            }
            // Best-effort hint to the sensor (free the TTL'd artifact now) —
            // the cancel write procedure takes `?id=<ulid>` (RFC 05).
            let _ = session
                .get(format!(
                    "{}?id={}",
                    // `producer` is a producer name ("netring"); it has contained
                    // no `/` since the #465 cutover retired key_prefix.
                    zensight_common::fleet_rpc_key(&producer, "artifact/cancel"),
                    id
                ))
                .await;
            Message::ArtifactSaved(Ok(None))
        }))
    }

    /// The local content store backing Tier-2 downloads (the redb `chunks` table,
    /// so chunks dedup across snapshots and survive restart). Falls back to an
    /// in-memory store when there is no persistent store (e.g. demo mode).
    fn content_store(&self) -> std::sync::Arc<dyn zenoh_blob::ContentStore> {
        match self.store.persistent() {
            Some(p) => std::sync::Arc::new(crate::store::RedbContentStore::new(p)),
            None => std::sync::Arc::new(zenoh_blob::MemoryStore::new()),
        }
    }

    /// Query every connected sensor's `artifact/status` procedure to learn which
    /// kinds it produces (report/snapshot/capture) plus their bounds/adverts,
    /// storing the result per producer so the Sensors view renders the right
    /// affordances.
    fn load_artifact_kinds(&self) -> Option<Task<Message>> {
        let session = self.session.clone()?;
        // Artifact procedures are producer-scoped (`@rpc/<producer>/artifact/*`),
        // so derive producer names from the snapshots' sensor names — the map keys
        // are per-instance (`sensor@source`) and would produce bogus names.
        let prefixes: Vec<String> = self
            .sensor_health
            .values()
            .map(|snap| snap.sensor.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if prefixes.is_empty() {
            return None;
        }
        let tasks = prefixes.into_iter().map(|producer| {
            let session = session.clone();
            Task::future(async move {
                let kinds =
                    crate::view::artifact_fetch::load_artifact_kinds(session, producer.clone())
                        .await;
                Message::ArtifactKindsLoaded { producer, kinds }
            })
        });
        Some(Task::batch(tasks))
    }

    /// Query the netlink sentinel's current expectation set (status queryable).
    fn query_expectations(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let key = zensight_common::fleet_rpc_key("netlink", "expectations");
        Task::future(async move {
            match session
                .get(&key)
                .target(zenoh::query::QueryTarget::All)
                .await
            {
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

    /// Query the systemd sentinel's current expectation set (#278). Routes to
    /// `SystemdExpectationsReceived`.
    fn query_systemd_expectations(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let key = zensight_common::fleet_rpc_key("systemd", "expectations");
        Task::future(async move {
            match session
                .get(&key)
                .target(zenoh::query::QueryTarget::All)
                .await
            {
                Ok(replies) => {
                    if let Ok(reply) = replies.recv_async().await
                        && let Ok(sample) = reply.result()
                    {
                        let body =
                            String::from_utf8_lossy(&sample.payload().to_bytes()).to_string();
                        return Message::SystemdExpectationsReceived(body);
                    }
                    Message::CommandFeedback {
                        success: false,
                        message: "No systemd sentinel responded".to_string(),
                    }
                }
                Err(e) => Message::CommandFeedback {
                    success: false,
                    message: format!("Status query failed: {e}"),
                },
            }
        })
    }

    /// Poll `@rpc/systemd/action` after sending a unit action (#283) and toast the
    /// outcome. The short delay lets the sensor's async `JobRemoved` tracking
    /// resolve first, so the toast usually carries the real job result.
    fn query_systemd_action_status(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let key = zensight_common::fleet_rpc_key("systemd", "action");
        Task::future(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let no_reply = Message::CommandFeedback {
                success: false,
                message: "No systemd sensor replied with an action status — are actions enabled?"
                    .to_string(),
            };
            match session.get(&key).await {
                Ok(replies) => {
                    if let Ok(reply) = replies.recv_async().await
                        && let Ok(sample) = reply.result()
                        && let Ok(status) = serde_json::from_slice::<serde_json::Value>(
                            &sample.payload().to_bytes(),
                        )
                    {
                        let accepted = status["accepted"].as_bool().unwrap_or(false);
                        let unit = status["unit"].as_str().unwrap_or("?").to_string();
                        let verb = status["verb"].as_str().unwrap_or("?").to_string();
                        let result = status["result"].as_str().map(str::to_string);
                        let error = status["error"].as_str().map(str::to_string);
                        let (success, message) = match (accepted, result, error) {
                            (false, _, reason) => (
                                false,
                                format!(
                                    "{verb} {unit} rejected: {}",
                                    reason.unwrap_or_else(|| "actions disabled".into())
                                ),
                            ),
                            (true, Some(r), _) if r == "done" => {
                                (true, format!("{verb} {unit}: done"))
                            }
                            (true, Some(r), e) => (
                                false,
                                format!(
                                    "{verb} {unit}: {r}{}",
                                    e.map(|e| format!(" — {e}")).unwrap_or_default()
                                ),
                            ),
                            (true, None, _) => (true, format!("{verb} {unit} accepted (pending)")),
                        };
                        return Message::CommandFeedback { success, message };
                    }
                    no_reply
                }
                Err(e) => Message::CommandFeedback {
                    success: false,
                    message: format!("Action status query failed: {e}"),
                },
            }
        })
    }

    /// Query the netring sensor's current detector config (#121, status
    /// queryable). Routes to `DetectorConfigReceived`.
    fn query_detector_status(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::done(Message::DetectorConfigReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let key = zensight_common::fleet_rpc_key("netring", "detectors");
        Task::future(async move {
            match session.get(&key).await {
                Ok(replies) => {
                    if let Ok(reply) = replies.recv_async().await
                        && let Ok(sample) = reply.result()
                    {
                        let body =
                            String::from_utf8_lossy(&sample.payload().to_bytes()).to_string();
                        return Message::DetectorConfigReceived(Ok(body));
                    }
                    Message::DetectorConfigReceived(Err("No netring sensor responded".to_string()))
                }
                Err(e) => Message::DetectorConfigReceived(Err(format!("Status query failed: {e}"))),
            }
        })
    }

    /// Query the netring sensor's live capture-focus filter
    /// (`@rpc/netring/capture_filter`). Routes to `CaptureFilterStatusReceived`.
    fn query_capture_filter_status(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::done(Message::CaptureFilterStatusReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let key = zensight_common::fleet_rpc_key("netring", "capture_filter");
        Task::future(async move {
            match session.get(&key).await {
                Ok(replies) => {
                    if let Ok(reply) = replies.recv_async().await
                        && let Ok(sample) = reply.result()
                    {
                        let body =
                            String::from_utf8_lossy(&sample.payload().to_bytes()).to_string();
                        return Message::CaptureFilterStatusReceived(Ok(body));
                    }
                    Message::CaptureFilterStatusReceived(Err(
                        "No netring sensor responded".to_string()
                    ))
                }
                Err(e) => {
                    Message::CaptureFilterStatusReceived(Err(format!("Status query failed: {e}")))
                }
            }
        })
    }

    /// Query the netring sensor's live threat-intel status
    /// (`@rpc/netring/threat_intel`). Routes to `ThreatIntelStatusReceived`.
    fn query_threat_intel_status(&self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::done(Message::ThreatIntelStatusReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let key = zensight_common::fleet_rpc_key("netring", "threat_intel");
        Task::future(async move {
            match session.get(&key).await {
                Ok(replies) => {
                    if let Ok(reply) = replies.recv_async().await
                        && let Ok(sample) = reply.result()
                    {
                        let body =
                            String::from_utf8_lossy(&sample.payload().to_bytes()).to_string();
                        return Message::ThreatIntelStatusReceived(Ok(body));
                    }
                    Message::ThreatIntelStatusReceived(Err(
                        "No netring sensor responded".to_string()
                    ))
                }
                Err(e) => {
                    Message::ThreatIntelStatusReceived(Err(format!("Status query failed: {e}")))
                }
            }
        })
    }

    /// Fetch an on-demand netlink detail table from the sensor's query channel.
    /// Prefetch the systemd detail channel(s) a newly-activated tab renders, so
    /// the panel isn't empty until a manual refresh (#281).
    fn prefetch_systemd_tab(
        &self,
        tab: crate::view::specialized::SpecializedTab,
    ) -> Option<Task<Message>> {
        use crate::view::specialized::SpecializedTab;
        use crate::view::specialized::fetch::Fetch;
        use crate::view::specialized::systemd_detail::SystemdDetailTopic;
        let device = self.selected_device.as_ref()?;
        let topic = match tab {
            SpecializedTab::Units => SystemdDetailTopic::Units,
            SpecializedTab::Timers => SystemdDetailTopic::Timers,
            SpecializedTab::Events => SystemdDetailTopic::Events,
            SpecializedTab::Cgroups => SystemdDetailTopic::Cgroups,
            _ => return None,
        };
        // Only prefetch when we haven't already loaded/started this channel.
        let already = match topic {
            SystemdDetailTopic::Units => !matches!(device.systemd_detail.units, Fetch::Idle),
            SystemdDetailTopic::Timers => !matches!(device.systemd_detail.timers, Fetch::Idle),
            SystemdDetailTopic::Events => !matches!(device.systemd_detail.events, Fetch::Idle),
            SystemdDetailTopic::Cgroups => !matches!(device.systemd_detail.cgroups, Fetch::Idle),
        };
        if already {
            return None;
        }
        Some(self.query_systemd_detail(topic))
    }

    /// Fetch a systemd on-demand detail channel and wrap the outcome (#281).
    fn query_systemd_detail(
        &self,
        topic: crate::view::specialized::systemd_detail::SystemdDetailTopic,
    ) -> Task<Message> {
        use crate::view::specialized::netlink_detail::fetch_records;
        use crate::view::specialized::systemd_detail::{
            SystemdDetailData, SystemdDetailTopic, fetch_one,
        };
        let Some(session) = self.session.clone() else {
            return Task::done(Message::SystemdDetailReceived(
                topic,
                Err("Not connected to Zenoh".to_string()),
            ));
        };
        let key = topic.key(
            self.selected_origin_for(zensight_common::Protocol::Systemd)
                .as_deref(),
        );
        Task::future(async move {
            let data = match topic {
                SystemdDetailTopic::Units => fetch_records(session, key)
                    .await
                    .map(SystemdDetailData::Units),
                SystemdDetailTopic::Timers => fetch_records(session, key)
                    .await
                    .map(SystemdDetailData::Timers),
                SystemdDetailTopic::Events => fetch_records(session, key)
                    .await
                    .map(SystemdDetailData::Events),
                // cgroups replies a single tree object (or null), not an array.
                SystemdDetailTopic::Cgroups => {
                    Some(SystemdDetailData::Cgroups(fetch_one(session, key).await))
                }
            };
            let result =
                data.ok_or_else(|| format!("No systemd sensor responded for {}", topic.label()));
            Message::SystemdDetailReceived(topic, result)
        })
    }

    /// Fetch one unit's identity detail (the `unit?name=` procedure) for the
    /// systemd drill-down panel (#313), targeting the drilled-in host.
    fn query_systemd_unit_detail(&self, unit: String) -> Task<Message> {
        use crate::view::specialized::systemd_detail::fetch_unit_detail;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::SystemdUnitDetailReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let origin = self.selected_origin_for(zensight_common::Protocol::Systemd);
        Task::future(async move {
            let result = fetch_unit_detail(session, origin, unit)
                .await
                .ok_or_else(|| "No systemd sensor responded".to_string());
            Message::SystemdUnitDetailReceived(result)
        })
    }

    /// Cross-view pivot (#313): open the systemd device for `host` on the Units
    /// tab with `unit`'s drill-down loading. Toast fallback when the host runs
    /// no systemd sensor (missing data is the normal case, never a dead end).
    fn pivot_to_unit(&mut self, host: String, unit: String) -> Task<Message> {
        use crate::view::specialized::fetch::Fetch;
        let id = crate::message::DeviceId::new(zensight_common::Protocol::Systemd, &host);
        if !self.dashboard.devices.contains_key(&id) {
            self.toasts.push(
                ToastSeverity::Info,
                format!("No systemd sensor for host {host}"),
            );
            return Task::none();
        }
        let select = self.select_device(id);
        if let Some(device) = self.selected_device.as_mut() {
            device.specialized_tab = crate::view::specialized::SpecializedTab::Units;
            device.systemd_detail.selected_unit = Some(unit.clone());
            device.systemd_detail.unit_detail = Fetch::Loading;
            device
                .systemd_detail
                .loading(crate::view::specialized::systemd_detail::SystemdDetailTopic::Units);
        }
        Task::batch([
            select,
            self.query_systemd_unit_detail(unit),
            self.query_systemd_detail(
                crate::view::specialized::systemd_detail::SystemdDetailTopic::Units,
            ),
        ])
    }

    /// Cross-view pivot (#313): open the sysinfo device for `host` with the
    /// process explorer filtered to `pid` (`start_time` arms the
    /// stale-generation guard). Toast fallback when the host runs no sysinfo
    /// sensor.
    fn pivot_to_process(
        &mut self,
        host: String,
        pid: i32,
        start_time: Option<u64>,
    ) -> Task<Message> {
        use crate::view::specialized::sysinfo_detail::PidFilter;
        let id = crate::message::DeviceId::new(zensight_common::Protocol::Sysinfo, &host);
        if !self.dashboard.devices.contains_key(&id) {
            self.toasts.push(
                ToastSeverity::Info,
                format!("No sysinfo sensor for host {host}"),
            );
            return Task::none();
        }
        let select = self.select_device(id);
        let sort = crate::view::specialized::sysinfo_detail::ProcessSort::default();
        if let Some(device) = self.selected_device.as_mut() {
            device.sysinfo_detail.pid_filter = Some(PidFilter { pid, start_time });
            device.sysinfo_detail.loading(sort);
        }
        Task::batch([select, self.query_sysinfo_processes(host, sort)])
    }

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
        let key = topic.key(
            self.selected_origin_for(zensight_common::Protocol::Netlink)
                .as_deref(),
        );
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
                NetlinkDetailTopic::Addresses => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Addresses),
                NetlinkDetailTopic::Events => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Events),
                NetlinkDetailTopic::RouteChanges => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::RouteChanges),
                NetlinkDetailTopic::Tc => {
                    fetch_records(session, key).await.map(NetlinkDetailData::Tc)
                }
                NetlinkDetailTopic::Xfrm => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Xfrm),
                NetlinkDetailTopic::Nft => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Nft),
                NetlinkDetailTopic::Retransmits => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Retransmits),
                NetlinkDetailTopic::Connections => fetch_records(session, key)
                    .await
                    .map(NetlinkDetailData::Connections),
            };
            let result =
                data.ok_or_else(|| format!("No netlink sensor responded for {}", topic.label()));
            Message::NetlinkDetailReceived(topic, result)
        })
    }

    /// Fetch the on-demand netring flow detail from the sensor's query channel.
    /// Generic on-demand sensor query (#127): fetch a `Vec<T>` from a channel and
    /// wrap the outcome in a message. Collapses the ~near-identical
    /// `query_netring_*` wrappers into one-liners. When disconnected the
    /// "Not connected" error is routed into the *same* channel (so the panel shows
    /// it, no toast); a non-responding sensor yields the channel's error state.
    /// `prefetch_on_open` already no-ops while disconnected, so this branch only
    /// fires on an explicit fetch.
    /// The v1 origin id (`h-<12hex>`) for a payload `source` (hostname), from
    /// the health/registration/entity-fed [`Self::origins`] map. `None` until
    /// the first health doc arrives (~5 s after connect) — callers fall back
    /// to the fleet selector.
    fn origin_for(&self, source: &str) -> Option<String> {
        self.origins.get(source).cloned()
    }

    /// Re-project the selected device's origin and focus flag (#476).
    ///
    /// The source→origin map fills in asynchronously (health/registration/entity
    /// docs), so a device selected in the first few seconds has no origin yet
    /// and its Focus control starts disabled. Call this whenever the map or the
    /// link's focus changes.
    fn refresh_focus_state(&mut self) {
        let focus = self.link.focus.clone();
        if let Some(device) = self.selected_device.as_mut() {
            let origin = self.origins.get(&device.device_id.source).cloned();
            device.focused = origin.is_some() && origin == focus;
            device.origin = origin;
        }
    }

    /// The parallax `stream/set` write key for `source`'s host: the concrete
    /// origin key when the origin is known, else the fleet selector (the
    /// command carries the stream name, and send_command targets All).
    fn parallax_stream_set_key(&self, source: &str) -> String {
        match self.origin_for(source) {
            Some(origin) => zensight_common::origin_rpc_key(&origin, "parallax", "stream/set"),
            None => zensight_common::fleet_command_key("parallax", "stream"),
        }
    }

    /// The v1 origin of the currently-selected device when it belongs to
    /// `proto` — detail-tab fetches target that host's concrete @rpc key;
    /// `None` (no selection, or origin not yet learned) falls back to the
    /// fleet selector.
    fn selected_origin_for(&self, proto: zensight_common::Protocol) -> Option<String> {
        self.selected_device
            .as_ref()
            .filter(|d| d.device_id.protocol == proto)
            .and_then(|d| self.origin_for(&d.device_id.source))
    }

    fn query_channel<T, Fut>(
        &self,
        fetch: impl FnOnce(std::sync::Arc<zenoh::Session>) -> Fut + Send + 'static,
        into_message: impl FnOnce(Result<Vec<T>, String>) -> Message + Send + 'static,
        not_responding: &'static str,
    ) -> Task<Message>
    where
        Fut: std::future::Future<Output = Option<Vec<T>>> + Send + 'static,
        T: Send + 'static,
    {
        let Some(session) = self.session.clone() else {
            return Task::done(into_message(Err("Not connected to Zenoh".to_string())));
        };
        Task::future(async move {
            let result = fetch(session)
                .await
                .ok_or_else(|| not_responding.to_string());
            into_message(result)
        })
    }

    /// On tab activation (#243), prefetch the on-demand channels that back a
    /// netring tab — but only those still `Idle`, so we never clobber loaded
    /// data or re-fire an in-flight request. Returns a batched task, or `None`
    /// when the tab is fully streamed (no queryables) or everything is fetched.
    fn prefetch_netring_tab(
        &mut self,
        tab: crate::view::specialized::SpecializedTab,
    ) -> Option<Task<Message>> {
        use crate::view::specialized::SpecializedTab as T;
        use crate::view::specialized::fetch::Fetch;

        let nd = &self.selected_device.as_ref()?.netring_detail;
        // The Capture tab's file index (#327) is its own on-demand channel.
        if matches!(tab, T::Capture) {
            if matches!(nd.captures, Fetch::Idle) {
                if let Some(device) = self.selected_device.as_mut() {
                    device.netring_detail.loading_captures();
                }
                return Some(self.query_netring_captures());
            }
            return None;
        }
        // Per-tab channel needs (flows, elephants, talkers, matrix, dns, http,
        // tls, quic, ssh, assets); overview/bandwidth/security stream.
        let (
            mut flows,
            mut elephants,
            mut talkers,
            mut matrix,
            mut dns,
            mut http,
            mut tls,
            mut quic,
            mut ssh,
            mut assets,
        ) = match tab {
            T::Flows => (
                true, true, false, false, false, false, false, false, false, false,
            ),
            T::TalkersMatrix => (
                false, false, true, true, false, false, false, false, false, false,
            ),
            T::Dns => (
                false, false, false, false, true, false, false, false, false, false,
            ),
            T::HttpTls => (
                false, false, false, false, false, true, true, true, true, false,
            ),
            T::Assets => (
                false, false, false, false, false, false, false, false, false, true,
            ),
            _ => return None,
        };
        // Only fetch idle channels.
        flows &= matches!(nd.flows, Fetch::Idle);
        elephants &= matches!(nd.elephants, Fetch::Idle);
        talkers &= matches!(nd.talkers, Fetch::Idle);
        matrix &= matches!(nd.matrix, Fetch::Idle);
        dns &= matches!(nd.dns, Fetch::Idle);
        http &= matches!(nd.http, Fetch::Idle);
        tls &= matches!(nd.tls, Fetch::Idle);
        quic &= matches!(nd.quic, Fetch::Idle);
        ssh &= matches!(nd.ssh, Fetch::Idle);
        assets &= matches!(nd.assets, Fetch::Idle);
        if !(flows || elephants || talkers || matrix || dns || http || tls || quic || ssh || assets)
        {
            return None;
        }
        // Mark loading (mutable borrow ends before we build the &self tasks).
        if let Some(device) = self.selected_device.as_mut() {
            let d = &mut device.netring_detail;
            if flows {
                d.loading();
            }
            if elephants {
                d.loading_elephants();
            }
            if talkers {
                d.loading_talkers();
            }
            if matrix {
                d.loading_matrix();
            }
            if dns {
                d.loading_dns();
                d.loading_encrypted_dns();
            }
            if http {
                d.loading_http();
            }
            if tls {
                d.loading_tls();
            }
            if quic {
                d.loading_quic();
            }
            if ssh {
                d.loading_ssh();
            }
            if assets {
                d.loading_assets();
            }
        }
        let mut tasks: Vec<Task<Message>> = Vec::new();
        if flows {
            tasks.push(self.query_netring_flows());
        }
        if elephants {
            tasks.push(self.query_netring_elephants());
        }
        if talkers {
            tasks.push(self.query_netring_talkers());
        }
        if matrix {
            tasks.push(self.query_netring_matrix());
        }
        if dns {
            tasks.push(self.query_netring_dns());
            // Encrypted DNS rides the same tab: it is precisely what the
            // cleartext RED rollups cannot see, so showing one without the
            // other is how a DoH tunnel stays invisible.
            tasks.push(self.query_netring_encrypted_dns());
        }
        if http {
            tasks.push(self.query_netring_http());
        }
        if tls {
            tasks.push(self.query_netring_tls());
        }
        if quic {
            tasks.push(self.query_netring_quic());
        }
        if ssh {
            tasks.push(self.query_netring_ssh());
        }
        if assets {
            tasks.push(self.query_netring_assets());
        }
        Some(Task::batch(tasks))
    }

    /// Prefetch the on-demand `@rpc/netlink/*` procedures a newly-activated netlink tab
    /// needs, so tabs populate without a manual "Fetch" click (#258). Only idle
    /// channels are fetched; Overview/Interfaces/WireGuard stream live.
    fn prefetch_netlink_tab(
        &mut self,
        tab: crate::view::specialized::SpecializedTab,
    ) -> Option<Task<Message>> {
        use crate::view::specialized::SpecializedTab as T;
        use crate::view::specialized::fetch::Fetch;
        use crate::view::specialized::netlink_detail::NetlinkDetailTopic as Topic;

        let topics: &[Topic] = match tab {
            // eBPF retransmits/connections (#269) are served only on eBPF-enabled
            // hosts; a non-responding host just leaves them Error (rendered as a
            // hint), so prefetching them unconditionally is safe.
            T::Sockets => &[Topic::Sockets, Topic::Retransmits, Topic::Connections],
            T::RoutingNeighbors => &[
                Topic::Routes,
                Topic::Neighbors,
                Topic::Addresses,
                Topic::RouteChanges,
            ],
            T::Qos => &[Topic::Tc],
            T::FirewallIpsec => &[Topic::Xfrm, Topic::Nft],
            T::Events => &[Topic::Events],
            _ => return None,
        };

        let d = &self.selected_device.as_ref()?.netlink_detail;
        let is_idle = |t: Topic| match t {
            Topic::Sockets => matches!(d.sockets, Fetch::Idle),
            Topic::Routes => matches!(d.routes, Fetch::Idle),
            Topic::Neighbors => matches!(d.neighbors, Fetch::Idle),
            Topic::Addresses => matches!(d.addresses, Fetch::Idle),
            Topic::Events => matches!(d.events, Fetch::Idle),
            Topic::RouteChanges => matches!(d.route_changes, Fetch::Idle),
            Topic::Tc => matches!(d.tc, Fetch::Idle),
            Topic::Xfrm => matches!(d.xfrm, Fetch::Idle),
            Topic::Nft => matches!(d.nft, Fetch::Idle),
            Topic::Retransmits => matches!(d.retransmits, Fetch::Idle),
            Topic::Connections => matches!(d.connections, Fetch::Idle),
        };
        let todo: Vec<Topic> = topics.iter().copied().filter(|t| is_idle(*t)).collect();
        if todo.is_empty() {
            return None;
        }
        if let Some(device) = self.selected_device.as_mut() {
            for t in &todo {
                device.netlink_detail.loading(*t);
            }
        }
        Some(Task::batch(
            todo.into_iter().map(|t| self.query_netlink_detail(t)),
        ))
    }

    fn query_netring_flows(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_flows;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_flows(s, origin)
            },
            Message::NetringFlowsReceived,
            "No netring sensor responded",
        )
    }

    /// Fetch the mesh-wide listen-socket table for the selected topology node
    /// (#393): every netlink sensor replies; rows are filtered to the node's
    /// addresses (plus wildcard listeners) on receipt. Fetched on selection,
    /// never on tick.
    fn query_topology_listen_sockets(&mut self, node_id: String) -> Task<Message> {
        use crate::view::specialized::fetch::Fetch;
        use crate::view::specialized::netlink_detail::fetch_records_all;
        // Only monitored netlink hosts can answer; skip the noise otherwise.
        let has_netlink = self
            .topology
            .nodes
            .get(&node_id)
            .map(|n| n.protocols.contains(&zensight_common::Protocol::Netlink))
            .unwrap_or(false);
        if !has_netlink {
            return Task::none();
        }
        let Some(session) = self.session.clone() else {
            self.topology.panel.listen = Fetch::Error("Not connected to Zenoh".to_string());
            return Task::none();
        };
        self.topology.panel.listen = Fetch::Loading;
        let key = format!(
            "{}?state=listen",
            zensight_common::fleet_rpc_key("netlink", "sockets")
        );
        Task::future(async move {
            let result = fetch_records_all::<zensight_common::SocketRecord>(session, key)
                .await
                .ok_or_else(|| "No netlink sensor responded".to_string());
            Message::TopologyListenSocketsReceived(node_id, result)
        })
    }

    /// Fetch recent flows for the selected topology edge (#393); filtered to
    /// the edge's endpoints on receipt. Fetched on selection, never on tick.
    fn query_topology_edge_flows(&mut self, edge_index: usize) -> Task<Message> {
        use crate::view::specialized::fetch::Fetch;
        use crate::view::specialized::netring_detail::fetch_flows;
        // Only flow edges have flow detail behind them.
        let is_flow = self
            .topology
            .edges
            .get(edge_index)
            .map(|e| e.kind == crate::view::topology::EdgeKind::Flow)
            .unwrap_or(false);
        if !is_flow {
            return Task::none();
        }
        let Some(session) = self.session.clone() else {
            self.topology.panel.edge_flows = Fetch::Error("Not connected to Zenoh".to_string());
            return Task::none();
        };
        self.topology.panel.edge_flows = Fetch::Loading;
        Task::future(async move {
            let result = fetch_flows(session, None)
                .await
                .ok_or_else(|| "No netring sensor responded".to_string());
            Message::TopologyEdgeFlowsReceived(edge_index, result)
        })
    }

    /// Keep only the flows that run between the selected edge's endpoints
    /// (#393). An Internet endpoint matches any unmapped public address.
    fn filter_flows_to_edge(
        &self,
        edge_index: usize,
        flows: Vec<zensight_common::FlowRecord>,
    ) -> Vec<zensight_common::FlowRecord> {
        use crate::view::topology::{INTERNET_NODE_ID, endpoint_ip, is_public_ip};
        let Some(edge) = self.topology.edges.get(edge_index) else {
            return Vec::new();
        };
        let ip_to_node = self.topology_ip_to_node();
        let side = |node_id: &str, ip: &str| -> bool {
            if node_id == INTERNET_NODE_ID {
                is_public_ip(ip) && !ip_to_node.contains_key(ip)
            } else {
                ip_to_node.get(ip).map(String::as_str) == Some(node_id)
            }
        };
        flows
            .into_iter()
            .filter(|f| {
                let src = endpoint_ip(&f.src);
                let dst = endpoint_ip(&f.dst);
                (side(&edge.from, src) && side(&edge.to, dst))
                    || (side(&edge.to, src) && side(&edge.from, dst))
            })
            .collect()
    }

    /// The full topology data-refresh batch (#391): flows + neighbors +
    /// matrix + assets, fetched concurrently and landed as ONE
    /// `TopologyBatchReceived` so the edge set rebuilds once per batch
    /// (#440). Issued on view entry and re-issued periodically while the
    /// view is open. Demo serves no queryables (session is None), so the
    /// demo branch feeds the synthetic matrix + asset fleet instead — the
    /// same wire contracts, per the demo/mock contract.
    fn query_topology_batch(&self) -> Task<Message> {
        use crate::view::topology::TopologyBatch;
        if self.demo_mode {
            return Task::done(Message::TopologyBatchReceived(TopologyBatch {
                matrix: Some(crate::mock::netring::matrix()),
                assets: Some(crate::mock::netring::assets()),
                ..Default::default()
            }));
        }
        use crate::view::specialized::netlink_detail::fetch_records;
        use crate::view::specialized::netring_detail::{fetch_assets, fetch_flows, fetch_matrix};
        let Some(session) = self.session.clone() else {
            // Not connected: leave edges as-is, no error toast.
            return Task::none();
        };
        let neighbors_key = zensight_common::fleet_rpc_key("netlink", "neighbors");
        Task::future(async move {
            let (flows, neighbors, matrix, assets) = tokio::join!(
                fetch_flows(session.clone(), None),
                fetch_records::<zensight_common::NeighborRecord>(session.clone(), neighbors_key),
                fetch_matrix(session.clone(), None),
                fetch_assets(session, None),
            );
            Message::TopologyBatchReceived(TopologyBatch {
                flows,
                neighbors,
                matrix,
                assets,
            })
        })
    }

    /// Piggybacked on the 1 Hz tick: while the topology view is open, re-issue
    /// the topology queries every [`TOPOLOGY_REFRESH_TICKS`] seconds so edge
    /// rates stay live (#391).
    fn maybe_refresh_topology(&mut self) -> Option<Task<Message>> {
        if self.current_view != CurrentView::Topology {
            self.topology_refresh_ticks = 0;
            return None;
        }
        self.topology_refresh_ticks = self.topology_refresh_ticks.saturating_add(1);
        if self.topology_refresh_ticks < TOPOLOGY_REFRESH_TICKS {
            return None;
        }
        self.topology_refresh_ticks = 0;
        Some(self.query_topology_batch())
    }

    /// Build a map from endpoint IP to topology node id (#25/#306). A node whose
    /// `source` is itself an IP maps directly; and each correlator entity's
    /// identifying IPs map to that entity's node (or a member device's node),
    /// bridging wire-level flow edges to hostname nodes.
    fn topology_ip_to_node(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        // Direct: a node whose id looks like an IP maps that IP to itself.
        for node_id in self.topology.nodes.keys() {
            map.insert(node_id.clone(), node_id.clone());
        }
        // Entity IPs (#306): an identifying IP resolves to the entity node when
        // present, else to a member device's node — feeds apply_flow_edges.
        for entity in self.entities.hosts.values() {
            let node_id = if self.topology.nodes.contains_key(&entity.entity_id) {
                Some(entity.entity_id.clone())
            } else {
                entity.members.iter().find_map(|m| {
                    let src = &m.source;
                    self.topology.nodes.contains_key(src).then(|| src.clone())
                })
            };
            if let Some(node_id) = node_id {
                for ip in &entity.ips {
                    map.entry(ip.clone()).or_insert_with(|| node_id.clone());
                }
            }
        }
        map
    }

    /// Build a map from normalized MAC to topology node id (#391), mirroring
    /// [`Self::topology_ip_to_node`] — the join key for the MAC-keyed netring
    /// asset inventory.
    fn topology_mac_to_node(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for entity in self.entities.hosts.values() {
            let node_id = if self.topology.nodes.contains_key(&entity.entity_id) {
                Some(entity.entity_id.clone())
            } else {
                entity.members.iter().find_map(|m| {
                    let src = &m.source;
                    self.topology.nodes.contains_key(src).then(|| src.clone())
                })
            };
            if let Some(node_id) = node_id {
                for mac in &entity.macs {
                    map.entry(crate::entity::normalize_mac(mac))
                        .or_insert_with(|| node_id.clone());
                }
            }
        }
        map
    }

    /// Re-derive entity-dependent view models after the [`EntityStore`] changes
    /// (#306). Currently refreshes the topology; the dashboard/host views read
    /// the store live at render time.
    fn rederive_entities(&mut self) {
        self.refresh_topology_nodes();
    }

    /// Refresh topology nodes from device/entity state, then apply any changed
    /// default-gateway edges (#391). Gateway application is change-gated inside
    /// [`TopologyState::apply_gateway_edges`], so calling this at 1 Hz is cheap.
    fn refresh_topology_nodes(&mut self) {
        // Device-group labels per node (#392): first assigned group wins,
        // resolved through the same device→entity mapping as node keying.
        let mut group_labels = std::collections::HashMap::new();
        for device_id in self.dashboard.devices.keys() {
            let groups = self.groups.device_groups(device_id);
            let Some(group) = groups.first() else {
                continue;
            };
            let node_id = match self.entities.by_device.get(device_id) {
                Some(eid) => self.entities.resolve_alias(eid).to_string(),
                None => device_id.source.clone(),
            };
            group_labels
                .entry(node_id)
                .or_insert_with(|| group.name.clone());
        }
        if self.topology.group_labels != group_labels {
            self.topology.group_labels = group_labels;
            self.topology.invalidate();
        }
        self.topology.update_from_devices(
            &self.dashboard.devices,
            &self.entities,
            &self.sensor_health,
            now_ms(),
        );
        let ip_to_node = self.topology_ip_to_node();
        self.topology.apply_gateway_edges(&ip_to_node, now_ms());
        // Live node rx/tx rates from hot-ring counter deltas (#391) — only
        // worth the store scan while the map is on screen.
        if self.current_view == CurrentView::Topology {
            let rates = self.topology_rates();
            self.topology.apply_rates(&rates);
        }
    }

    /// Sum per-interface `network/*/{rx,tx}_bytes` counter deltas from the hot
    /// ring into a bytes/sec pair per topology node (#391). Sysinfo facets
    /// only — the canonical per-host NIC counters.
    fn topology_rates(&self) -> std::collections::HashMap<String, (f64, f64)> {
        use zensight_common::Protocol;
        let mut rates: std::collections::HashMap<String, (f64, f64)> =
            std::collections::HashMap::new();
        for (device_id, device_state) in &self.dashboard.devices {
            if device_id.protocol != Protocol::Sysinfo {
                continue;
            }
            let node_id = match self.entities.by_device.get(device_id) {
                Some(eid) => self.entities.resolve_alias(eid).to_string(),
                None => device_id.source.clone(),
            };
            let mut rx = 0.0f64;
            let mut tx = 0.0f64;
            let mut saw = false;
            for metric in device_state.metrics.keys() {
                // sysinfo `network/{iface}/{rx,tx}_bytes`, via the registry (#475).
                use zensight_keyspace::registry::sysinfo::Subject as SysSubject;
                let is_rx = match SysSubject::parse_metric(metric) {
                    Some(SysSubject::NetworkRxBytes { .. }) => true,
                    Some(SysSubject::NetworkTxBytes { .. }) => false,
                    _ => continue,
                };
                let key = format!("{}/{}|{}", device_id.protocol, device_id.source, metric);
                if let Some(rate) =
                    crate::view::topology::counter_rate(&self.store.hot_samples(&key))
                {
                    saw = true;
                    if is_rx {
                        rx += rate;
                    } else {
                        tx += rate;
                    }
                }
            }
            if saw {
                let entry = rates.entry(node_id).or_insert((0.0, 0.0));
                entry.0 += rx;
                entry.1 += tx;
            }
        }
        rates
    }

    /// Fetch the on-demand netring TLS asset inventory.
    fn query_netring_tls(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_tls;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_tls(s, origin)
            },
            Message::NetringTlsReceived,
            "No netring sensor responded",
        )
    }

    /// Fetch the on-demand netring QUIC SNI/ALPN inventory (#72).
    fn query_netring_quic(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_quic;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_quic(s, origin)
            },
            Message::NetringQuicReceived,
            "No QUIC data — is the netring sensor running with collect.quic enabled?",
        )
    }

    /// Fetch the on-demand netring SSH/HASSH inventory (#72).
    fn query_netring_ssh(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_ssh;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_ssh(s, origin)
            },
            Message::NetringSshReceived,
            "No SSH data — is the netring sensor running with collect.ssh enabled?",
        )
    }

    /// Fetch the on-demand netring JA4H HTTP-fingerprint inventory (#256).
    fn query_netring_ja4h(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_ja4h;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_ja4h(s, origin)
            },
            Message::NetringJa4hReceived,
            "No JA4H data — needs a netring sensor built with the ja4plus feature and collect.http_fp enabled",
        )
    }

    /// Fetch the on-demand netring passive asset inventory (#70).
    fn query_netring_assets(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_assets;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_assets(s, origin)
            },
            Message::NetringAssetsReceived,
            "No netring sensor responded",
        )
    }

    /// Combined fetch for the first-class inventory view (#120): assets + the
    /// TLS/QUIC/SSH fingerprint inventories, fetched concurrently from the global
    /// `@rpc/netring/*` procedures and folded into one [`InventoryData`].
    fn query_inventory(&self) -> Task<Message> {
        use crate::view::inventory::InventoryData;
        use crate::view::specialized::netring_detail::{
            fetch_assets, fetch_ja4h, fetch_quic, fetch_ssh, fetch_tls,
        };
        // Demo mode serves no queryables (session is None), so hand the inventory
        // view a synthetic enriched fleet (#329) instead of an empty table.
        if self.demo_mode {
            return Task::done(Message::InventoryLoaded(Ok(InventoryData {
                assets: crate::mock::netring::assets(),
                assets_responded: true,
                tls: Vec::new(),
                quic: Vec::new(),
                ssh: Vec::new(),
                ja4h: Vec::new(),
            })));
        }
        let Some(session) = self.session.clone() else {
            return Task::done(Message::InventoryLoaded(Err(
                "Not connected to Zenoh".to_string()
            )));
        };
        Task::future(async move {
            // Fetch all inventories concurrently; an empty/absent channel just
            // yields an empty table rather than failing the whole view. JA4H is
            // only populated when the sensor was built with `--features ja4plus`.
            let (assets, tls, quic, ssh, ja4h) = tokio::join!(
                fetch_assets(session.clone(), None),
                fetch_tls(session.clone(), None),
                fetch_quic(session.clone(), None),
                fetch_ssh(session.clone(), None),
                fetch_ja4h(session.clone(), None),
            );
            if assets.is_none()
                && tls.is_none()
                && quic.is_none()
                && ssh.is_none()
                && ja4h.is_none()
            {
                return Message::InventoryLoaded(Err("No netring sensor responded".to_string()));
            }
            Message::InventoryLoaded(Ok(InventoryData {
                assets_responded: assets.is_some(),
                assets: assets.unwrap_or_default(),
                tls: tls.unwrap_or_default(),
                quic: quic.unwrap_or_default(),
                ssh: ssh.unwrap_or_default(),
                ja4h: ja4h.unwrap_or_default(),
            }))
        })
    }

    /// Fetch the on-demand netring top-talker histogram (#45).
    fn query_netring_talkers(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_talkers;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_talkers(s, origin)
            },
            Message::NetringTalkersReceived,
            "No netring sensor responded",
        )
    }

    /// Fetch the on-demand netring `(src,dst)` traffic matrix / service map (#122).
    fn query_netring_matrix(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_matrix;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_matrix(s, origin)
            },
            Message::NetringMatrixReceived,
            "No netring sensor responded",
        )
    }

    /// Fetch the on-demand netring elephant-flow ring (#45).
    fn query_netring_elephants(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_elephants;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_elephants(s, origin)
            },
            Message::NetringElephantsReceived,
            "No netring sensor responded",
        )
    }

    /// Fetch the on-demand netring per-SLD DNS detail (#45).
    fn query_netring_dns(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_dns;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_dns(s, origin)
            },
            Message::NetringDnsReceived,
            "No DNS data — is the netring sensor running with collect.dns enabled?",
        )
    }

    /// Fetch the passive encrypted-DNS destination inventory (#326).
    fn query_netring_encrypted_dns(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_encrypted_dns;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_encrypted_dns(s, origin)
            },
            Message::NetringEncryptedDnsReceived,
            "No encrypted-DNS data — is the netring sensor running with collect.dns enabled?",
        )
    }

    /// Fetch the on-demand netring per-host HTTP detail (#45).
    fn query_netring_http(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_http;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_http(s, origin)
            },
            Message::NetringHttpReceived,
            "No HTTP data — is the netring sensor running with collect.http enabled?",
        )
    }

    /// Fetch the capture-to-disk file index for the device Capture tab (#327).
    fn query_netring_captures(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_captures;
        self.query_channel(
            {
                let origin = self.selected_origin_for(zensight_common::Protocol::Netring);
                move |s| fetch_captures(s, origin)
            },
            Message::NetringCapturesReceived,
            "No captures — is capture.to_disk enabled on the netring sensor?",
        )
    }

    /// Fetch the capture-to-disk index for the Security drill-down (#327), so an
    /// expanded anomaly can offer its matching triggered capture.
    fn query_anomaly_captures(&self) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_captures;
        self.query_channel(
            |s| fetch_captures(s, None),
            Message::AnomalyCapturesReceived,
            "no capture index",
        )
    }

    /// Pivot from a Security anomaly to its netring flows (#119): fetch the
    /// recent-flow ring and keep only flows whose src or dst IP matches the
    /// anomaly's offending source. Client-side filtering keeps the sensor's
    /// `@rpc/netring/flows` contract unchanged.
    fn query_anomaly_flows(&self, key: String, src: String) -> Task<Message> {
        use crate::view::specialized::netring_detail::fetch_flows;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::AnomalyFlowsReceived(
                key,
                Err("Not connected to Zenoh".to_string()),
            ));
        };
        // The anomaly src is `ip:port` or `ip`; reduce it to the bare IP so it
        // matches both directions of a flow's `ip:port` endpoints.
        let want_ip = endpoint_ip(&src);
        Task::future(async move {
            let result = match fetch_flows(session, None).await {
                Some(flows) => Ok(flows
                    .into_iter()
                    .filter(|f| endpoint_ip(&f.src) == want_ip || endpoint_ip(&f.dst) == want_ip)
                    .collect()),
                None => Err("No netring sensor responded".to_string()),
            };
            Message::AnomalyFlowsReceived(key, result)
        })
    }

    /// Flow ↔ process join (#309): fetch every netlink sensor's sockets
    /// narrowed to the flow's endpoint IPs (`?ip=`, server-side), then match
    /// the 5-tuple. Only the host that actually owns an endpoint can hold a
    /// matching socket, so the tuple match is itself host-discriminating — no
    /// per-host key needed.
    fn query_flow_attribution(
        &self,
        target: crate::message::AttributionTarget,
        key: String,
        src: String,
        dst: String,
    ) -> Task<Message> {
        use crate::view::specialized::attribution::match_flow_socket;
        use crate::view::specialized::netlink_detail::{fetch_records_all, sockets_match_key};
        let Some(session) = self.session.clone() else {
            return Task::done(Message::FlowAttributionReceived {
                target,
                key,
                result: Err("Not connected to Zenoh".to_string()),
            });
        };
        Task::future(async move {
            let src_ip = endpoint_ip(&src);
            let dst_ip = endpoint_ip(&dst);
            let a: Option<Vec<zensight_common::SocketRecord>> =
                fetch_records_all(session.clone(), sockets_match_key(&src_ip)).await;
            let b: Option<Vec<zensight_common::SocketRecord>> = if dst_ip != src_ip {
                fetch_records_all(session, sockets_match_key(&dst_ip)).await
            } else {
                None
            };
            let result = match (a, b) {
                (None, None) => Err("no netlink sensor responded".to_string()),
                (a, b) => {
                    let mut sockets = a.unwrap_or_default();
                    sockets.extend(b.unwrap_or_default());
                    Ok(match_flow_socket(&sockets, &src, &dst))
                }
            };
            Message::FlowAttributionReceived {
                target,
                key,
                result,
            }
        })
    }

    /// Fetch the on-demand sysinfo process explorer for `host` (#47). The sysinfo
    /// query channel is host-scoped, so the key carries the device source.
    fn query_sysinfo_processes(
        &self,
        host: String,
        sort: crate::view::specialized::sysinfo_detail::ProcessSort,
    ) -> Task<Message> {
        use crate::view::specialized::sysinfo_detail::fetch_processes;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::SysinfoProcessesReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let origin = self.origin_for(&host);
        Task::future(async move {
            let result = fetch_processes(session, origin, sort)
                .await
                .ok_or_else(|| "No sysinfo sensor responded".to_string());
            Message::SysinfoProcessesReceived(result)
        })
    }

    /// Fetch the recent-flow ring for the selected exporter's host (#469).
    ///
    /// The exporter's `source` is the *exporter* name, not the host running the
    /// collector, so the origin comes from the device's origin map like every
    /// other drill-down.
    fn query_netflow_flows(&self, host: String) -> Task<Message> {
        use crate::view::specialized::netflow_detail::fetch_flows;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::NetflowFlowsReceived(Err(
                "Not connected to Zenoh".to_string()
            )));
        };
        let origin = self.origin_for(&host);
        Task::future(async move {
            let result = fetch_flows(session, origin)
                .await
                .ok_or_else(|| "No netflow sensor responded".to_string());
            Message::NetflowFlowsReceived(result)
        })
    }

    /// Fetch the eBPF saturation histograms for `host` (#99).
    ///
    /// The sensor declares this queryable even without the `ebpf` feature (it
    /// replies `available: false`), so "no sensor responded" and "not built with
    /// eBPF" are genuinely different answers — and the view says which.
    fn query_sysinfo_latency(&self, host: String) -> Task<Message> {
        use crate::view::specialized::sysinfo_detail::fetch_latency;
        let Some(session) = self.session.clone() else {
            return Task::done(Message::SysinfoLatencyReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let origin = self.origin_for(&host);
        Task::future(async move {
            let result = fetch_latency(session, origin)
                .await
                .ok_or_else(|| "No sysinfo sensor responded".to_string());
            Message::SysinfoLatencyReceived(result)
        })
    }

    /// Fetch the parallax stream catalogue for `host` (#408). Demo mode
    /// serves the mock catalogue (demo mirrors the wire contract; demo never
    /// serves queryables).
    fn query_parallax_streams(&self, host: String) -> Task<Message> {
        if self.demo_mode {
            return Task::done(Message::ParallaxStreamsReceived(Ok(
                crate::mock::parallax::streams(),
            )));
        }
        let Some(session) = self.session.clone() else {
            return Task::done(Message::ParallaxStreamsReceived(Err(
                "Not connected to Zenoh".to_string(),
            )));
        };
        let origin = self.origin_for(&host);
        Task::future(async move {
            let result = crate::view::specialized::parallax_detail::fetch_streams(session, origin)
                .await
                .ok_or_else(|| "No parallax sensor responded".to_string());
            Message::ParallaxStreamsReceived(result)
        })
    }

    /// Open a live preview tile (#408): send `open_stream` (codec `mjpeg`)
    /// and spawn the abortable per-tile subscriber task. The abort handle is
    /// stored on the tile via `abort_on_drop()` so dropping the tile state
    /// always kills the subscriber (which is the sensor's falling-edge
    /// teardown signal).
    fn open_parallax_tile(&mut self, stream: String) -> Task<Message> {
        let Some(source) = self
            .selected_device
            .as_ref()
            .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
            .map(|d| d.device_id.source.clone())
        else {
            return Task::none();
        };
        if self.demo_mode {
            // Demo: contract-shaped tile state without a live subscriber —
            // the tile renders its placeholder frame.
            if let Some(device) = self.selected_device.as_mut() {
                let generation = device.parallax_detail.allocate_generation();
                device
                    .parallax_detail
                    .open_tile(&stream, generation, None, false);
            }
            return Task::none();
        }
        let Some(session) = self.session.clone() else {
            self.toasts
                .push(ToastSeverity::Error, "Not connected to Zenoh".to_string());
            return Task::none();
        };
        // Switching an open video tile back to preview (collapse restoring
        // the pre-expand profile, #436) replaces the tile state, so the
        // earlier `open_stream` must be balanced with a `close_stream` —
        // same ordering rationale as the preview→video switch below.
        let was_open = self
            .selected_device
            .as_ref()
            .is_some_and(|d| d.parallax_detail.is_open(&stream));
        let Some(generation) = self
            .selected_device
            .as_mut()
            .map(|d| d.parallax_detail.allocate_generation())
        else {
            return Task::none();
        };
        // Media keys are origin-scoped; `*` (a legal subscriber wildcard)
        // covers the window before the first health doc maps the origin.
        let media_origin = self.origin_for(&source).unwrap_or_else(|| "*".into());
        let (frames, handle) = Task::stream(
            crate::view::specialized::parallax_detail::preview_tile_stream(
                session,
                media_origin,
                stream.clone(),
                generation,
            ),
        )
        .abortable();
        if let Some(device) = self.selected_device.as_mut() {
            device.parallax_detail.open_tile(
                &stream,
                generation,
                Some(handle.abort_on_drop()),
                false,
            );
        }
        let cmd_key = self.parallax_stream_set_key(&source);
        let open =
            zensight_common::command::Command::new(zensight_common::StreamControl::OpenStream {
                stream: stream.clone(),
                codec: Some("mjpeg".to_string()),
                max_height: None,
            });
        let mut send = self.send_command(
            cmd_key.clone(),
            &open,
            format!("Opened preview for {stream}"),
        );
        if was_open {
            let close = zensight_common::command::Command::new(
                zensight_common::StreamControl::CloseStream {
                    stream: stream.clone(),
                },
            );
            send = self
                .send_command(cmd_key, &close, format!("Closed video for {stream}"))
                .chain(send);
        }
        Task::batch([send, frames])
    }

    /// Close a preview tile (#408): abort its subscriber task, drop it, and
    /// send `close_stream` so the sensor reaps without the idle timeout.
    fn close_parallax_tile(&mut self, stream: String) -> Task<Message> {
        let Some(source) = self
            .selected_device
            .as_ref()
            .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
            .map(|d| d.device_id.source.clone())
        else {
            return Task::none();
        };
        if let Some(device) = self.selected_device.as_mut() {
            device.parallax_detail.close_tile(&stream);
        }
        if self.demo_mode || self.command_registry.is_none() {
            return Task::none();
        }
        let close =
            zensight_common::command::Command::new(zensight_common::StreamControl::CloseStream {
                stream: stream.clone(),
            });
        self.send_command(
            self.parallax_stream_set_key(&source),
            &close,
            format!("Closed preview for {stream}"),
        )
    }

    /// Open a live H.264 video tile (#409). Only functional on builds with
    /// the `h264` feature — otherwise toast the build hint. The tile shares
    /// the preview-tile state machine (one tile per stream; opening video
    /// replaces an open preview tile and aborts its subscriber).
    #[cfg(feature = "h264")]
    fn open_parallax_video_tile(&mut self, stream: String) -> Task<Message> {
        use crate::view::specialized::parallax_h264;
        let Some(source) = self
            .selected_device
            .as_ref()
            .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
            .map(|d| d.device_id.source.clone())
        else {
            return Task::none();
        };
        if self.demo_mode {
            if let Some(device) = self.selected_device.as_mut() {
                let generation = device.parallax_detail.allocate_generation();
                device
                    .parallax_detail
                    .open_tile(&stream, generation, None, true);
            }
            return Task::none();
        }
        let Some(session) = self.session.clone() else {
            self.toasts
                .push(ToastSeverity::Error, "Not connected to Zenoh".to_string());
            return Task::none();
        };
        // Switching an open preview tile to video replaces the tile state
        // (aborting the preview subscriber), so the preview's earlier
        // `open_stream` must be balanced with a `close_stream` — otherwise
        // its refcount leaks on the sensor and the preview profile encodes
        // for nobody until the idle reaper. Sent BEFORE the h264 open, and
        // strictly ordered: `Task::chain` awaits the close put before the
        // open put, both ride the same cached declared publisher on the same
        // key (per-publisher order is preserved), and the sensor's command
        // loop feeds its actor mpsc in arrival order. At this point only the
        // preview profile is open, so the codec-less decrement is balanced.
        let was_open = self
            .selected_device
            .as_ref()
            .is_some_and(|d| d.parallax_detail.is_open(&stream));
        let Some(generation) = self
            .selected_device
            .as_mut()
            .map(|d| d.parallax_detail.allocate_generation())
        else {
            return Task::none();
        };
        // Media keys are origin-scoped; `*` covers the pre-map window.
        let media_origin = self.origin_for(&source).unwrap_or_else(|| "*".into());
        let (frames, handle) = Task::stream(parallax_h264::h264_tile_stream(
            session,
            media_origin,
            stream.clone(),
            generation,
        ))
        .abortable();
        if let Some(device) = self.selected_device.as_mut() {
            device.parallax_detail.open_tile(
                &stream,
                generation,
                Some(handle.abort_on_drop()),
                true,
            );
        }
        let cmd_key = self.parallax_stream_set_key(&source);
        let open =
            zensight_common::command::Command::new(zensight_common::StreamControl::OpenStream {
                stream: stream.clone(),
                codec: Some("h264".to_string()),
                max_height: None,
            });
        let mut send =
            self.send_command(cmd_key.clone(), &open, format!("Opened video for {stream}"));
        if was_open {
            let close = zensight_common::command::Command::new(
                zensight_common::StreamControl::CloseStream {
                    stream: stream.clone(),
                },
            );
            send = self
                .send_command(cmd_key, &close, format!("Closed preview for {stream}"))
                .chain(send);
        }
        Task::batch([send, frames])
    }

    /// Without the `h264` feature the video tile is a stub: explain how to
    /// get it instead of failing silently (#409).
    #[cfg(not(feature = "h264"))]
    fn open_parallax_video_tile(&mut self, _stream: String) -> Task<Message> {
        self.toasts.push(
            ToastSeverity::Info,
            crate::view::specialized::parallax_h264::UNAVAILABLE_HINT.to_string(),
        );
        Task::none()
    }

    /// Relay the H.264 tile decoder's discontinuity recovery: ask the sensor
    /// for a fresh IDR (`request_keyframe`) on the stream's command channel.
    fn request_parallax_keyframe(&mut self, stream: String) -> Task<Message> {
        let Some(source) = self
            .selected_device
            .as_ref()
            .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
            .map(|d| d.device_id.source.clone())
        else {
            return Task::none();
        };
        if self.demo_mode || self.command_registry.is_none() {
            return Task::none();
        }
        let request = zensight_common::command::Command::new(
            zensight_common::StreamControl::RequestKeyframe {
                stream: stream.clone(),
            },
        );
        // Quiet on success: resync-driven requests are automatic and can
        // recur (backed off in h264_tile_stream) — a toast per request is
        // pure noise. Failures still surface.
        self.send_command(
            self.parallax_stream_set_key(&source),
            &request,
            String::new(),
        )
    }

    /// Expand a tile into the near-fullscreen overlay (#436). A preview
    /// tile is upgraded to the H.264 video profile when the build carries
    /// the decoder and the stream advertises the codec — the same
    /// refcount-balanced switch as the catalogue's Video button. Demo mode
    /// just shows the (placeholder) frame large.
    fn expand_parallax_tile(&mut self, stream: String) -> Task<Message> {
        use crate::view::specialized::parallax_h264;
        let Some(device) = self
            .selected_device
            .as_mut()
            .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
        else {
            return Task::none();
        };
        let Some(needs_video) = device.parallax_detail.expand(&stream) else {
            return Task::none();
        };
        let advertises_h264 = device
            .parallax_detail
            .catalogue
            .ready()
            .is_some_and(|streams| {
                streams
                    .iter()
                    .any(|s| s.stream == stream && s.codecs.iter().any(|c| c == "h264"))
            });
        if needs_video && parallax_h264::AVAILABLE && advertises_h264 && !self.demo_mode {
            return self.open_parallax_video_tile(stream);
        }
        Task::none()
    }

    /// Dismiss the expanded-tile overlay (#436), switching the tile back to
    /// the preview profile when expand had upgraded it — the sensor-side
    /// refcounts return to their pre-expand state.
    fn collapse_parallax_tile(&mut self) -> Task<Message> {
        let Some(device) = self
            .selected_device
            .as_mut()
            .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
        else {
            return Task::none();
        };
        let Some(expanded) = device.parallax_detail.collapse() else {
            return Task::none();
        };
        let tile_is_video = device
            .parallax_detail
            .tiles
            .get(&expanded.stream)
            .is_some_and(|tile| tile.video);
        if !expanded.was_video && tile_is_video && !self.demo_mode {
            return self.open_parallax_tile(expanded.stream);
        }
        Task::none()
    }

    /// Abort every open parallax preview tile and batch the `close_stream`
    /// sends (#408). Called whenever the device view goes away — deselect,
    /// disconnect, session replacement. `abort_on_drop` already kills the
    /// subscribers (the sensor's crash backstop); the explicit close makes
    /// the sensor reap immediately instead of after the idle timeout.
    fn teardown_parallax_tiles(&mut self) -> Task<Message> {
        let Some(device) = self.selected_device.as_mut() else {
            return Task::none();
        };
        if device.device_id.protocol != zensight_common::Protocol::Parallax {
            return Task::none();
        }
        let source = device.device_id.source.clone();
        let streams = device.parallax_detail.teardown();
        if streams.is_empty() || self.demo_mode || self.command_registry.is_none() {
            return Task::none();
        }
        let cmd_key = self.parallax_stream_set_key(&source);
        Task::batch(streams.into_iter().map(|stream| {
            let close = zensight_common::command::Command::new(
                zensight_common::StreamControl::CloseStream {
                    stream: stream.clone(),
                },
            );
            self.send_command(
                cmd_key.clone(),
                &close,
                format!("Closed preview for {stream}"),
            )
        }))
    }

    /// Fetch the per-process bandwidth table from the netlink sensor's
    /// `@rpc/netlink/bandwidth` procedure (#319/epic #320). In demo mode (no session)
    /// return synthetic rows so the Processes view is developable without sensors
    /// — demo never serves queryables.
    fn query_bandwidth(&self) -> Task<Message> {
        if self.demo_mode {
            return Task::done(Message::BandwidthLoaded(Ok(
                crate::mock::bandwidth::processes(),
            )));
        }
        use crate::view::specialized::netlink_detail::fetch_records_all;
        // Two per-process tiers answer on their own protocol key: the netlink
        // sock_diag goodput tier and the netring wire-L2 attribution tier (#318).
        // Fetch BOTH (every host on the mesh, per #309) and merge — each row is a
        // `BandwidthRecord` self-tagged with its source/semantics badge, so the
        // table renders them side by side; host-scoping is applied on `apply`.
        let netlink_key = format!(
            "{}?by=process&top=100",
            zensight_common::bandwidth::bandwidth_query_key(zensight_common::Protocol::Netlink),
        );
        let netring_key = format!(
            "{}?top=100",
            zensight_common::bandwidth::bandwidth_query_key(zensight_common::Protocol::Netring),
        );
        let Some(session) = self.session.clone() else {
            return Task::done(Message::BandwidthLoaded(Err(
                "Not connected to Zenoh".to_string()
            )));
        };
        Task::future(async move {
            let netlink =
                fetch_records_all::<zensight_common::BandwidthRecord>(session.clone(), netlink_key)
                    .await;
            let netring =
                fetch_records_all::<zensight_common::BandwidthRecord>(session, netring_key).await;
            let result = match (netlink, netring) {
                (None, None) => Err("No netlink or netring sensor responded".to_string()),
                (a, b) => {
                    let mut rows = a.unwrap_or_default();
                    rows.extend(b.unwrap_or_default());
                    Ok(rows)
                }
            };
            Message::BandwidthLoaded(result)
        })
    }

    /// Every (origin, producer, host) we know is up, from the sensor-registration
    /// docs. The fleet view needs this to tell "not deployed" from "deployed but
    /// answering nothing" — a producer that is alive and silent on `introspect`
    /// is the row you most want to see, and fanning out alone cannot find it.
    fn alive_producers(&self) -> Vec<crate::view::fleet::AliveProducer> {
        let mut out: Vec<crate::view::fleet::AliveProducer> = self
            .known_sensors
            .values()
            .filter_map(|info| {
                let origin = info.host_id.clone()?;
                Some((origin, info.producer.clone(), info.source.clone()))
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Fan `introspect` out across the fleet (#469, RFC 08 §6).
    ///
    /// One GET per registered producer, `QueryTarget::All` so a `complete`
    /// queryable on one host cannot short-circuit the multi-host consolidation
    /// (RFC 05 §2.1). The origin comes from the *answering key* — a registry
    /// slice describes a build, not a deployment, so it does not name its host.
    ///
    /// `@catalog` is a service origin, and a verbatim `@` chunk is structurally
    /// unmatchable by the `*` in a fleet selector (design property D2). So it
    /// takes its own key rather than riding the fan-out — which is the grammar
    /// working, not an exception to it.
    fn query_fleet(&self) -> Task<Message> {
        use crate::view::fleet::FleetReply;

        if self.demo_mode {
            return Task::done(Message::FleetLoaded(Ok(crate::mock::fleet::replies())));
        }
        let Some(session) = self.session.clone() else {
            return Task::done(Message::FleetLoaded(Err(
                "Not connected to Zenoh".to_string()
            )));
        };

        let keys: Vec<(String, String)> = zensight_keyspace::registry::REGISTRIES
            .iter()
            .map(|(name, _)| {
                let key = if *name == "catalog" {
                    zensight_common::catalog_rpc_key("introspect")
                } else {
                    zensight_common::fleet_rpc_key(name, "introspect")
                };
                (name.to_string(), key)
            })
            .collect();

        Task::future(async move {
            let mut replies: Vec<FleetReply> = Vec::new();
            let mut errors = 0usize;
            for (producer, key) in keys {
                let Ok(stream) = session
                    .get(&key)
                    .target(zenoh::query::QueryTarget::All)
                    .timeout(std::time::Duration::from_secs(3))
                    .await
                else {
                    errors += 1;
                    continue;
                };
                while let Ok(reply) = stream.recv_async().await {
                    let Ok(sample) = reply.result() else { continue };
                    // The concrete key that answered carries the origin.
                    let Some(parsed) =
                        zensight_common::keyexpr::parse_wire_key(sample.key_expr().as_str())
                    else {
                        continue;
                    };
                    let Ok(toml) = String::from_utf8(sample.payload().to_bytes().to_vec()) else {
                        continue;
                    };
                    replies.push(FleetReply {
                        origin: parsed.origin.chunk().to_string(),
                        producer: producer.clone(),
                        toml,
                    });
                }
            }
            if replies.is_empty() && errors > 0 {
                return Message::FleetLoaded(Err(format!("{errors} introspect queries failed")));
            }
            Message::FleetLoaded(Ok(replies))
        })
    }

    /// Derive the per-service bandwidth rows (#319) from streamed systemd
    /// `unit/<name>/{ip_ingress_bps,ip_egress_bps}` telemetry, with per-unit
    /// sparkline history from the store. Ingress = rx, egress = tx.
    fn bandwidth_service_rows(&self) -> Vec<crate::view::bandwidth::BwRow> {
        use zensight_common::TelemetryValue;
        use zensight_common::bandwidth::{
            BandwidthKey, BandwidthRecord, BandwidthSource, ByteSemantics, ProtoScope,
        };
        let mut rows = Vec::new();
        for dev in self.dashboard.devices.values() {
            if dev.id.protocol != zensight_common::Protocol::Systemd {
                continue;
            }
            let host = dev.id.source.clone();
            // unit -> (tx = egress, rx = ingress)
            let mut units: std::collections::HashMap<String, (f64, f64)> =
                std::collections::HashMap::new();
            for (metric, point) in &dev.metrics {
                let v = if let TelemetryValue::Gauge(v) = &point.value {
                    *v
                } else {
                    continue;
                };
                // systemd `unit/{unit}/ip_{egress,ingress}_bps`, via the registry (#475).
                use zensight_keyspace::registry::systemd::Subject as SystemdSubject;
                match SystemdSubject::parse_metric(metric) {
                    Some(SystemdSubject::UnitIpEgressBps { unit }) => {
                        units.entry(unit).or_default().0 = v;
                    }
                    Some(SystemdSubject::UnitIpIngressBps { unit }) => {
                        units.entry(unit).or_default().1 = v;
                    }
                    _ => {}
                }
            }
            for (unit, (tx, rx)) in units {
                let spark = self
                    .store
                    .hot_samples(&format!("systemd/{host}|unit/{unit}/ip_ingress_bps"))
                    .into_iter()
                    .map(|s| s.value)
                    .collect();
                rows.push(crate::view::bandwidth::BwRow {
                    record: BandwidthRecord {
                        key: BandwidthKey::Service { unit },
                        tx_bps: tx,
                        rx_bps: rx,
                        source: BandwidthSource::Systemd,
                        semantics: ByteSemantics::WireL3,
                        proto: ProtoScope::All,
                        host: Some(host.clone()),
                    },
                    spark,
                });
            }
        }
        rows
    }

    /// Handle Escape key - close dialogs or go back.
    fn handle_escape(&mut self) -> Task<Message> {
        // Transient overlays close first, before any view navigation.
        if self.command_palette.open {
            self.command_palette.close();
            return Task::none();
        }
        if self.help_open {
            self.help_open = false;
            return Task::none();
        }
        // The global search overlay takes priority: Escape closes it first (#27).
        if self.global_search.open {
            self.global_search.close();
            return Task::none();
        }
        // The expanded parallax tile is an overlay too: Escape collapses it
        // (restoring the pre-expand profile) before leaving the device view.
        if self
            .selected_device
            .as_ref()
            .is_some_and(|d| d.parallax_detail.expanded.is_some())
        {
            return self.collapse_parallax_tile();
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
                        let teardown = self.teardown_parallax_tiles();
                        self.selected_device = None;
                        self.set_view(CurrentView::Dashboard);
                        return teardown;
                    }
                }
            }
            CurrentView::Expectations
            | CurrentView::Security
            | CurrentView::Sensors
            | CurrentView::Logs
            | CurrentView::Inventory
            | CurrentView::Bandwidth
            | CurrentView::Fleet
            | CurrentView::Incidents => {
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
        Task::none()
    }

    /// Create subscriptions for Zenoh telemetry and periodic updates.
    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = if self.demo_mode {
            // In demo mode, use mock data generator instead of Zenoh
            vec![
                demo_subscription(),
                tick_subscription(),
                keyboard_subscription(),
            ]
        } else {
            vec![
                zenoh_subscription(self.link.clone()),
                tick_subscription(),
                keyboard_subscription(),
            ]
        };
        // Flow-dash animation (#394): only while the map is open AND traffic
        // is actually flowing — an idle network burns no frames. 10 fps is
        // plenty for a dash march and an order cheaper than window::frames.
        if self.current_view == CurrentView::Topology && self.topology.has_animated_edges() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(100))
                    .map(|_| Message::TopologyAnimTick),
            );
        }
        // Layout animation (#441/#442): ~30 fps while the force simulation
        // is actually settling or a tiered-position tween is in flight —
        // self-terminating, the same gating pattern as the dash animation.
        // A settled graph burns no frames; the 1 Hz tick only refreshes data.
        let force_settling = self.topology.prefs.layout == crate::view::topology::LayoutMode::Force
            && self.topology.auto_layout
            && !self.topology.layout_stable;
        if self.current_view == CurrentView::Topology
            && (force_settling || self.topology.tween_active())
        {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(33))
                    .map(|_| Message::TopologyLayoutFrame),
            );
        }
        Subscription::batch(subs)
    }

    /// Render the view.
    pub fn view(&self) -> Element<'_, Message> {
        use iced::widget::{Stack, row};

        // Badge counts both unacknowledged rule alerts and active sensor-pushed
        // alerts (anomalies + expectation violations).
        let unack = self.alerts.unacknowledged_count + self.alerts.external_count();

        // Per-card sparkline previews (#24). Built at 1 Hz in `handle_tick` and
        // cached in `dashboard_sparks` — rendering just clones the small (≤2
        // metrics/device) result rather than rescanning the store every frame,
        // which is what pegged the UI thread under a high telemetry rate.
        let sparks = if self.on_dashboard_grid() {
            self.dashboard_sparks.clone()
        } else {
            crate::view::trend::DeviceSparks::new()
        };

        let main_view: Element<'_, Message> = match self.current_view {
            CurrentView::Settings => settings_view(&self.settings),
            CurrentView::Alerts => alerts_view(&self.alerts),
            CurrentView::Topology => {
                topology_view(&self.topology, &self.entities, &self.store, self.theme)
            }
            CurrentView::Expectations => {
                crate::view::expectations::expectations_view(&self.expectations)
            }
            CurrentView::Security => crate::view::security::security_view(
                &self.alerts,
                &self.security,
                &self.detection_tuning,
            ),
            CurrentView::Sensors => crate::view::sensors::sensors_view(
                &self.sensor_health,
                &self.recent_errors,
                &self.artifact_fetch,
                self.artifact_job.as_ref().map(|j| j.producer.as_str()),
                self.artifact_job.as_ref().map(|j| j.kind.slug()),
                &self.artifact_kinds,
                &self.capture_forms,
            ),
            CurrentView::Logs => {
                let logs: Vec<_> = self.recent_logs.iter().cloned().collect();
                crate::view::specialized::logs_view(&logs, &self.syslog_filter)
            }
            CurrentView::Inventory => {
                crate::view::inventory::inventory_view(&self.inventory, &self.entities, now_ms())
            }
            CurrentView::Bandwidth => crate::view::bandwidth::bandwidth_view(&self.bandwidth),
            CurrentView::Fleet => crate::view::fleet::fleet_view(&self.fleet),
            CurrentView::Incidents => {
                crate::view::incident::incidents_view(&self.alerts, &self.incidents)
            }
            CurrentView::Device => {
                if let Some(ref device_state) = self.selected_device {
                    // For a syslog device, hand the view this host's recent log
                    // stream from the rolling buffer (so it shows history, like
                    // the Logs tab). Cheap: the buffer is bounded.
                    let host = device_state.device_id.source.as_str();
                    let host_logs: Vec<_> = self
                        .recent_logs
                        .iter()
                        .filter(|m| m.host() == host)
                        .cloned()
                        .collect();
                    // #133/#306: gather this physical host's sensor facets so the
                    // detail renders them as tabs — the protocol is a facet of a
                    // host, not a top-level axis. When a correlator entity claims
                    // the device, the facets span every member source (union),
                    // else they fall back to the same-source facets.
                    let entity = self.entities.entity_for_device(&device_state.device_id);
                    let member_ids: Option<std::collections::HashSet<DeviceId>> = entity.map(|e| {
                        e.members
                            .iter()
                            .filter_map(crate::entity::member_device_id)
                            .collect()
                    });
                    let mut facet_states: Vec<&DeviceState> = self
                        .dashboard
                        .devices
                        .values()
                        .filter(|d| match &member_ids {
                            Some(ids) => ids.contains(&d.id),
                            None => d.id.source == device_state.device_id.source,
                        })
                        .collect();
                    facet_states.sort_by_key(|d| {
                        (
                            crate::view::host::protocol_priority(d.id.protocol),
                            d.id.protocol,
                        )
                    });
                    let mut facets: Vec<crate::view::device::FacetTab> = facet_states
                        .iter()
                        .map(|d| crate::view::device::FacetTab {
                            id: d.id.clone(),
                            protocol: d.id.protocol,
                            status: d.effective_status(),
                            active: d.id == device_state.device_id,
                        })
                        .collect();
                    // Members with no live DeviceState still show as disabled tabs
                    // (union), so the host's full sensor set is visible.
                    if let Some(ids) = &member_ids {
                        let live: std::collections::HashSet<&DeviceId> =
                            facet_states.iter().map(|d| &d.id).collect();
                        let mut missing: Vec<&DeviceId> =
                            ids.iter().filter(|id| !live.contains(id)).collect();
                        missing.sort_by_key(|id| {
                            (
                                crate::view::host::protocol_priority(id.protocol),
                                id.protocol,
                            )
                        });
                        for id in missing {
                            facets.push(crate::view::device::FacetTab {
                                id: id.clone(),
                                protocol: id.protocol,
                                status: zensight_common::DeviceStatus::Unknown,
                                active: false,
                            });
                        }
                    }
                    crate::view::device::host_detail_view(crate::view::device::DeviceViewCtx {
                        state: device_state,
                        syslog_filter: &self.syslog_filter,
                        host_logs: &host_logs,
                        facets: &facets,
                        entity,
                        identity_expanded: self.identity_expanded,
                        artifact: Some(crate::view::artifact_fetch::ArtifactCtx {
                            fetch: &self.artifact_fetch,
                            kinds: &self.artifact_kinds,
                            capture_forms: &self.capture_forms,
                            active_prefix: self.artifact_job.as_ref().map(|j| j.producer.as_str()),
                            active_kind: self.artifact_job.as_ref().map(|j| j.kind.slug()),
                        }),
                    })
                } else {
                    dashboard_view(
                        &self.dashboard,
                        self.theme,
                        unack,
                        &self.groups,
                        &self.overview,
                        &self.sensor_health,
                        sparks,
                        &self.entities,
                        &self.firing_by_source,
                        self.settings.group_by_host,
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
                &self.entities,
                &self.firing_by_source,
                self.settings.group_by_host,
            ),
        };

        // Wrap the page in the persistent shell (left nav rail + top bar with
        // breadcrumb, alert badge, and connection status visible on every screen).
        let device_name = self
            .selected_device
            .as_ref()
            .filter(|_| self.current_view == CurrentView::Device)
            .map(|d| d.device_id.source.as_str());
        // Focus mode (#476): show the *hostname* if we can map the origin back
        // to one, else the origin id itself — never nothing, because the banner
        // is what explains why the rest of the fleet vanished.
        let focused_host = self.link.focus.as_ref().map(|origin| {
            self.origins
                .iter()
                .find(|(_, v)| *v == origin)
                .map(|(source, _)| source.clone())
                .unwrap_or_else(|| origin.clone())
        });

        let shelled = crate::view::shell::app_shell(
            self.current_view,
            device_name,
            self.dashboard.connection_state,
            unack,
            self.last_telemetry_ms,
            now_ms(),
            focused_host,
            main_view,
        );

        let view_container: Element<'_, Message> = container(shelled)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        // Show groups panel as a sidebar if open
        let base_view: Element<'_, Message> = if self.groups.panel_open {
            row![view_container, groups_panel(&self.groups)].into()
        } else {
            view_container
        };

        // All overlays live in a single, always-present `Stack` with `base_view`
        // permanently at layer 0. This keeps the root widget's identity stable
        // across frames: previously the root flipped between `Container` and
        // `Stack` (and base_view was re-wrapped) as toasts/palette/help/search
        // came and went, so Iced saw a different node type at the root, rebuilt
        // the entire widget-state tree, and reset every scrollable's offset —
        // i.e. an alert toast popping in scrolled the page back to the top.
        // Keeping base_view at index 0 lets Iced reconcile (not rebuild) its
        // subtree, so scroll position survives. (#alerts-scroll)
        let mut layers: Vec<Element<'_, Message>> = vec![base_view];

        // Expanded parallax tile (#436): a near-fullscreen live view over the
        // device view. Renders only while the device view shows a parallax
        // source with an expanded tile — every teardown choke point clears
        // the tile state (and the expansion with it).
        if self.current_view == CurrentView::Device
            && let Some(device) = self
                .selected_device
                .as_ref()
                .filter(|d| d.device_id.protocol == zensight_common::Protocol::Parallax)
            && let Some(overlay) =
                crate::view::specialized::parallax::expanded_overlay(&device.parallax_detail)
        {
            layers.push(overlay);
        }

        // Global metric search overlay (#27), centered over the current view.
        if self.global_search.open {
            let hits = crate::view::search::search(
                self.dashboard.devices.values(),
                &self.global_search.query,
            );
            // Entity hostname/IP results + the passive-DNS naming pivot (#314).
            let entity_hits = crate::view::search::search_entities(
                &self.entities,
                &self.global_search.query,
                now_ms(),
            );
            let ip_offer =
                crate::view::search::ip_lookup_offer(&self.entities, &self.global_search.query);
            layers.push(
                container(crate::view::search::global_search_panel(
                    &self.global_search,
                    hits,
                    entity_hits,
                    ip_offer,
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            );
        }

        // Command palette overlay (#28), centered over the current view.
        if self.command_palette.open {
            let filtered = crate::view::palette::filter(&self.command_palette.query);
            layers.push(
                container(crate::view::palette::command_palette_panel(
                    &self.command_palette,
                    &filtered,
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            );
        }

        // Keyboard-shortcuts help overlay (#28), centered over the current view.
        if self.help_open {
            layers.push(
                container(crate::view::help::help_overlay())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .into(),
            );
        }

        // Toast notifications, bottom-right. ALWAYS pushed as the top layer
        // (it renders nothing when there are no toasts) so adding or removing a
        // toast never changes the root topology — see the note above.
        layers.push(
            container(toast_overlay(&self.toasts))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_right(Length::Shrink)
                .align_bottom(Length::Shrink)
                .padding(20)
                .into(),
        );

        Stack::with_children(layers).into()
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
        // Parse protocol from string. Use the canonical FromStr impl so newer
        // sensors (netlink/netring) aren't silently dropped — the hand-rolled
        // match here only covered the legacy protocols (#125).
        let Ok(protocol) = protocol_str.parse::<Protocol>() else {
            return; // Unknown protocol, ignore
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

    /// Apply a sensor-level liveliness transition to the health cards.
    ///
    /// Liveliness tokens carry `(protocol, source)`; `sensor_health` is keyed
    /// `sensor@source`, and the sensor name is the protocol segment, so the
    /// two line up. Legacy (non-host-scoped) tokens have no source and match
    /// every instance of the protocol — the legacy token shape can't
    /// distinguish hosts anyway.
    fn set_sensor_liveliness(&mut self, protocol: &str, source: Option<&str>, alive: bool) {
        for (key, snap) in self.sensor_health.iter_mut() {
            if !sensor_liveliness_matches(key, protocol, source) {
                continue;
            }
            if alive {
                // Only lift an Offline badge; a live sensor's real status
                // comes from its own HealthSnapshots.
                if snap.status == HealthStatus::Offline {
                    tracing::info!(sensor = %key, "Sensor card lifted Offline → Starting (liveliness token reappeared)");
                    snap.status = HealthStatus::Starting;
                }
            } else {
                tracing::info!(sensor = %key, was = %snap.status, "Sensor card marked Offline (liveliness token gone)");
                snap.status = HealthStatus::Offline;
            }
        }
    }

    /// Merge cold-store search-back results (#107, C9) into the rolling log
    /// buffer via the shared de-dup merge below.
    fn merge_log_history(&mut self, logs: Vec<crate::store::StoredLog>) {
        let msgs = logs
            .into_iter()
            .map(|log| {
                let point = log.to_point();
                crate::view::specialized::syslog_message_from_point(&point, &point.source)
            })
            .collect();
        self.merge_log_messages(msgs);
    }

    /// Shared log-buffer merge (#107/#358): drop rows already present (by
    /// time+message — also de-dups within the incoming batch, so overlapping
    /// `since=` fetch windows are idempotent), then keep the newest
    /// [`MAX_RECENT_LOGS`] across the union, time-ordered.
    fn merge_log_messages(&mut self, msgs: Vec<crate::view::specialized::SyslogMessage>) {
        if msgs.is_empty() {
            return;
        }
        use std::collections::HashSet;
        let mut seen: HashSet<(i64, String)> = self
            .recent_logs
            .iter()
            .map(|m| (m.timestamp(), m.message().to_string()))
            .collect();
        let mut merged: Vec<crate::view::specialized::SyslogMessage> = Vec::new();
        for msg in msgs {
            if seen.insert((msg.timestamp(), msg.message().to_string())) {
                merged.push(msg);
            }
        }
        if merged.is_empty() {
            return;
        }
        merged.extend(self.recent_logs.drain(..));
        merged.sort_by_key(|m| m.timestamp());
        let start = merged.len().saturating_sub(MAX_RECENT_LOGS);
        self.recent_logs = merged.split_off(start).into();
    }

    /// Whether the periodic log-events refresh should fire this tick (#358):
    /// connected, not demo, no fetch in flight, a logs surface is actually on
    /// screen, and the cadence gap has elapsed.
    fn should_refresh_logs(&self, now_ms: i64) -> bool {
        // (`query_log_events` itself degrades gracefully with no session, so
        // `connected` is the only liveness gate needed here.)
        if self.demo_mode || !self.dashboard.connected || self.log_fetch_inflight {
            return false;
        }
        let viewing_logs = self.current_view == CurrentView::Logs
            || (self.current_view == CurrentView::Device
                && self
                    .selected_device
                    .as_ref()
                    .is_some_and(|d| d.device_id.protocol == Protocol::Logs));
        if !viewing_logs {
            return false;
        }
        self.last_log_fetch_ms
            .is_none_or(|t| now_ms - t >= LOG_REFRESH_SECS * 1000)
    }

    /// Fire an incremental `@rpc/logs/events` fetch when due (#358). The `since=`
    /// selector trails the newest-seen event by [`LOG_FETCH_OVERLAP_MS`] so
    /// clock skew never opens a gap; the merge de-dup absorbs the overlap.
    fn maybe_refresh_logs(&mut self) -> Option<Task<Message>> {
        let now = now_ms();
        if !self.should_refresh_logs(now) {
            return None;
        }
        self.log_fetch_inflight = true;
        self.last_log_fetch_ms = Some(now);
        let since = self.last_log_event_ms.map(|t| t - LOG_FETCH_OVERLAP_MS);
        Some(self.query_log_events(since))
    }

    /// One `@rpc/logs/events` GET (#358): fans out to every logs sensor's
    /// queryable, drains ALL replies (one per sensor — never first-reply-wins),
    /// and concatenates the decoded records.
    fn query_log_events(&self, since: Option<i64>) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::done(Message::LogEventsLoaded(Err(
                "Not connected to Zenoh".to_string()
            )));
        };
        // NB: zenoh selector parameters are `;`-separated (`Parameters`), not
        // `&` — the server reads them via `query.parameters().get(..)`.
        // Fleet fan-in: every logs sensor answers, so target All (RFC 05 §2.1).
        let mut selector = format!(
            "{}?max={LOG_FETCH_MAX}",
            zensight_common::fleet_rpc_key("logs", "events")
        );
        if let Some(since) = since {
            selector.push_str(&format!(";since={since}"));
        }
        Task::future(async move {
            match session
                .get(&selector)
                .target(zenoh::query::QueryTarget::All)
                .timeout(std::time::Duration::from_secs(3))
                .await
            {
                Ok(replies) => {
                    let mut records: Vec<zensight_common::LogRecord> = Vec::new();
                    while let Ok(reply) = replies.recv_async().await {
                        if let Ok(sample) = reply.result()
                            && let Ok(mut batch) =
                                zensight_common::decode_auto::<Vec<zensight_common::LogRecord>>(
                                    &sample.payload().to_bytes(),
                                )
                        {
                            records.append(&mut batch);
                        }
                    }
                    Message::LogEventsLoaded(Ok(records))
                }
                Err(e) => Message::LogEventsLoaded(Err(e.to_string())),
            }
        })
    }

    /// Handle incoming telemetry.
    fn handle_telemetry(&mut self, point: TelemetryPoint) {
        // Write through to the local tiered store (O(1) hot-ring append; numeric
        // values only). Charts/trends read back from here so history survives restart.
        self.store.record(&point);

        // Keep the bandwidth monitor's Services table live while it is open: a
        // systemd `ip_*_bps` point changes the derived rows (#319). Recomputed at
        // the tail, after this point has landed in the device-state map.
        let bw_services_relevant = self.current_view == CurrentView::Bandwidth
            && self.bandwidth.mode == crate::view::bandwidth::BandwidthMode::Services
            && point.protocol == Protocol::Systemd
            && (point.metric.ends_with("/ip_ingress_bps")
                || point.metric.ends_with("/ip_egress_bps"));

        // Track the newest point for the global freshness verdict (#23).
        self.last_telemetry_ms = Some(
            self.last_telemetry_ms
                .map_or(point.timestamp, |prev| prev.max(point.timestamp)),
        );

        // Syslog/journald lines feed the rolling buffer behind the Logs view.
        // Unlike per-metric device state (which keeps only the latest point per
        // facility/severity), this preserves the full recent stream.
        //
        // Since #358 current sensors serve per-line events from `@rpc/logs/events`
        // instead of streaming them, so live lines normally arrive via the
        // periodic fetch (`LogEventsLoaded`). This ingest branch stays for demo
        // mode (the mock stream) and pre-#358 sensors on the wire.
        //
        // Only actual per-line log events (a Text payload) belong here. The logs
        // sensor also streams derived rollup telemetry (`logs/by_severity/*`,
        // `logs/ingest/*`, `logs/by_unit/*` — counters/gauges) on the same
        // `Protocol::Logs`; those are real metrics for the per-device derived
        // cards but must not masquerade as log lines, or they render as
        // `Counter(N)` junk and evict real messages from the bounded buffer. This
        // mirrors the cold-store guard in `StoredLog::from_point` (Text-only).
        if point_is_log_line(&point) {
            self.recent_logs
                .push_back(crate::view::specialized::syslog_message_from_point(
                    &point,
                    &point.source,
                ));
            while self.recent_logs.len() > MAX_RECENT_LOGS {
                self.recent_logs.pop_front();
            }
            // Persist to the cold store (#107, C9) — template-aware sampling
            // decides what survives restart for search-back. Only per-line
            // events carry a uid; rollup/derived points (no uid) are skipped.
            if let Some(log) = crate::store::StoredLog::from_point(&point) {
                self.store.record_log(log);
            }
        }

        let device_id = DeviceId::from_telemetry(&point);

        // Update dashboard device state
        let device_state = self
            .dashboard
            .devices
            .entry(device_id.clone())
            .or_insert_with(|| DeviceState::new(device_id.clone()));

        device_state.last_update = point.timestamp;
        device_state.is_healthy = true;
        // Per-line log events (#104) use unique `events/<uid>` metrics — keeping
        // the latest point per metric would grow the device map without bound (one
        // entry per log line). They live in `recent_logs` instead; here we only
        // refresh liveness. All other telemetry keeps last-value-per-metric.
        let is_log_event = point.protocol == zensight_common::Protocol::Logs
            && point.metric.starts_with("events/");
        if !is_log_event {
            device_state
                .metrics
                .insert(point.metric.clone(), point.clone());
        }
        device_state.metric_count = device_state.metrics.len();

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

        // Update selected device if this telemetry is for it. Per-line log events
        // are excluded for the same cardinality reason as above (#104).
        if let Some(ref mut selected) = self.selected_device
            && selected.device_id == device_id
            && !is_log_event
        {
            selected.update(point);
        }

        // Update topology if we're viewing it
        if self.current_view == CurrentView::Topology {
            self.refresh_topology_nodes();
        }

        // Recompute the bandwidth Services table now that the point has landed.
        if bw_services_relevant {
            let rows = self.bandwidth_service_rows();
            self.bandwidth.set_services(rows);
        }
    }

    /// Select a device to view in detail. Returns a task that pre-loads this
    /// device's restart-survived history from the local store off the UI thread
    /// (#22), so the detail chart opens pre-populated with persisted trends.
    /// Project the firing external anomalies scoped to the selected netring
    /// device's source into its detail state, so the Security tab + Overview
    /// anomaly strip render without threading `AlertsState` through the view
    /// (#253). No-op unless a netring device is open.
    fn refresh_netring_anomalies(&mut self) {
        use zensight_common::{AlertKind, Protocol};
        let Some(source) = self
            .selected_device
            .as_ref()
            .filter(|d| d.device_id.protocol == Protocol::Netring)
            .map(|d| d.device_id.source.clone())
        else {
            return;
        };
        let anomalies: Vec<zensight_common::Alert> = self
            .alerts
            .active_external()
            .into_iter()
            .filter(|a| a.kind == AlertKind::Anomaly && a.source == source)
            .cloned()
            .collect();
        if let Some(device) = self.selected_device.as_mut() {
            device.netring_detail.anomalies = anomalies;
        }
    }

    fn select_device(&mut self, device_id: DeviceId) -> Task<Message> {
        tracing::info!(device = %device_id, "Selected device");
        // Replacing the selection replaces its parallax tile state: close any
        // live tiles FIRST so their `close_stream`s are actually sent (this
        // is the choke point for SelectDevice / SelectAdjacentDevice /
        // InvestigateAlert, which all land here).
        let teardown = self.teardown_parallax_tiles();
        // We don't have the full TelemetryPoints in the dashboard,
        // so the detail view will populate as new data arrives
        let max_history = self.settings.max_history_value();
        let mut detail_state = DeviceDetailState::with_max_history(device_id.clone(), max_history);
        // Project this device's favorited metrics (#27) from the global set.
        detail_state.set_favorites(self.device_favorites(&device_id));
        // Focus state (#476): the origin may not be known yet (the map is fed by
        // health/registration/entity docs), in which case the Focus control
        // renders disabled — see `refresh_focus_state`.
        detail_state.origin = self.origin_for(&device_id.source);
        detail_state.focused =
            detail_state.origin.is_some() && detail_state.origin == self.link.focus;
        self.selected_device = Some(detail_state);
        self.set_view(CurrentView::Device);
        // Project firing anomalies for this source into the netring view (#253).
        self.refresh_netring_anomalies();

        // Prefetch this protocol's primary detail channels so the drill-in opens
        // pre-populated rather than Idle-until-clicked (#127).
        let mut prefetch = self.prefetch_on_open(&device_id);

        // Contextual capture (#351): a netring drill-down hosts the on-demand
        // capture form, which needs the sensor's advertised artifact kinds.
        // Lazily discover them (and seed the shared form) if the Sensors page
        // hasn't already.
        if device_id.protocol == Protocol::Netring
            && !self.artifact_kinds.contains_key(Protocol::Netring.as_str())
            && let Some(task) = self.load_artifact_kinds()
        {
            prefetch = Task::batch([prefetch, task]);
        }

        // Resolve the persisted metric ids for this device, then query the warm
        // (minute) tier off-thread. Last 24h of minute buckets is plenty to
        // pre-populate a chart without blocking the UI.
        let history = 'history: {
            let Some(store) = self.store.persistent() else {
                break 'history Task::none();
            };
            let protocol = device_id.protocol.to_string();
            let metric_ids = self.store.device_metric_ids(&protocol, &device_id.source);
            if metric_ids.is_empty() {
                break 'history Task::none();
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
        };

        Task::batch([teardown, history, prefetch])
    }

    /// Range-query the store for an absolute `[from_ms, to_ms]` window (#36) and
    /// seed the open chart with it, so an operator can pull up an exact past slice
    /// (e.g. "14:05–14:12 yesterday") even when it's no longer in the hot ring.
    /// Mirrors the on-open 24h load but with a caller-chosen window.
    fn load_device_history_range(
        &self,
        device_id: DeviceId,
        from_ms: i64,
        to_ms: i64,
    ) -> Task<Message> {
        let Some(store) = self.store.persistent() else {
            return Task::none();
        };
        let protocol = device_id.protocol.to_string();
        let metric_ids = self.store.device_metric_ids(&protocol, &device_id.source);
        if metric_ids.is_empty() {
            return Task::none();
        }
        Task::future(async move {
            let series = tokio::task::spawn_blocking(move || {
                metric_ids
                    .into_iter()
                    .filter_map(|(name, id)| {
                        store
                            .query(id, crate::store::Tier::Minute, from_ms, to_ms)
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

    /// Prefetch the primary on-demand detail channels for a device's protocol so
    /// the specialized view opens pre-populated (#127). Declarative policy keyed
    /// by protocol; reuses the existing `Fetch*` message flow (which marks the
    /// `Fetch<T>` slot `Loading` and issues the query), so there is no duplicated
    /// fetch logic here. No-op when disconnected or for protocols without
    /// queryable detail channels.
    fn prefetch_on_open(&self, device_id: &DeviceId) -> Task<Message> {
        if self.session.is_none() {
            return Task::none();
        }
        Task::batch(
            prefetch_channels(device_id.protocol)
                .into_iter()
                .map(Task::done),
        )
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

        // Update the link config. The live subscription is keyed on this whole
        // value (`Subscription::run_with(link, …)`), so changing connection,
        // subscription scope, or link profile makes Iced tear down the current
        // session and reconnect with the new settings — no restart needed. We
        // surface that to the user instead of doing it silently (#38).
        let new_link = crate::subscription::LinkConfig {
            zenoh: ZenohConfig {
                mode: self.settings.zenoh_mode.as_str().to_string(),
                connect: self.settings.connect_endpoints(),
                listen: self.settings.listen_endpoints(),
                scouting: true,
            },
            scope: self.settings.scope_entries(),
            profile: self.settings.link_profile,
            // A settings edit does not clear focus.
            focus: self.link.focus.clone(),
        };
        let connection_changed = self.link != new_link;
        self.link = new_link;

        if connection_changed && !self.demo_mode {
            // Reflect the impending reconnect immediately; the restarted
            // subscription will drive Connecting → Connected/Disconnected.
            self.dashboard.connection_state = crate::view::dashboard::ConnectionState::Connecting;
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
        persistent.alert_filter_presets = self.alerts.alert_filter_presets.clone();
        persistent.favorite_metrics = self.favorites.iter().cloned().collect();
        persistent.overview_selected_protocol = self.overview.selected_protocol;
        persistent.overview_expanded = self.overview.expanded;
        persistent.topology_lens = self.topology.prefs.lens;
        persistent.topology_grouping = self.topology.prefs.grouping;
        persistent.topology_edge_label = self.topology.prefs.edge_label;
        persistent.topology_filters = self.topology.prefs.filters;
        persistent.topology_layout = self.topology.prefs.layout;
        let (pins, positions) = self.topology.pinned_positions();
        persistent.topology_pinned = pins;
        persistent.topology_positions = positions;
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

    /// Export the current device's CSV via a native save dialog (#37). Returns
    /// `None` when no device is selected (nothing to export).
    fn export_to_csv(&mut self) -> Option<Task<Message>> {
        let device = self.selected_device.as_ref()?;
        // Prefer the full time series (the trend on screen, #37); fall back to
        // the latest-value snapshot only when no history exists yet.
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
        Some(export_dialog(filename, csv))
    }

    /// Export the current device's JSON via a native save dialog (#37).
    fn export_to_json(&mut self) -> Option<Task<Message>> {
        let device = self.selected_device.as_ref()?;
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
        Some(export_dialog(filename, json))
    }

    /// Handle periodic tick (update health status, etc.).
    /// Whether the current view renders the device-card grid (which needs the
    /// sparkline previews). Dashboard, or the Device route with no device open.
    fn on_dashboard_grid(&self) -> bool {
        matches!(
            self.current_view,
            CurrentView::Dashboard | CurrentView::Device
        ) && !(self.current_view == CurrentView::Device && self.selected_device.is_some())
    }

    fn handle_tick(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        for device in self.dashboard.devices.values_mut() {
            device.update_health(now, self.stale_threshold_ms);
        }

        // Rebuild the dashboard-card sparklines at 1 Hz (only when a card grid is
        // actually showing), so the per-frame render just clones the cached result
        // instead of rescanning the store on every redraw (startup-freeze fix).
        if self.on_dashboard_grid() {
            self.dashboard_sparks = crate::view::trend::build_device_sparks(
                &self.store,
                self.dashboard.devices.keys(),
                2,
            );
        } else if !self.dashboard_sparks.is_empty() {
            self.dashboard_sparks.clear();
        }

        // Bound the device map over long sessions: reap devices gone for a day
        // (#40). Logged so the drop is never silent.
        let evicted = self
            .dashboard
            .evict_stale_devices(now, crate::view::dashboard::DEVICE_EVICTION_AGE_MS);
        if evicted > 0 {
            tracing::info!(evicted, "Evicted stale devices from dashboard");
        }

        // Expire alert silences whose window has passed (#26).
        self.alerts.prune_silences(now);

        // Rebuild the per-source firing-alert rollup for host cards (#306).
        self.firing_by_source.clear();
        for alert in self.alerts.active_external() {
            *self
                .firing_by_source
                .entry(alert.source.clone())
                .or_insert(0) += 1;
        }

        // Apply debounced search filter
        self.dashboard.apply_pending_search();

        // Update chart time for selected device
        if let Some(ref mut device) = self.selected_device {
            device.update_chart_time();
        }

        // Clean up expired toasts
        self.toasts.cleanup_expired();

        // Update topology data when viewing it. Layout stepping moved to the
        // gated ~30 fps `TopologyLayoutFrame` subscription (#441) — at 1 Hz
        // the force simulation settled in visible once-a-second lurches.
        if self.current_view == CurrentView::Topology {
            self.refresh_topology_nodes();
        }

        // Land any debounced topology-pref changes (#440): at most one
        // settings.json5 write per second, off the interaction path.
        self.flush_topology_prefs();
    }
}

/// The per-instance key for `sensor_health`/`recent_errors`: `sensor@source`
/// (bare `sensor` for legacy snapshots without a source). Deliberately matches
/// the `known_sensors` `<name>@<source>` convention so the two maps line up.
pub(crate) fn sensor_instance_key(sensor: &str, source: Option<&str>) -> String {
    match source {
        Some(s) => format!("{sensor}@{s}"),
        None => sensor.to_string(),
    }
}

/// Whether a `sensor_health` instance key belongs to the sensor a liveliness
/// token identifies. Host-scoped tokens carry a `<source>` and match exactly
/// one instance; legacy tokens carry none and match every instance of the
/// protocol (the legacy key shape can't distinguish hosts).
fn sensor_liveliness_matches(key: &str, protocol: &str, source: Option<&str>) -> bool {
    match source {
        Some(_) => key == sensor_instance_key(protocol, source),
        None => {
            key == protocol || (key.starts_with(protocol) && key[protocol.len()..].starts_with('@'))
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

/// Whether a telemetry point is an actual per-line log event (so it belongs in
/// the Logs view's rolling buffer), as opposed to the logs sensor's derived
/// rollup telemetry. Log lines carry a `Text` payload; rollups (`logs/by_*`,
/// `logs/ingest/*`, …) are counters/gauges. Pure, and the unit of testing for
/// the Logs-buffer admission policy — keeping rollups out so they don't render
/// as `Counter(N)` junk and evict real messages. Mirrors `StoredLog::from_point`.
fn point_is_log_line(point: &TelemetryPoint) -> bool {
    point.protocol == zensight_common::Protocol::Logs
        && matches!(point.value, TelemetryValue::Text(_))
}

/// The primary on-demand detail channels to prefetch when a device of this
/// protocol is opened (#127), as the `Fetch*` messages that drive them. Pure
/// (the unit of testing for the prefetch policy); empty for protocols whose
/// detail is fully streamed (no queryable channels) or has no specialized view.
fn prefetch_channels(protocol: zensight_common::Protocol) -> Vec<Message> {
    use crate::view::specialized::netlink_detail::NetlinkDetailTopic;
    use crate::view::specialized::sysinfo_detail::ProcessSort;
    use zensight_common::Protocol;

    match protocol {
        Protocol::Netlink => vec![
            Message::FetchNetlinkDetail(NetlinkDetailTopic::Sockets),
            Message::FetchNetlinkDetail(NetlinkDetailTopic::Routes),
            Message::FetchNetlinkDetail(NetlinkDetailTopic::Neighbors),
            // Pre-populate the default-route flap history (#111) so it's visible
            // on open, not behind an extra click.
            Message::FetchNetlinkDetail(NetlinkDetailTopic::RouteChanges),
        ],
        Protocol::Netring => vec![Message::FetchNetringFlows],
        Protocol::Sysinfo => vec![Message::FetchSysinfoProcesses(ProcessSort::default())],
        Protocol::Parallax => vec![Message::FetchParallaxStreams],
        _ => Vec::new(),
    }
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

/// The `protocol/source/` prefix under which a device's favorited metrics (#27)
/// are keyed in the global favorites set.
fn fav_prefix(device_id: &DeviceId) -> String {
    format!("{}/{}/", device_id.protocol, device_id.source)
}

/// The global favorites key for `metric` on `device_id` (#27).
fn fav_key(device_id: &DeviceId, metric: &str) -> String {
    format!("{}{}", fav_prefix(device_id), metric)
}

/// Fire a best-effort desktop notification for a CRITICAL alert (#26). Runs on a
/// detached thread because `notify-rust`'s `show()` does blocking D-Bus I/O;
/// errors are swallowed — a missing notification daemon must never disturb the UI.
fn notify_critical(summary: String) {
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary("ZenSight — Critical alert")
            .body(&summary)
            .timeout(notify_rust::Timeout::Milliseconds(10_000))
            .show();
    });
}

/// Open a native "save as" dialog (#37) seeded with `default_name`, write
/// `contents` to the chosen path, and resolve to an [`Message::ExportFinished`]
/// outcome. Defaults to the user's Downloads directory so files land somewhere
/// discoverable instead of the process CWD. Cancelling the dialog is a no-op.
fn export_dialog(default_name: String, contents: String) -> Task<Message> {
    Task::future(async move {
        let mut dialog = rfd::AsyncFileDialog::new().set_file_name(&default_name);
        if let Some(dir) = dirs::download_dir().or_else(dirs::home_dir) {
            dialog = dialog.set_directory(dir);
        }
        let Some(handle) = dialog.save_file().await else {
            return Message::ExportFinished(Ok(None));
        };
        let path = handle.path().to_path_buf();
        match tokio::fs::write(&path, contents).await {
            Ok(()) => Message::ExportFinished(Ok(Some(path.display().to_string()))),
            Err(e) => Message::ExportFinished(Err(e.to_string())),
        }
    })
}

/// Native "Save as…" for a downloaded debug report. Unlike [`export_dialog`],
/// this **moves** an already-verified temp file to the chosen path (falling back
/// to a streamed copy across filesystems), so a large blob is never read into
/// RAM (memo R2). Cancelling the dialog discards the temp file.
fn save_blob_dialog(
    default_name: String,
    src: std::path::PathBuf,
    decompress_on_save: bool,
) -> Task<Message> {
    Task::future(async move {
        let mut dialog = rfd::AsyncFileDialog::new().set_file_name(&default_name);
        if let Some(dir) = dirs::download_dir().or_else(dirs::home_dir) {
            dialog = dialog.set_directory(dir);
        }
        let Some(handle) = dialog.save_file().await else {
            // Cancelled: discard the temp artifact.
            let _ = tokio::fs::remove_file(&src).await;
            return Message::ArtifactSaved(Ok(None));
        };
        let dst = handle.path().to_path_buf();
        // Rename first (same filesystem); fall back to a streamed copy on EXDEV.
        let result = match tokio::fs::rename(&src, &dst).await {
            Ok(()) => Ok(dst.display().to_string()),
            Err(_) => match tokio::fs::copy(&src, &dst).await {
                Ok(_) => {
                    let _ = tokio::fs::remove_file(&src).await;
                    Ok(dst.display().to_string())
                }
                Err(e) => Err(e.to_string()),
            },
        };

        // Decompress a saved `.pcap.zst` capture back to a `.pcap` sibling (#333).
        let result = match result {
            Ok(saved) if decompress_on_save && saved.ends_with(".pcap.zst") => {
                let zst = std::path::PathBuf::from(&saved);
                let pcap = zst.with_extension(""); // strip trailing `.zst`
                let decoded = tokio::task::spawn_blocking(move || {
                    let reader = std::io::BufReader::new(std::fs::File::open(&zst)?);
                    let writer = std::fs::File::create(&pcap)?;
                    zstd::stream::copy_decode(reader, writer)?;
                    let _ = std::fs::remove_file(&zst);
                    Ok::<_, std::io::Error>(pcap.display().to_string())
                })
                .await;
                match decoded {
                    Ok(Ok(pcap_path)) => Ok(pcap_path),
                    // Decompression failed: keep the saved .zst, report it.
                    Ok(Err(_)) | Err(_) => Ok(saved),
                }
            }
            other => other,
        };
        Message::ArtifactSaved(result.map(Some))
    })
}

#[cfg(test)]
mod prefetch_tests {
    use super::*;
    use crate::view::specialized::netlink_detail::NetlinkDetailTopic;
    use zensight_common::Protocol;

    #[test]
    fn prefetch_policy_by_protocol() {
        // Netlink prefetches its primary host tables (sockets/routes/neighbors)
        // plus the default-route flap history (#111).
        let nl = prefetch_channels(Protocol::Netlink);
        assert_eq!(nl.len(), 4);
        assert!(matches!(
            nl[0],
            Message::FetchNetlinkDetail(NetlinkDetailTopic::Sockets)
        ));
        assert!(matches!(
            nl[3],
            Message::FetchNetlinkDetail(NetlinkDetailTopic::RouteChanges)
        ));

        // Netring prefetches flows; sysinfo prefetches the process explorer.
        assert!(matches!(
            prefetch_channels(Protocol::Netring).as_slice(),
            [Message::FetchNetringFlows]
        ));
        assert!(matches!(
            prefetch_channels(Protocol::Sysinfo).as_slice(),
            [Message::FetchSysinfoProcesses(_)]
        ));

        // Parallax prefetches the stream catalogue (#408).
        assert!(matches!(
            prefetch_channels(Protocol::Parallax).as_slice(),
            [Message::FetchParallaxStreams]
        ));

        // Protocols without queryable detail channels prefetch nothing.
        assert!(prefetch_channels(Protocol::Snmp).is_empty());
        assert!(prefetch_channels(Protocol::Logs).is_empty());
        assert!(prefetch_channels(Protocol::Modbus).is_empty());
    }

    /// Regression: only per-line Text log events feed the Logs buffer — the logs
    /// sensor's derived rollup counters/gauges (`logs/by_severity/*`,
    /// `logs/ingest/*`, …) stream on the same `Protocol::Logs` but must not be
    /// admitted, or they render as `Counter(N)` junk and evict real messages.
    #[test]
    fn only_text_log_events_feed_the_buffer() {
        let line = TelemetryPoint::new(
            "host01",
            Protocol::Logs,
            "events/0000000000000000000000001",
            TelemetryValue::Text("INTRUDER ALERT from 10.0.0.9".to_string()),
        );
        assert!(point_is_log_line(&line));

        // Derived rollups (counters/gauges) are excluded.
        for (metric, value) in [
            ("logs/by_severity/error_total", TelemetryValue::Counter(3)),
            ("logs/ingest/received_total", TelemetryValue::Counter(6)),
            ("logs/units_in_failure", TelemetryValue::Gauge(0.0)),
        ] {
            let rollup = TelemetryPoint::new("host01", Protocol::Logs, metric, value);
            assert!(
                !point_is_log_line(&rollup),
                "{metric} must not be a log line"
            );
        }

        // Non-Logs telemetry is never a log line, even when Text.
        let snmp_text = TelemetryPoint::new(
            "router01",
            Protocol::Snmp,
            "system/sysDescr",
            TelemetryValue::Text("Cisco IOS".to_string()),
        );
        assert!(!point_is_log_line(&snmp_text));
    }
}

/// #358: per-line log events are pulled from `@rpc/logs/events`, not streamed.
/// These tests pin the fetch gating, the watermark, and the overlap de-dup.
#[cfg(test)]
mod log_fetch_tests {
    use super::*;

    fn app() -> ZenSight {
        ZenSight::boot(true).0
    }

    fn rec(uid: &str, ts: i64, message: &str) -> zensight_common::LogRecord {
        zensight_common::LogRecord {
            uid: uid.to_string(),
            ts,
            host: "web01".to_string(),
            facility: "daemon".to_string(),
            severity: "err".to_string(),
            severity_number: 17,
            app: None,
            pid: None,
            message: message.to_string(),
            labels: Default::default(),
        }
    }

    #[test]
    fn refresh_gating() {
        let mut a = app();
        let now = 1_000_000_000_000;
        // Demo mode never fetches (mock stream feeds the buffer directly).
        assert!(!a.should_refresh_logs(now));

        a.demo_mode = false;
        a.dashboard.connected = true;
        // Not on a logs surface → no fetch.
        a.current_view = CurrentView::Dashboard;
        assert!(!a.should_refresh_logs(now));
        // Logs view + first fetch (no cadence history) → fetch.
        a.current_view = CurrentView::Logs;
        assert!(a.should_refresh_logs(now));
        // Within the cadence window → no fetch.
        a.last_log_fetch_ms = Some(now - (LOG_REFRESH_SECS * 1000 - 1));
        assert!(!a.should_refresh_logs(now));
        // Past the window → fetch again.
        a.last_log_fetch_ms = Some(now - LOG_REFRESH_SECS * 1000);
        assert!(a.should_refresh_logs(now));
        // …unless one is already in flight.
        a.log_fetch_inflight = true;
        assert!(!a.should_refresh_logs(now));
    }

    #[test]
    fn loaded_events_advance_watermark_and_merge() {
        let mut a = app();
        a.log_fetch_inflight = true;
        let _ = a.update(Message::LogEventsLoaded(Ok(vec![
            rec("u1", 1000, "first"),
            rec("u2", 2000, "second"),
        ])));
        assert!(!a.log_fetch_inflight, "fetch completion clears the flag");
        assert_eq!(a.last_log_event_ms, Some(2000));
        assert_eq!(a.recent_logs.len(), 2);

        // An overlapping refetch (same records + one new) merges without dupes.
        a.log_fetch_inflight = true;
        let _ = a.update(Message::LogEventsLoaded(Ok(vec![
            rec("u2", 2000, "second"),
            rec("u3", 3000, "third"),
        ])));
        assert_eq!(a.last_log_event_ms, Some(3000));
        assert_eq!(a.recent_logs.len(), 3, "overlap de-dups by (ts, message)");
        // Time-ordered after merge.
        let ts: Vec<i64> = a.recent_logs.iter().map(|m| m.timestamp()).collect();
        assert_eq!(ts, vec![1000, 2000, 3000]);
    }

    #[test]
    fn fetch_error_clears_inflight() {
        let mut a = app();
        a.log_fetch_inflight = true;
        let _ = a.update(Message::LogEventsLoaded(Err("timeout".to_string())));
        assert!(!a.log_fetch_inflight);
        assert!(a.recent_logs.is_empty());
    }
}

/// #132: the decomposed `update()` routes each message to exactly one per-domain
/// `update_*` handler (claiming it → `Break`) or hands it back (`Continue`) so the
/// chain — and ultimately the main `match` — can handle it. These tests pin that
/// contract so a future handler can't silently swallow a foreign message.
#[cfg(test)]
mod update_routing_tests {
    use super::*;

    fn app() -> ZenSight {
        // Demo mode boots without Zenoh or disk-backed history.
        ZenSight::boot(true).0
    }

    #[test]
    fn handler_claims_its_own_domain() {
        let mut a = app();
        // Chart interactions are owned by update_chart even with no device open.
        assert!(matches!(
            a.update_chart(Message::ChartZoomIn),
            ControlFlow::Break(_)
        ));
        // Syslog panel toggle is owned by update_syslog.
        assert!(matches!(
            a.update_syslog(Message::ToggleSyslogFilterPanel),
            ControlFlow::Break(_)
        ));
        // A detail filter is owned by update_detail.
        assert!(matches!(
            a.update_detail(Message::SetNetlinkSocketPortFilter("80".into())),
            ControlFlow::Break(_)
        ));
    }

    #[test]
    fn handler_passes_back_foreign_messages() {
        let mut a = app();
        // None of these handlers own ToggleTheme — each must hand it back so a
        // later stage (here, the main match) gets a chance.
        assert!(matches!(
            a.update_chart(Message::ToggleTheme),
            ControlFlow::Continue(_)
        ));
        assert!(matches!(
            a.update_detail(Message::ToggleTheme),
            ControlFlow::Continue(_)
        ));
        assert!(matches!(
            a.update_topology_msg(Message::ToggleTheme),
            ControlFlow::Continue(_)
        ));
    }

    #[test]
    fn update_falls_through_to_main_match() {
        let mut a = app();
        // ToggleTheme is owned by the main match, past all five handlers; routing
        // must reach it and flip the theme.
        let was_dark = matches!(a.theme, AppTheme::Dark);
        let _ = a.update(Message::ToggleTheme);
        assert_ne!(was_dark, matches!(a.theme, AppTheme::Dark));
    }
}

#[cfg(test)]
mod sensor_liveliness_tests {
    use super::*;

    fn app() -> ZenSight {
        ZenSight::boot(true).0
    }

    fn snapshot(sensor: &str, source: Option<&str>, status: HealthStatus) -> HealthSnapshot {
        HealthSnapshot {
            sensor: sensor.to_string(),
            status,
            uptime_secs: 60,
            devices_total: 1,
            devices_responding: 1,
            devices_failed: 0,
            last_poll_duration_ms: 10,
            errors_last_hour: 0,
            metrics_published: 100,
            host_id: None,
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn liveliness_match_host_scoped_is_exact() {
        assert!(sensor_liveliness_matches(
            "netlink@hostA",
            "netlink",
            Some("hostA")
        ));
        assert!(!sensor_liveliness_matches(
            "netlink@hostB",
            "netlink",
            Some("hostA")
        ));
        assert!(!sensor_liveliness_matches(
            "netring@hostA",
            "netlink",
            Some("hostA")
        ));
    }

    #[test]
    fn liveliness_match_legacy_covers_protocol_not_lookalikes() {
        // A legacy token (no source) matches the bare key and every host
        // instance of that protocol...
        assert!(sensor_liveliness_matches("snmp", "snmp", None));
        assert!(sensor_liveliness_matches("snmp@h1", "snmp", None));
        // ...but not protocols that merely share a prefix.
        assert!(!sensor_liveliness_matches("snmpx@h1", "snmp", None));
        assert!(!sensor_liveliness_matches("sysinfo@h1", "snmp", None));
    }

    #[test]
    fn sensor_offline_marks_its_health_card_offline() {
        let mut a = app();
        let _ = a.update(Message::HealthSnapshotReceived(snapshot(
            "netlink",
            Some("hostA"),
            HealthStatus::Healthy,
        )));
        let _ = a.update(Message::HealthSnapshotReceived(snapshot(
            "netlink",
            Some("hostB"),
            HealthStatus::Healthy,
        )));

        let _ = a.update(Message::SensorOffline(
            "netlink".into(),
            Some("hostA".into()),
        ));

        // Only hostA's card flips; hostB's instance is untouched.
        assert_eq!(
            a.sensor_health["netlink@hostA"].status,
            HealthStatus::Offline
        );
        assert_eq!(
            a.sensor_health["netlink@hostB"].status,
            HealthStatus::Healthy
        );
    }

    #[test]
    fn sensor_online_lifts_offline_but_never_overrides_real_health() {
        let mut a = app();
        let _ = a.update(Message::HealthSnapshotReceived(snapshot(
            "sysinfo",
            Some("hostA"),
            HealthStatus::Degraded,
        )));
        let _ = a.update(Message::HealthSnapshotReceived(snapshot(
            "sysinfo",
            Some("hostB"),
            HealthStatus::Offline,
        )));

        let _ = a.update(Message::SensorOnline(
            "sysinfo".into(),
            Some("hostA".into()),
        ));
        let _ = a.update(Message::SensorOnline(
            "sysinfo".into(),
            Some("hostB".into()),
        ));

        // A live sensor's real status comes from its own snapshots — Degraded
        // stays Degraded; only an Offline badge is lifted (to Starting, until
        // the next snapshot arrives).
        assert_eq!(
            a.sensor_health["sysinfo@hostA"].status,
            HealthStatus::Degraded
        );
        assert_eq!(
            a.sensor_health["sysinfo@hostB"].status,
            HealthStatus::Starting
        );
    }
}

/// Forget-device (#stale facets): dropping a facet removes its map entry, and
/// forgetting the open device reuses the back-to-dashboard path.
#[cfg(test)]
mod origin_map_tests {
    use super::*;
    use zensight_common::Protocol;

    /// The drill-down fetches key off the v1 origin, which the wire payloads
    /// don't carry in `source` (hostnames) — the map bridges via the health/
    /// registration/entity docs' `host_id` (== origin id, RFC 06 §1).
    #[test]
    fn health_snapshot_populates_the_origin_map() {
        let mut a = ZenSight::boot(true).0;
        assert_eq!(a.origin_for("hostA"), None);

        let snapshot = zensight_common::HealthSnapshot {
            sensor: "sysinfo".into(),
            status: zensight_common::HealthStatus::Healthy,
            uptime_secs: 1,
            devices_total: 0,
            devices_responding: 0,
            devices_failed: 0,
            last_poll_duration_ms: 0,
            errors_last_hour: 0,
            metrics_published: 1,
            host_id: Some("h-3fa9c2d41b7e".into()),
            source: Some("hostA".into()),
        };
        let _ = a.update(Message::HealthSnapshotReceived(snapshot));
        assert_eq!(a.origin_for("hostA").as_deref(), Some("h-3fa9c2d41b7e"));
    }

    #[test]
    fn entity_members_populate_the_origin_map() {
        let mut a = ZenSight::boot(true).0;
        let entity = zensight_common::HostEntity {
            entity_id: "h-3fa9c2d41b7e".into(),
            aliases: vec![],
            host_id: Some("h-3fa9c2d41b7e".into()),
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: Some("hostA".into()),
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            members: vec![zensight_common::MemberClaim {
                sensor: "netring".into(),
                source: "hostA".into(),
                rule: "host_id".into(),
                confidence: 1.0,
                last_seen: 1,
            }],
            status: None,
            last_updated: 1,
        };
        let _ = a.update(Message::EntityReceived(entity));
        assert_eq!(a.origin_for("hostA").as_deref(), Some("h-3fa9c2d41b7e"));
    }

    /// selected_origin_for: mapped origin when the selected device's source is
    /// known; None (→ fleet fallback) otherwise.
    #[test]
    fn selected_origin_resolves_through_the_map() {
        let mut a = ZenSight::boot(true).0;
        let id = DeviceId::new(Protocol::Netring, "hostA");
        a.selected_device = Some(DeviceDetailState::new(id));
        assert_eq!(a.selected_origin_for(Protocol::Netring), None);

        a.origins.insert("hostA".into(), "h-3fa9c2d41b7e".into());
        assert_eq!(
            a.selected_origin_for(Protocol::Netring).as_deref(),
            Some("h-3fa9c2d41b7e")
        );
        // Wrong protocol → no origin (fleet fallback).
        assert_eq!(a.selected_origin_for(Protocol::Sysinfo), None);
    }
}

#[cfg(test)]
mod forget_device_tests {
    use super::*;
    use zensight_common::Protocol;

    #[test]
    fn forget_device_removes_map_entry() {
        let mut a = ZenSight::boot(true).0;
        let id = DeviceId::new(Protocol::Snmp, "router01");
        a.dashboard
            .devices
            .insert(id.clone(), DeviceState::new(id.clone()));

        let _ = a.update(Message::ForgetDevice(id.clone()));
        assert!(!a.dashboard.devices.contains_key(&id));
    }

    #[test]
    fn forget_selected_device_clears_selection() {
        let mut a = ZenSight::boot(true).0;
        let id = DeviceId::new(Protocol::Snmp, "router01");
        a.dashboard
            .devices
            .insert(id.clone(), DeviceState::new(id.clone()));
        a.selected_device = Some(DeviceDetailState::new(id.clone()));
        a.current_view = CurrentView::Device;

        let _ = a.update(Message::ForgetDevice(id.clone()));
        assert!(!a.dashboard.devices.contains_key(&id));
        assert!(a.selected_device.is_none(), "selection cleared");
        assert!(matches!(a.current_view, CurrentView::Dashboard));

        // Forgetting some *other* device must not touch the open selection.
        let open = DeviceId::new(Protocol::Sysinfo, "server01");
        let gone = DeviceId::new(Protocol::Sysinfo, "toolbx");
        a.dashboard
            .devices
            .insert(open.clone(), DeviceState::new(open.clone()));
        a.dashboard
            .devices
            .insert(gone.clone(), DeviceState::new(gone.clone()));
        a.selected_device = Some(DeviceDetailState::new(open.clone()));
        let _ = a.update(Message::ForgetDevice(gone.clone()));
        assert!(!a.dashboard.devices.contains_key(&gone));
        assert!(a.dashboard.devices.contains_key(&open));
        assert!(a.selected_device.is_some());
    }
}
