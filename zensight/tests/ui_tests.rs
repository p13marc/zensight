//! UI tests using iced_test Simulator.
//!
//! These tests verify the UI behavior without needing actual Zenoh connections
//! or hardware sensors.

// Test fixtures build state stepwise (`let mut s = State::default(); s.field = ..`),
// which reads more clearly here than a single large struct literal.
#![allow(clippy::field_reassign_with_default)]

use iced_test::simulator;

// Re-export view components for testing
use zensight::app::{AppTheme, CurrentView};
use zensight::message::{DeviceId, Message};
use zensight::mock;
use zensight::view::dashboard::{ConnectionState, DashboardState, DeviceState, dashboard_view};
use zensight::view::device::{
    DeviceDetailState, DeviceViewCtx, FacetTab, device_view_with_syslog_filter, host_detail_view,
};
use zensight::view::groups::GroupsState;
use zensight::view::overview::OverviewState;
use zensight::view::settings::{SettingsState, settings_view};
use zensight::view::specialized::SyslogFilterState;
use zensight::view::topology::{TopologyState, topology_view};

/// Render the topology view with empty panel context (#393).
fn topo_view(state: &TopologyState, theme: AppTheme) -> iced::Element<'_, Message> {
    thread_local! {
        static ENTITIES: std::cell::OnceCell<&'static zensight::entity::EntityStore> =
            const { std::cell::OnceCell::new() };
        static STORE: std::cell::OnceCell<&'static zensight::store::MetricStore> =
            const { std::cell::OnceCell::new() };
    }
    let entities = ENTITIES
        .with(|c| *c.get_or_init(|| Box::leak(Box::new(zensight::entity::EntityStore::default()))));
    let store = STORE.with(|c| {
        *c.get_or_init(|| {
            Box::leak(Box::new(zensight::store::MetricStore::new(
                zensight::store::DEFAULT_HOT_CAPACITY,
                None,
            )))
        })
    });
    topology_view(state, entities, store, theme)
}

use std::collections::HashMap;
use zensight_common::Protocol;

/// Test that the dashboard view renders correctly with no devices.
#[test]
fn test_dashboard_empty() {
    let state = DashboardState::default();
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // Should show "Waiting for telemetry data..." message
    assert!(ui.find("Waiting for telemetry data...").is_ok());
}

/// Test that the dashboard shows devices when populated.
#[test]
fn test_dashboard_with_devices() {
    let mut state = DashboardState::default();
    state.connected = true;
    state.connection_state = ConnectionState::Connected;

    // Add mock devices
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut device = DeviceState::new(device_id.clone());
    device.metric_count = 5;
    device.is_healthy = true;
    state.devices.insert(device_id, device);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // Should show the device name
    assert!(ui.find("router01").is_ok());
    // Should show metric count
    assert!(ui.find("5 metrics").is_ok());
    // (Connection status now lives in the app shell, not the dashboard view.)
}

/// #130: a Degraded host surfaces in the worst-first health overview, and the
/// overview chip selects that device. Also covers the per-card health badge.
#[test]
fn test_dashboard_health_overview_surfaces_worst_host() {
    use zensight_common::DeviceStatus;

    let mut state = DashboardState::default();
    state.connected = true;
    state.connection_state = ConnectionState::Connected;

    let degraded_id = DeviceId::fixture(Protocol::Sysinfo, "host-sad".to_string());
    let mut degraded = DeviceState::new(degraded_id.clone());
    degraded.update_from_liveness(DeviceStatus::Degraded, 2, Some("flapping".into()));
    state.devices.insert(degraded_id.clone(), degraded);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // The worst-first overview banner appears with the unhealthy host.
    assert!(ui.find("Worst hosts (1)").is_ok());
    // Clicking the overview chip (host name · score) selects the device.
    let _ = ui.click("host-sad · 60");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::SelectDevice(id) if id.source == "host-sad"))
    );
}

/// A device card renders its trend-badge + sparkline strip when sparks are
/// provided. The badge text ("+50.0%") is searchable in the simulator (#24).
#[test]
fn test_dashboard_card_shows_trend_badge() {
    use zensight::store::Sample;
    use zensight::view::trend::{self, DeviceSparks, MetricSpark};

    let mut state = DashboardState::default();
    state.connected = true;
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut device = DeviceState::new(device_id.clone());
    device.metric_count = 1;
    device.is_healthy = true;
    state.devices.insert(device_id.clone(), device);

    // A rising series: 100 -> 150 == +50%.
    let samples = vec![
        Sample {
            ts: 0,
            value: 100.0,
        },
        Sample {
            ts: 1,
            value: 150.0,
        },
    ];
    let spark = MetricSpark {
        metric: "cpu/usage".to_string(),
        values: samples.iter().map(|s| s.value).collect(),
        trend: trend::compute(&samples),
    };
    let mut sparks = DeviceSparks::new();
    sparks.insert(device_id, vec![spark]);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        sparks,
        &entities,
        &firing,
        true,
    ));

    assert!(ui.find("server01").is_ok());
    assert!(ui.find("cpu/usage").is_ok());
    // Trend badge: up arrow + signed percent.
    assert!(ui.find("\u{2191} +50.0%").is_ok());
}

/// The global search panel renders matching results and a Close button (#27).
#[test]
fn test_global_search_panel_results() {
    use zensight::view::search::{self, GlobalSearchState, SearchHit};

    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut device = DeviceState::new(device_id.clone());
    device.metrics.insert(
        "queue/depth".to_string(),
        zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "router01".to_string(),
            protocol: Protocol::Snmp,
            metric: "queue/depth".to_string(),
            value: zensight_common::TelemetryValue::Gauge(7.0),
            labels: HashMap::new(),
            unit: None,
        },
    );

    let mut state = GlobalSearchState::default();
    state.open();
    state.query = "queue".to_string();
    let hits: Vec<SearchHit> = search::search([&device].into_iter(), &state.query);
    assert_eq!(hits.len(), 1);

    let mut ui = simulator(search::global_search_panel(&state, hits, Vec::new(), None));
    assert!(ui.find("Global Metric Search").is_ok());
    assert!(ui.find("Close").is_ok());
    assert!(ui.find("1 result(s)").is_ok());
}

/// Render the persistent app shell around a dummy page, for nav-rail tests.
fn shell_ui() -> iced_test::Simulator<'static, Message> {
    let content = iced::widget::text("content").into();
    simulator(zensight::view::shell::app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        Some(10_000),
        12_000,
        None,
        content,
    ))
}

/// The shell top bar shows the global freshness verdict. Connected with a
/// recent point reads "Live"; disconnected reads "Paused".
#[test]
fn test_shell_shows_freshness_live() {
    let content = iced::widget::text("content").into();
    let mut ui = simulator(zensight::view::shell::app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        Some(10_000),
        12_000, // 2s after last point => Live
        None,
        content,
    ));
    assert!(ui.find("Live").is_ok());
}

#[test]
fn test_shell_shows_freshness_paused() {
    let content = iced::widget::text("content").into();
    let mut ui = simulator(zensight::view::shell::app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Disconnected,
        0,
        None,
        12_000,
        None,
        content,
    ));
    assert!(ui.find("Paused").is_ok());
}

/// The shell top bar's "?" button toggles the keyboard-shortcuts help (#28).
#[test]
fn test_shell_help_button() {
    let mut ui = shell_ui();
    let _ = ui.click("?");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::ToggleHelp)));
}

/// The help overlay lists shortcuts and offers a Close action (#28).
#[test]
fn test_help_overlay_lists_shortcuts() {
    let mut ui = simulator(zensight::view::help::help_overlay());
    assert!(ui.find("Keyboard Shortcuts").is_ok());
    assert!(ui.find("Search metrics across all devices").is_ok());
    let _ = ui.click("Close");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::ToggleHelp)));
}

/// The command palette renders its commands and dispatches the chosen one (#28).
#[test]
fn test_command_palette_runs_command() {
    use zensight::view::palette::{self, CommandPaletteState};

    let mut state = CommandPaletteState::default();
    state.open();
    let filtered = palette::filter(&state.query);
    let mut ui = simulator(palette::command_palette_panel(&state, &filtered));

    assert!(ui.find("Command Palette").is_ok());
    assert!(ui.find("Go to Alerts").is_ok());

    // Clicking a command dispatches RunPaletteCommand with its filtered index.
    let _ = ui.click("Go to Alerts");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::RunPaletteCommand(_)))
    );
}

/// The nav rail's Settings button emits OpenSettings.
#[test]
fn test_shell_settings_button() {
    let mut ui = shell_ui();
    let _ = ui.click("Settings");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));
}

/// The nav rail's Alerts button emits OpenAlerts.
#[test]
fn test_shell_alerts_button() {
    let mut ui = shell_ui();
    let _ = ui.click("Alerts");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::OpenAlerts)));
}

/// The shell shows the connection status (here: Connected) on every screen.
#[test]
fn test_shell_shows_connection_status() {
    let ui = shell_ui();
    let mut ui = ui;
    assert!(ui.find("Connected").is_ok());
    // #133: the dashboard nav entry is host-centric ("Hosts").
    assert!(ui.find("Hosts").is_ok());
}

/// Test device detail view with mock data.
#[test]
fn test_device_detail_view() {
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add mock telemetry
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Should show the device name
    assert!(ui.find("server01").is_ok());
    // Should show Back button
    assert!(ui.find("Back").is_ok());
    // Should show section headers (specialized view shows CPU, Memory, etc.)
    assert!(ui.find("CPU").is_ok());
    assert!(ui.find("Memory").is_ok());
}

/// #476: the host detail header offers "Focus this host" once the host's origin
/// is known, and clicking it drives `SetFocusHost(Some(origin))`.
#[test]
fn test_device_detail_focus_button() {
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    state.origin = Some("h-3fa9c2d41b7e".to_string());

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    let _ = ui.click("Focus this host");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::SetFocusHost(Some(o)) if o == "h-3fa9c2d41b7e")),
        "clicking Focus should request focus on this host's origin"
    );
}

/// #476: while focused the same button becomes the way out — and the shell says
/// so, because an emptied fleet dashboard otherwise looks like an outage.
#[test]
fn test_focus_mode_offers_a_way_out() {
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    state.origin = Some("h-3fa9c2d41b7e".to_string());
    state.focused = true;

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    let _ = ui.click("Exit focus");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::SetFocusHost(None)))
    );

    // The shell banner names the focused host and carries its own exit.
    let content = iced::widget::text("content").into();
    let mut shell = simulator(zensight::view::shell::app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        Some(10_000),
        12_000,
        Some("server01".to_string()),
        content,
    ));
    assert!(shell.find("Focused on server01").is_ok());
    let _ = shell.click("Exit focus");
    let msgs: Vec<Message> = shell.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::SetFocusHost(None)))
    );
}

/// #133: a multi-sensor host renders one facet tab per sensor, and clicking an
/// inactive facet switches to it (`SelectDevice`). The protocol is a facet of the
/// host, not a top-level axis.
#[test]
fn test_host_detail_facet_tabs() {
    use zensight_common::DeviceStatus;

    let active = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut state = DeviceDetailState::new(active.clone());
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }

    let netlink_id = DeviceId::fixture(Protocol::Netlink, "server01".to_string());
    let facets = vec![
        FacetTab::live(active.clone(), DeviceStatus::Online, true),
        FacetTab::live(netlink_id.clone(), DeviceStatus::Degraded, false),
    ];

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &state,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: None,
        identity_expanded: false,
        artifact: None,
    }));

    // Both sensor facets are shown as tabs.
    assert!(ui.find("Facets").is_ok());
    assert!(ui.find("sysinfo").is_ok());
    assert!(ui.find("netlink").is_ok());

    // Clicking the inactive netlink facet switches to it.
    let _ = ui.click("netlink");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::SelectDevice(id) if *id == netlink_id))
    );
}

/// #133: a single-sensor host shows no facet strip (nothing to switch between).
#[test]
fn test_host_detail_single_facet_has_no_strip() {
    use zensight_common::DeviceStatus;

    let id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut state = DeviceDetailState::new(id.clone());
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }
    let facets = vec![FacetTab::live(id, DeviceStatus::Online, true)];

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &state,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: None,
        identity_expanded: false,
        artifact: None,
    }));

    // No "Facets" strip for a lone sensor; the detail still renders.
    assert!(ui.find("Facets").is_err());
    assert!(ui.find("server01").is_ok());
}

/// Two same-protocol facets with different sources (e.g. a toolbox-run sensor
/// correlated into the same host as the host-run one) get a muted "· <source>"
/// suffix on their host-card chips; a lone facet per protocol does not.
#[test]
fn test_host_card_disambiguates_same_protocol_facets() {
    use zensight_common::{HostEntity, MemberClaim};

    fn member(sensor: &str, source: &str) -> MemberClaim {
        MemberClaim {
            sensor: sensor.into(),
            source: source.into(),
            rule: "host_id".into(),
            confidence: 1.0,
            last_seen: 1,
        }
    }

    fn dashboard(sources: &[&str]) -> (DashboardState, zensight::entity::EntityStore) {
        let mut state = DashboardState::default();
        state.connected = true;
        state.connection_state = ConnectionState::Connected;
        for source in sources {
            let id = DeviceId::fixture(Protocol::Sysinfo, *source);
            let mut device = DeviceState::new(id.clone());
            device.metric_count = 3;
            device.is_healthy = true;
            state.devices.insert(id, device);
        }
        let mut entities = zensight::entity::EntityStore::default();
        entities.upsert(HostEntity {
            entity_id: "h_web01".into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: Some("web-01".into()),
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            members: sources.iter().map(|s| member("sysinfo", s)).collect(),
            status: None,
            last_updated: 1_000,
        });
        (state, entities)
    }

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    // Duplicated protocol → both chips carry their source suffix.
    let (state, entities) = dashboard(&["host-a", "toolbx"]);
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));
    assert!(ui.find("· toolbx").is_ok());
    assert!(ui.find("· host-a").is_ok());

    // Single facet for the protocol → no suffix.
    let (state, entities) = dashboard(&["toolbx"]);
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));
    assert!(ui.find("· toolbx").is_err());
}

/// The "Forget" affordance renders only for an Offline facet, and clicking it
/// emits `ForgetDevice` for that facet. (The map-entry removal itself is
/// asserted update-level in `app::forget_device_tests`.) Also pins the facet
/// tab strip's "· <source>" disambiguation for duplicated protocols.
#[test]
fn test_forget_button_only_for_offline_facet() {
    use zensight_common::DeviceStatus;

    fn view_for(status: DeviceStatus) -> (DeviceId, Vec<FacetTab>, DeviceDetailState) {
        let active = DeviceId::fixture(Protocol::Sysinfo, "toolbx");
        let other = DeviceId::fixture(Protocol::Sysinfo, "host-a");
        let facets = vec![
            FacetTab::live(active.clone(), status, true),
            FacetTab::live(other, DeviceStatus::Online, false),
        ];
        let state = DeviceDetailState::new(active.clone());
        (active, facets, state)
    }

    let syslog_filter = SyslogFilterState::default();

    // Offline active facet → Forget is present and emits ForgetDevice.
    let (active, facets, state) = view_for(DeviceStatus::Offline);
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &state,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: None,
        identity_expanded: false,
        artifact: None,
    }));
    assert!(ui.find("Forget").is_ok());
    // Duplicated-protocol tabs carry their source suffix.
    assert!(ui.find("· toolbx").is_ok());
    let _ = ui.click("Forget");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ForgetDevice(id) if *id == active)),
        "expected ForgetDevice for the active facet, got {messages:?}"
    );

    // Online active facet → no Forget affordance.
    let (_, facets, state) = view_for(DeviceStatus::Online);
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &state,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: None,
        identity_expanded: false,
        artifact: None,
    }));
    assert!(ui.find("Forget").is_err());
}

/// Test clicking Back button in device view.
#[test]
fn test_device_back_button() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let state = DeviceDetailState::new(device_id);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Click Back button
    let _ = ui.click("Back");

    // Should have produced ClearSelection message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ClearSelection))
    );
}

/// #35: clicking "View" on an alert row jumps to the offending device + metric.
#[test]
fn test_alert_investigate_navigates_to_device_metric() {
    use zensight::view::alerts::{Alert, AlertRule, AlertsState, Severity, alerts_view};

    let mut state = AlertsState::new();
    let rule = AlertRule::new(1, "High CPU", "cpu/usage").with_severity(Severity::Critical);
    let device = DeviceId::fixture(Protocol::Sysinfo, "server01");
    state.alerts.push(Alert::new(
        1,
        &rule,
        device.clone(),
        "cpu/usage".into(),
        95.0,
        0,
    ));

    let mut ui = simulator(alerts_view(&state));
    let _ = ui.click("View");

    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::InvestigateAlert { source, metric: Some(metric), .. }
                if source == "server01" && metric == "cpu/usage"
        )),
        "expected InvestigateAlert for server01/cpu/usage, got {messages:?}"
    );
}

/// #50: a metric row's "alert" button emits PromoteMetricToAlert with the
/// metric path and current value.
#[test]
fn test_metric_promote_to_alert() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01");
    let mut state = DeviceDetailState::new(device_id);
    let mut p = zensight_common::TelemetryPoint {
        timestamp: 0,
        source: "server01".to_string(),
        protocol: Protocol::Sysinfo,
        metric: "cpu/usage".to_string(),
        value: TelemetryValue::Gauge(91.0),
        labels: HashMap::new(),
        unit: None,
    };
    state.update(p.clone());
    p.metric = "memory/used".to_string();
    state.update(p);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    let _ = ui.click("alert");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            Message::PromoteMetricToAlert { metric, value, .. }
                if !metric.is_empty() && *value > 0.0
        )),
        "alert button should emit PromoteMetricToAlert, got {msgs:?}"
    );
}

/// #47: sysinfo renders PSI, cgroup, and system-health cards when the host
/// publishes those metric families.
#[test]
fn test_sysinfo_depth_cards() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: f64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "server01".to_string(),
            protocol: Protocol::Sysinfo,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(v),
            labels: HashMap::new(),
            unit: None,
        });
    };
    put("pressure/cpu/some_avg10", 12.5);
    put("cgroup/memory/used_percent", 80.0);
    put("system/file_descriptors_used_percent", 42.0);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    assert!(ui.find("Pressure (PSI)").is_ok());
    assert!(ui.find("cgroup").is_ok());
    assert!(ui.find("System health").is_ok());
}

/// #46: netlink renders the TC/qdisc panel from streamed tc/* metrics.
#[test]
fn test_netlink_tc_panel() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: u64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "gw01".to_string(),
            protocol: Protocol::Netlink,
            metric: metric.to_string(),
            value: TelemetryValue::Counter(v),
            labels: HashMap::new(),
            unit: None,
        });
    };
    put("tc/eth0/fq_codel/drops", 42);
    put("tc/eth0/fq_codel/overlimits", 7);

    // TC now lives under the QoS / Queues tab as per-qdisc cards (#258/#263).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Qos;
    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    assert!(ui.find("eth0 / fq_codel").is_ok());
    assert!(ui.find("Qdisc / class tree").is_ok());
}

/// #46: netlink renders the IPsec/xfrm card and RTT-percentile socket lines.
#[test]
fn test_netlink_depth_cards() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: f64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "gw01".to_string(),
            protocol: Protocol::Netlink,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(v),
            labels: HashMap::new(),
            unit: None,
        });
    };
    put("sockets/tcp/established", 10.0);
    put("sockets/tcp/rtt_p95_us", 1234.0);
    put("xfrm/sa/total", 4.0);

    let syslog_filter = SyslogFilterState::default();
    // Socket aggregates (incl. RTT p95) now live under the Sockets tab (#258).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Sockets;
    {
        let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
        assert!(ui.find("RTT p95 (us)").is_ok());
    }
    // xfrm/IPsec now lives under the Firewall & IPsec tab.
    state.specialized_tab = zensight::view::specialized::SpecializedTab::FirewallIpsec;
    {
        let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
        assert!(ui.find("IPsec / xfrm").is_ok());
    }
}

/// #45: netring renders DNS RED, HTTP RED, and per-L4 cards when present.
#[test]
fn test_netring_red_cards() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Netring, "sensor01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: f64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "sensor01".to_string(),
            protocol: Protocol::Netring,
            metric: metric.to_string(),
            value: TelemetryValue::Counter(v as u64),
            labels: HashMap::new(),
            unit: None,
        });
    };
    put("dns/queries_total", 100.0);
    put("http/requests_total", 50.0);
    put("flow/by_l4/tcp/flows_total", 7.0);

    let syslog_filter = SyslogFilterState::default();
    // #247: the RED cards now live in tabs. Overview carries per-L4; DNS and
    // HTTP/TLS each have their own tab. Drive the active tab per assertion
    // (view tests can't run the app update loop to switch via click).
    {
        // Overview (default): per-L4 split + capability-aware tab labels present.
        let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
        assert!(ui.find("Per-protocol (L4)").is_ok());
        assert!(ui.find("DNS").is_ok());
        assert!(ui.find("HTTP/TLS").is_ok());
    }
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Dns;
    {
        let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
        assert!(ui.find("DNS (RED)").is_ok());
    }
    state.specialized_tab = zensight::view::specialized::SpecializedTab::HttpTls;
    {
        let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
        assert!(ui.find("HTTP (RED)").is_ok());
    }
}

/// Test settings view renders correctly.
#[test]
fn test_settings_view() {
    let state = SettingsState::default();
    let mut ui = simulator(settings_view(&state));

    // Should show Settings title
    assert!(ui.find("Settings").is_ok());
    // Should show Zenoh Connection section
    assert!(ui.find("Zenoh Connection").is_ok());
    // Should show Mode picker
    assert!(ui.find("Mode:").is_ok());
    // Should show Save button
    assert!(ui.find("Save Settings").is_ok());
}

/// The link-profile controls render in the Zenoh section (#364).
#[test]
fn test_settings_link_profile_controls() {
    let state = SettingsState::default();
    let mut ui = simulator(settings_view(&state));

    assert!(ui.find("Link profile:").is_ok());
    assert!(ui.find("Subscription scope:").is_ok());
    // Standard profile help text is shown by default.
    assert!(
        ui.find("Full fidelity: reconnect history burst + missed-sample recovery")
            .is_ok()
    );
}

/// Constrained profile swaps in the low-bandwidth help caption (#364).
#[test]
fn test_settings_constrained_profile_help() {
    let mut state = SettingsState::default();
    state.set_link_profile(zensight_common::LinkProfile::Constrained);
    assert!(state.modified);
    let mut ui = simulator(settings_view(&state));
    assert!(
        ui.find("Low bandwidth: no history/recovery traffic; history comes from the local store")
            .is_ok()
    );
}

/// Test clicking Save Settings button.
#[test]
fn test_settings_save_button() {
    let state = SettingsState::default();
    // The settings form is taller than the default simulator viewport since
    // the link-profile section (#364); size up so Save stays clickable.
    let mut ui = iced_test::Simulator::with_size(
        iced::Settings::default(),
        iced::Size::new(1024.0, 1400.0),
        settings_view(&state),
    );

    // Click Save button
    let _ = ui.click("Save Settings");

    // Should have produced SaveSettings message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::SaveSettings)));
}

/// Test metric filtering in device view.
#[test]
fn test_device_metric_filter() {
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add mock telemetry
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }

    // Set filter (goes to pending with debouncing)
    state.set_metric_filter("cpu".to_string());
    // Apply immediately by setting the applied filter directly
    state.metric_filter = state.pending_filter.clone();

    // Verify filtering works
    let filtered = state.sorted_metrics();
    assert!(filtered.iter().all(|(name, _)| name.contains("cpu")));
    assert!(filtered.len() < state.total_metric_count());
}

/// Test SNMP specialized view renders with interface table.
#[test]
fn test_snmp_specialized_view() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add mock SNMP telemetry
    for point in mock::snmp::router("router01") {
        state.update(point);
    }

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Should show the device name
    assert!(ui.find("router01").is_ok());
    // Should show Interfaces section (SNMP specialized view)
    assert!(ui.find("Interfaces").is_ok());
    // Should show System Metrics section
    assert!(ui.find("System Metrics").is_ok());
}

/// SNMP interface table renders from the typed `InterfaceTable` doc (#530):
/// names, alias, rates, utilization, and the down interface.
#[test]
fn test_snmp_interface_table_from_doc() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    for point in mock::snmp::router("router01") {
        state.update(point);
    }
    let metrics = state.metrics.clone();
    state
        .snmp_detail
        .apply_interfaces(mock::snmp::interface_table("router01", 3), &metrics);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    assert!(ui.find("eth0").is_ok());
    assert!(ui.find("uplink to core").is_ok(), "alias renders");
    // eth1's in-rate: 25 MB/s (12.5 MB/s × 2).
    assert!(ui.find("23.8 MB/s").is_ok(), "rates render humanized");
    // eth1 error rate.
    assert!(ui.find("3.5").is_ok(), "error rate renders");
    // Last interface is oper-down while admin-up.
    assert!(ui.find("DOWN").is_ok(), "down interface shows");
}

/// A device without ifXTable still renders a coherent table (no rates yet).
#[test]
fn test_snmp_interface_table_without_hc() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "legacy01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    let metrics = state.metrics.clone();
    state
        .snmp_detail
        .apply_interfaces(mock::snmp::interface_table_no_hc("legacy01"), &metrics);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    assert!(ui.find("eth0").is_ok());
    assert!(ui.find("10 Mb/s").is_ok(), "ifSpeed fallback renders");
}

/// Without the doc, the view shows the waiting hint (no string parsing left).
#[test]
fn test_snmp_view_without_doc_shows_hint() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    for point in mock::snmp::router("router01") {
        state.update(point);
    }

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    assert!(
        ui.find("No interface data yet — waiting for the sensor's interface doc")
            .is_ok()
    );
}

/// Clicking a sortable column header emits the sort message (#530).
#[test]
fn test_snmp_interface_table_sort_click() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    let metrics = state.metrics.clone();
    state
        .snmp_detail
        .apply_interfaces(mock::snmp::interface_table("router01", 2), &metrics);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    ui.click("name").expect("click name header");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::SnmpTableSort(1))),
        "sort message for the name column, got {messages:?}"
    );
}

/// Test syslog specialized view renders with severity distribution.
#[test]
fn test_syslog_specialized_view() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Logs, "server01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add a syslog message
    let mut point = TelemetryPoint::new(
        "server01",
        Protocol::Logs,
        "message/1",
        TelemetryValue::Text("Test log message".to_string()),
    );
    point.labels.insert("severity".to_string(), "4".to_string()); // Warning
    point
        .labels
        .insert("app_name".to_string(), "test".to_string());
    state.update(point);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Should show the device name
    assert!(ui.find("server01").is_ok());
    // Should show Log Stream section (syslog specialized view)
    assert!(ui.find("Log Stream").is_ok());
}

/// Test modbus specialized view renders with register sections.
#[test]
fn test_modbus_specialized_view() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Modbus, "plc01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add a holding register
    let point = TelemetryPoint::new(
        "plc01",
        Protocol::Modbus,
        "holding/40001/temperature",
        TelemetryValue::Gauge(72.5),
    );
    state.update(point);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Should show the device name
    assert!(ui.find("plc01").is_ok());
    // Should show Holding Registers section (modbus specialized view)
    assert!(ui.find("Holding Registers").is_ok());
}

/// Test netflow specialized view renders with traffic sections.
#[test]
fn test_netflow_specialized_view() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Netflow, "router01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add a flow record
    let mut point = TelemetryPoint::new(
        "router01",
        Protocol::Netflow,
        "flow/1",
        TelemetryValue::Counter(1000),
    );
    point
        .labels
        .insert("src_ip".to_string(), "10.0.0.1".to_string());
    point
        .labels
        .insert("dst_ip".to_string(), "10.0.0.2".to_string());
    point.labels.insert("protocol".to_string(), "6".to_string()); // TCP
    state.update(point);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Should show exporter name (NetFlow view shows "Exporter: <name>")
    assert!(ui.find("Exporter: router01").is_ok());
    // Should show Top Talkers section (netflow specialized view)
    assert!(ui.find("Top Talkers (by bytes)").is_ok());
    // Should show Recent Flows section
    assert!(ui.find("Recent Flows").is_ok());
}

/// Test gNMI specialized view renders with path browser.
#[test]
fn test_gnmi_specialized_view() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::fixture(Protocol::Gnmi, "spine01".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Add a gNMI path
    let point = TelemetryPoint::new(
        "spine01",
        Protocol::Gnmi,
        "interfaces/interface/state/name",
        TelemetryValue::Text("eth0".to_string()),
    );
    state.update(point);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));

    // Should show the device name
    assert!(ui.find("spine01").is_ok());
    // Should show Active Subscriptions section (gnmi specialized view)
    assert!(ui.find("Active Subscriptions").is_ok());
    // Should show Path Browser section
    assert!(ui.find("Path Browser").is_ok());
}

// ============================================================================
// Overview Section Tests
// ============================================================================

/// Test that overview section shows when devices are present.
#[test]
fn test_overview_section_renders() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let mut state = DashboardState::default();
    state.connected = true;

    // Add a sysinfo device with metrics
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut device = DeviceState::new(device_id.clone());
    device.metric_count = 3;
    device.is_healthy = true;

    // Add actual telemetry points
    let point = TelemetryPoint::new(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(45.0),
    );
    device.metrics.insert("cpu/usage".to_string(), point);

    state.devices.insert(device_id, device);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // Should show Protocol Overviews header
    assert!(ui.find("Protocol Overviews").is_ok());
    // Should show Sysinfo tab since we have a sysinfo device
    assert!(ui.find("Sysinfo (1)").is_ok());
}

/// Test clicking overview protocol tab.
#[test]
fn test_overview_protocol_tab_click() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let mut state = DashboardState::default();
    state.connected = true;

    // Add an SNMP device
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut device = DeviceState::new(device_id.clone());
    device.metric_count = 1;
    device.is_healthy = true;

    let point = TelemetryPoint::new(
        "router01",
        Protocol::Snmp,
        "ifAdminStatus/1",
        TelemetryValue::Counter(1),
    );
    device.metrics.insert("ifAdminStatus/1".to_string(), point);

    state.devices.insert(device_id, device);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // Click SNMP tab
    let _ = ui.click("SNMP (1)");

    // Should produce SelectOverviewProtocol message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::SelectOverviewProtocol(Protocol::Snmp)))
    );
}

/// Test overview section can be collapsed.
#[test]
fn test_overview_collapse_toggle() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let mut state = DashboardState::default();
    state.connected = true;

    // Add a device so overview shows
    let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut device = DeviceState::new(device_id.clone());

    let point = TelemetryPoint::new(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(50.0),
    );
    device.metrics.insert("cpu/usage".to_string(), point);

    state.devices.insert(device_id, device);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let entities = zensight::entity::EntityStore::default();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // Click the Protocol Overviews header to toggle
    let _ = ui.click("Protocol Overviews");

    // Should produce ToggleOverviewExpanded message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ToggleOverviewExpanded))
    );
}

// ============================================================================
// Topology View Tests
// ============================================================================

/// Test that the topology view renders correctly with no nodes.
#[test]
fn test_topology_view_empty() {
    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));

    // Should show the title
    assert!(ui.find("Network Topology").is_ok());
    // Should show Back button
    assert!(ui.find("Back").is_ok());
    // Should show node count
    assert!(ui.find("0 nodes").is_ok());
    // Should show connection count
    assert!(ui.find("0 connections").is_ok());
}

/// Test clicking Back button in topology view.
#[test]
fn test_topology_back_button() {
    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));

    // Click Back button
    let _ = ui.click("Back");

    // Should have produced CloseTopology message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::CloseTopology)));
}

/// Test topology zoom buttons.
#[test]
fn test_topology_zoom_controls() {
    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));

    // Click zoom in button
    let _ = ui.click("+");

    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologyZoomIn))
    );
}

/// The nav rail's Map button (promoted topology, #133) emits OpenTopology.
#[test]
fn test_shell_topology_button() {
    let mut ui = shell_ui();
    let _ = ui.click("Map");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::OpenTopology)));
}

/// Test topology search input.
#[test]
fn test_topology_search_input() {
    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));

    // Should show search placeholder
    assert!(ui.find("Search nodes...").is_ok());
}

/// The topology lens selector switches presentation lenses (#392).
#[test]
fn test_topology_lens_selector() {
    use zensight::view::topology::Lens;

    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));

    // All four lenses render in the control row.
    for label in ["Traffic", "Security", "L2", "Health"] {
        assert!(ui.find(label).is_ok(), "missing lens button {label}");
    }

    // Clicking an inactive lens emits TopologySetLens.
    let _ = ui.click("Security");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologySetLens(Lens::Security)))
    );
}

/// Filter checkboxes and the grouping picker render and emit toggles (#392).
#[test]
fn test_topology_filter_controls() {
    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    for label in ["Hide idle", "Hide passive", "Hide external", "Flows:"] {
        assert!(ui.find(label).is_ok(), "missing control {label}");
    }
    let _ = ui.click("Hide idle");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologyToggleHideIdle))
    );
}

/// Focus mode: the node panel's Focus button enters, the breadcrumb exits
/// (#392).
#[test]
fn test_topology_focus_flow() {
    use zensight::view::topology::{FocusState, Node};

    // A selected node shows the Focus button.
    let mut state = TopologyState::default();
    state.nodes.insert(
        "web1".to_string(),
        Node {
            id: "web1".to_string(),
            label: "web1".to_string(),
            ..Default::default()
        },
    );
    state.selected_node = Some("web1".to_string());
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    let _ = ui.click("Focus");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologyFocusNode(id) if id == "web1"))
    );

    // Active focus renders the breadcrumb with hop buttons + exit.
    let mut state = TopologyState::default();
    state.prefs.focus = Some(FocusState {
        root: "web1".to_string(),
        hops: 1,
    });
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    assert!(ui.find("Focus: web1").is_ok());
    let _ = ui.click("Exit focus");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologyExitFocus))
    );
}

/// The node panel shows identity, listening sockets, and pivots (#393).
#[test]
fn test_topology_node_panel_sections() {
    use zensight::view::specialized::fetch::Fetch;
    use zensight::view::topology::Node;
    use zensight_common::{Protocol, SocketRecord};

    let mut state = TopologyState::default();
    let mut node = Node {
        id: "web1".to_string(),
        label: "web1".to_string(),
        ips: vec!["10.0.0.11".to_string()],
        cpu_usage: Some(34.0),
        ..Default::default()
    };
    node.protocols.insert(Protocol::Netlink);
    state.nodes.insert("web1".to_string(), node);
    state.selected_node = Some("web1".to_string());
    state.panel.listen = Fetch::Ready(vec![SocketRecord {
        local: "0.0.0.0:22".to_string(),
        state: "listen".to_string(),
        process: Some("sshd".to_string()),
        ..Default::default()
    }]);

    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    assert!(ui.find("Identity").is_ok());
    assert!(ui.find("Listening").is_ok());
    assert!(ui.find(":22 · sshd").is_ok());
    assert!(ui.find("View Device Details").is_ok());
    assert!(ui.find("Focus").is_ok());
}

/// The edge panel lists backing flows with attribution + community-id copy
/// (#393).
#[test]
fn test_topology_edge_panel_flows() {
    use zensight::view::specialized::fetch::Fetch;
    use zensight::view::topology::{Edge, EdgeKind, Node};
    use zensight_common::FlowRecord;

    let mut state = TopologyState::default();
    for id in ["a", "b"] {
        state.nodes.insert(
            id.to_string(),
            Node {
                id: id.to_string(),
                label: id.to_string(),
                ..Default::default()
            },
        );
    }
    state.edges.push(Edge {
        from: "a".to_string(),
        to: "b".to_string(),
        kind: EdgeKind::Flow,
        rate: 1000.0,
        reverse_rate: 200.0,
        ..Default::default()
    });
    state.selected_edge = Some(0);
    state.panel.edge_flows = Fetch::Ready(vec![FlowRecord {
        src: "10.0.0.1:1234".to_string(),
        dst: "10.0.0.2:443".to_string(),
        proto: "tcp".to_string(),
        bytes: 4096,
        packets: 12,
        duration_ms: 350,
        reason: "fin".to_string(),
        community_id: Some("1:abcdef".to_string()),
        directed: true,
        bytes_initiator: 2048,
        bytes_responder: 2048,
        packets_initiator: 6,
        packets_responder: 6,
        dst_names: Vec::new(),
    }]);

    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    assert!(ui.find("Flows").is_ok());
    assert!(ui.find("attr").is_ok());
    // Copying the community id emits the clipboard message.
    let _ = ui.click("copy");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologyCopyText(cid) if cid == "1:abcdef"))
    );
}

/// The legend toggles per lens (#394).
#[test]
fn test_topology_legend_toggle() {
    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    let _ = ui.click("Legend");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::TopologyToggleLegend))
    );

    let mut state = TopologyState::default();
    state.show_legend = true;
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    // Traffic-lens legend content is visible.
    assert!(
        ui.find("solid = traffic · dotted = L2 · dashed = gateway")
            .is_ok()
    );
    // The tiered layout is the default, so the legend leads with the tier
    // order (#443).
    assert!(
        ui.find("rows: Internet → gateways & infrastructure → hosts by subnet → discovered")
            .is_ok()
    );
}

/// The tiered layout is the default and the header adapts to it (#442/#443).
#[test]
fn test_topology_tiered_default_header() {
    use zensight::view::topology::LayoutMode;

    let state = TopologyState::default();
    assert_eq!(state.prefs.layout, LayoutMode::Tiered);
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    // Auto-layout only governs the force simulation — hidden by default.
    assert!(ui.find("Auto Layout: ON").is_err());
    // Deterministic layouts read as stable, never "adjusting".
    assert!(ui.find("Layout: Stable").is_ok());

    // Under Force the auto-layout toggle comes back.
    let mut state = TopologyState::default();
    state.prefs.layout = LayoutMode::Force;
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    assert!(ui.find("Auto Layout: ON").is_ok());
}

/// The active lens button is inert; the edge-label picker is present (#392).
#[test]
fn test_topology_lens_active_inert() {
    let state = TopologyState::default(); // default lens = Traffic
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    let _ = ui.click("Traffic");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::TopologySetLens(_)))
    );

    let state = TopologyState::default();
    let mut ui = simulator(topo_view(&state, AppTheme::Dark));
    assert!(ui.find("Edge labels:").is_ok());
}

/// The security view lists network anomalies (not expectation alerts).
#[test]
fn test_security_view() {
    use zensight::view::alerts::AlertsState;
    use zensight::view::security::{SecurityState, security_view};
    use zensight_common::{Alert, AlertKind, AlertSeverity};

    let mut alerts = AlertsState::new();
    // An anomaly (shown) and an expectation (hidden by the security lens).
    alerts.ingest_external(
        Alert::new(
            "wiretap1",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScanTRW",
            AlertSeverity::Warning,
            "PortScanTRW from 10.0.0.5",
        )
        .with_label("src", "10.0.0.5"),
    );
    alerts.ingest_external(Alert::new(
        "router01",
        Protocol::Netlink,
        AlertKind::Expectation,
        "socket:sshd",
        AlertSeverity::Critical,
        "sshd not listening",
    ));

    let sec = SecurityState::default();
    let tuning = zensight::view::detection_tuning::DetectionTuningState::default();
    let mut ui = simulator(security_view(&alerts, &sec, &tuning));
    assert!(ui.find("Security — Network Anomalies").is_ok());
    assert!(ui.find("PortScanTRW from 10.0.0.5").is_ok());
    assert!(ui.find("10.0.0.5").is_ok());
    // The expectation alert must NOT appear in the security lens.
    assert!(ui.find("sshd not listening").is_err());
}

/// #129: firing alerts dedup into per-host incidents; expanding one reveals its
/// timeline + evidence pivots, and the metric pivot emits InvestigateAlert with
/// the offending metric from the alert's `metric` label.
#[test]
fn test_incidents_group_and_pivot() {
    use zensight::view::alerts::AlertsState;
    use zensight::view::incident::{IncidentsState, incidents_view};
    use zensight_common::{Alert, AlertKind, AlertSeverity};

    let mut alerts = AlertsState::new();
    // Two alerts on the same host coalesce into one incident; the netlink
    // sentinel one carries a `metric` label (the metric-evidence anchor).
    alerts.ingest_external(
        Alert::new(
            "router01",
            Protocol::Netlink,
            AlertKind::Expectation,
            "retrans",
            AlertSeverity::Warning,
            "high retransmits",
        )
        .with_label("metric", "sockets/tcp/retransmits_total"),
    );
    alerts.ingest_external(Alert::new(
        "router01",
        Protocol::Netlink,
        AlertKind::Expectation,
        "socket:sshd",
        AlertSeverity::Critical,
        "sshd not listening",
    ));

    // One incident for the host, max severity (Critical), two alerts.
    let incs = alerts.incidents();
    assert_eq!(incs.len(), 1);
    assert_eq!(incs[0].host, "router01");
    assert_eq!(incs[0].alert_keys.len(), 2);

    // Collapsed: the incident card shows; expand it.
    let state = IncidentsState::default();
    let mut ui = simulator(incidents_view(&alerts, &state));
    assert!(ui.find("Incidents (1)").is_ok());
    assert!(ui.find("router01").is_ok());

    // Expanded: the evidence pivots render and "metric ↗" fires InvestigateAlert
    // with the offending metric.
    let expanded = IncidentsState {
        selected: Some(incs[0].id.clone()),
    };
    let mut ui = simulator(incidents_view(&alerts, &expanded));
    assert!(ui.find("Evidence:").is_ok());
    let _ = ui.click("metric ↗");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::InvestigateAlert { metric: Some(metric), .. }
            if metric == "sockets/tcp/retransmits_total"
    )));
}

/// #73: the netring 0.27 threat-intel anomaly kinds (flow-risk / IOC / Sigma)
/// render as first-class detector cards — friendly titles + a "what it means"
/// description — with the per-detector evidence available in the drill-down.
#[test]
fn test_security_threat_intel_first_class() {
    use zensight::view::security::{SecurityState, security_view};
    use zensight_common::{Alert, AlertKind, AlertSeverity};

    let mut alerts = zensight::view::alerts::AlertsState::new();
    // An IOC match carrying the detector's evidence observations as labels.
    alerts.ingest_external(
        Alert::new(
            "wiretap1",
            Protocol::Netring,
            AlertKind::Anomaly,
            "ioc_match",
            AlertSeverity::Critical,
            "ioc_match 10.0.0.5 -> 203.0.113.6",
        )
        .with_label("src", "10.0.0.5")
        .with_label("ioc_kind", "ip")
        .with_label("indicator", "203.0.113.6"),
    );
    // A flow-risk obsolete-TLS finding.
    alerts.ingest_external(Alert::new(
        "wiretap1",
        Protocol::Netring,
        AlertKind::Anomaly,
        "obsolete_tls",
        AlertSeverity::Warning,
        "obsolete_tls 10.0.0.7 -> 1.1.1.1",
    ));

    // Expand the IOC match so its evidence renders.
    let sec = SecurityState {
        selected: Some(
            Alert::new(
                "wiretap1",
                Protocol::Netring,
                AlertKind::Anomaly,
                "ioc_match",
                AlertSeverity::Critical,
                "ioc_match 10.0.0.5 -> 203.0.113.6",
            )
            .with_label("src", "10.0.0.5")
            .with_label("ioc_kind", "ip")
            .with_label("indicator", "203.0.113.6")
            .alert_key(),
        ),
        ..SecurityState::default()
    };

    let tuning = zensight::view::detection_tuning::DetectionTuningState::default();
    let mut ui = simulator(security_view(&alerts, &sec, &tuning));
    // Friendly detector titles (not the raw slugs).
    assert!(ui.find("IOC match").is_ok());
    assert!(ui.find("Obsolete TLS").is_ok());
    // "What it means" descriptions.
    assert!(
        ui.find("Flow matched a known indicator of compromise")
            .is_ok()
    );
    // The detector's evidence observation is in the drill-down.
    assert!(ui.find("203.0.113.6").is_ok());
}

/// #48: clicking an anomaly row expands its evidence drill-down (emits
/// SelectAnomaly), and the "Hide info" toggle emits its message.
#[test]
fn test_security_drilldown_and_filter() {
    use zensight::view::security::{SecurityState, security_view};
    use zensight_common::{Alert, AlertKind, AlertSeverity};

    let mut alerts = zensight::view::alerts::AlertsState::new();
    let mut a = Alert::new(
        "10.0.0.5",
        Protocol::Netring,
        AlertKind::Anomaly,
        "PortScanTRW",
        AlertSeverity::Warning,
        "PortScanTRW from 10.0.0.5",
    );
    a.labels.insert("src".into(), "10.0.0.5".into());
    a.labels.insert("n_observed".into(), "42".into());
    alerts.ingest_external(a);

    let sec = SecurityState::default();
    let tuning = zensight::view::detection_tuning::DetectionTuningState::default();
    let mut ui = simulator(security_view(&alerts, &sec, &tuning));
    let _ = ui.click("PortScanTRW from 10.0.0.5");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::SelectAnomaly(Some(_)))),
        "row click should emit SelectAnomaly, got {msgs:?}"
    );

    // With the anomaly expanded, its evidence label is visible.
    let sec2 = SecurityState {
        selected: Some(alerts.active_external()[0].alert_key()),
        hide_info: false,
        ..SecurityState::default()
    };
    let mut ui2 = simulator(security_view(&alerts, &sec2, &tuning));
    assert!(ui2.find("n_observed:").is_ok());
    // #119: an anomaly with a src exposes the flow-pivot action.
    assert!(ui2.find("Show flows").is_ok());
}

/// #27: the external-alerts feed shows severity + source filter pills; clicking
/// one emits the corresponding filter message.
#[test]
fn test_alert_filter_pills() {
    use zensight::view::alerts::{AlertsState, alerts_view};
    use zensight_common::{Alert, AlertKind, AlertSeverity};

    let mut alerts = AlertsState::new();
    alerts.ingest_external(Alert::new(
        "host1",
        Protocol::Netlink,
        AlertKind::Expectation,
        "ssh-listening",
        AlertSeverity::Critical,
        "sshd down on host1",
    ));
    alerts.ingest_external(Alert::new(
        "host2",
        Protocol::Netlink,
        AlertKind::Expectation,
        "ntp-listening",
        AlertSeverity::Warning,
        "ntp down on host2",
    ));

    // Pills render (severity row + source row since there are two sources).
    let mut ui = simulator(alerts_view(&alerts));
    assert!(ui.find("Severity").is_ok());
    assert!(ui.find("Source").is_ok());

    // Click the "Critical" severity pill.
    let _ = ui.click("Critical");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            Message::SetAlertSeverityFilter(Some(AlertSeverity::Critical))
        )),
        "Critical pill should emit SetAlertSeverityFilter(Critical), got {msgs:?}"
    );

    // Click a source pill.
    let mut ui2 = simulator(alerts_view(&alerts));
    let _ = ui2.click("host2");
    let msgs2: Vec<Message> = ui2.into_messages().collect();
    assert!(
        msgs2
            .iter()
            .any(|m| matches!(m, Message::SetAlertSourceFilter(Some(s)) if s == "host2")),
        "host2 pill should emit SetAlertSourceFilter(host2), got {msgs2:?}"
    );
}

/// The expectations authoring view renders and "Add & Push" emits a message.
#[test]
fn test_expectations_view() {
    use zensight::view::expectations::{ExpectationsState, expectations_view, parse_status};

    let mut state = ExpectationsState::default();
    state.current = parse_status(
        r#"{"sockets":[{"name":"sshd","listen":22,"severity":"critical"}],"links":[]}"#,
    );

    let mut ui = simulator(expectations_view(&state));
    assert!(ui.find("Expectations (netlink sentinel)").is_ok());
    assert!(ui.find("socket:sshd").is_ok());
    assert!(ui.find("listen :22").is_ok());

    let _ = ui.click("Add & Push");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::AddExpectation))
    );

    // Metric-threshold kind renders the metric form (no panic) and Add & Push
    // still emits AddExpectation (the app builds an add_metric command from it).
    state.new_kind = zensight::view::expectations::ExpKind::MetricThreshold;
    state.new_metric = "conntrack/utilization".into();
    state.new_value = "0.9".into();
    let mut ui = simulator(expectations_view(&state));
    let _ = ui.click("Add & Push");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::AddExpectation))
    );
}

/// Every specialized device view is wrapped with the shared nav header, so a
/// Back button is present and clicking it clears the selection (returns to the
/// dashboard). Regression guard for "specialized views had no Back button".
#[test]
fn test_specialized_device_view_has_back_button() {
    use zensight::view::device::device_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "router01");
    let mut state = DeviceDetailState::new(device_id);
    state.update(TelemetryPoint::new(
        "router01",
        Protocol::Netlink,
        "iface/eth0/rx_bytes",
        TelemetryValue::Counter(1000),
    ));

    let mut ui = simulator(device_view(&state));
    // The specialized netlink content is present...
    assert!(ui.find("Netlink: router01").is_ok());
    // ...AND a Back button now wraps it.
    assert!(ui.find("Back").is_ok());
    let _ = ui.click("Back");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ClearSelection)),
        "clicking Back should clear the device selection"
    );
}

/// The netlink specialized view renders interfaces + socket aggregates.
#[test]
fn test_netlink_specialized_view() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "router01");
    let mut state = DeviceDetailState::new(device_id);
    for (metric, value) in [
        ("iface/eth0/rx_bytes", TelemetryValue::Counter(1000)),
        ("iface/eth0/tx_bytes", TelemetryValue::Counter(2000)),
        ("iface/eth0/oper_state", TelemetryValue::Text("up".into())),
        ("iface/eth0/mtu", TelemetryValue::Gauge(1500.0)),
        ("sockets/tcp/established", TelemetryValue::Gauge(7.0)),
        ("sockets/tcp/listen", TelemetryValue::Gauge(3.0)),
        ("diagnostics/bottleneck_score", TelemetryValue::Gauge(0.0)),
        ("diagnostics/issues/total", TelemetryValue::Gauge(0.0)),
        ("neighbors/total", TelemetryValue::Gauge(4.0)),
        ("neighbors/by_state/reachable", TelemetryValue::Gauge(2.0)),
        ("routes/ipv4_count", TelemetryValue::Gauge(5.0)),
        ("routes/default_v4_present", TelemetryValue::Boolean(true)),
    ] {
        state.update(TelemetryPoint::new(
            "router01",
            Protocol::Netlink,
            metric,
            value,
        ));
    }

    // Pre-populate an on-demand fetched socket detail table (as if the query
    // channel had replied) to exercise the drill-down render path.
    {
        use zensight::view::specialized::netlink_detail::{NetlinkDetailData, NetlinkDetailTopic};
        state.netlink_detail.apply(
            NetlinkDetailTopic::Sockets,
            Ok(NetlinkDetailData::Sockets(vec![
                zensight_common::SocketRecord {
                    local: "10.0.0.1:5555".into(),
                    remote: "1.1.1.1:443".into(),
                    state: "established".into(),
                    uid: 1000,
                    recv_q: 0,
                    send_q: 0,
                    rtt_us: 1234,
                    retrans: 0,
                    inode: 9999,
                    congestion: Some("cubic".into()),
                    bbr_bw_bps: None,
                    cc_min_rtt_us: None,
                    snd_cwnd: 10,
                    snd_buf: 16384,
                    rcv_buf: 32768,
                    delivery_rate: 0,
                    pacing_rate: 0,
                    bytes_retrans: 0,
                    bytes_acked: 0,
                    bytes_received: 0,
                    bytes_sent: 0,
                    total_retrans: 0,
                    rcv_rtt_us: 0,
                    lost: 0,
                    reord_seen: 0,
                    cookie: 42,
                    cgroup_id: None,
                    cgroup: None,
                    pid: Some(4321),
                    process: Some("sshd".into()),
                    proc_start_time: Some(987654),
                },
            ])),
        );
    }

    // #258: the view is tabbed. The header is always visible; each section now
    // lives in exactly one tab — drive the active tab explicitly (view tests
    // can't switch via click) and assert per-tab.
    use zensight::view::specialized::SpecializedTab;
    // Overview: header + health hero (bottleneck gauge) + TCP-health tiles.
    {
        state.specialized_tab = SpecializedTab::Overview;
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("Netlink: router01").is_ok());
        assert!(ui.find("Health").is_ok());
        assert!(ui.find("TCP health").is_ok());
    }
    // Interfaces tab.
    {
        state.specialized_tab = SpecializedTab::Interfaces;
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("eth0").is_ok());
    }
    // Sockets tab: aggregates + first-class explorer (refresh + fetched table).
    {
        state.specialized_tab = SpecializedTab::Sockets;
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("TCP Sockets").is_ok());
        assert!(ui.find("Socket Explorer").is_ok());
        assert!(ui.find("Fetch Sockets").is_ok());
        assert!(ui.find("10.0.0.1:5555").is_ok());
        // Enriched columns + pagination footer (#261, no silent .take(200)).
        assert!(ui.find("cong").is_ok());
        assert!(ui.find("showing 1 of 1 sockets").is_ok());
    }
    // Routing & Neighbors tab.
    {
        state.specialized_tab = SpecializedTab::RoutingNeighbors;
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("Neighbors (ARP/NDP)").is_ok());
        assert!(ui.find("Routes").is_ok());
    }
}

/// The netring specialized view renders flows + top talkers.
#[test]
fn test_netring_specialized_view() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    for (metric, value) in [
        ("flow/started_total", TelemetryValue::Counter(10)),
        ("flow/active", TelemetryValue::Gauge(2.0)),
        ("flow/bytes_total", TelemetryValue::Counter(582)),
        ("flow/packets_total", TelemetryValue::Counter(10)),
        ("tcp/resets_total", TelemetryValue::Counter(1)),
        ("tcp/refused_total", TelemetryValue::Counter(0)),
        (
            "bandwidth/https/bytes_per_sec",
            TelemetryValue::Gauge(50000.0),
        ),
        ("bandwidth/dns/bytes_per_sec", TelemetryValue::Gauge(1200.0)),
    ] {
        state.update(TelemetryPoint::new(
            "wiretap1",
            Protocol::Netring,
            metric,
            value,
        ));
    }

    // Pre-populate on-demand flow detail (as if @rpc/netring/flows had replied).
    state
        .netring_detail
        .apply(Ok(vec![zensight_common::FlowRecord {
            src: "10.0.0.1:54321".into(),
            dst: "10.0.0.2:80".into(),
            proto: "tcp".into(),
            bytes: 694,
            packets: 10,
            duration_ms: 100,
            reason: "fin".into(),
            community_id: None,
            directed: true,
            bytes_initiator: 120,
            bytes_responder: 574,
            packets_initiator: 4,
            packets_responder: 6,
            dst_names: Vec::new(),
        }]));

    // #247: content is tabbed. Loading/error render inline on the Flows tab;
    // drive the active tab explicitly (view tests can't switch via click).
    {
        let mut s = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "wiretap1"));
        s.specialized_tab = zensight::view::specialized::SpecializedTab::Flows;
        s.netring_detail.loading();
        {
            let mut ui = simulator(netring_sensor_view(&s, None));
            assert!(ui.find("Fetching…").is_ok());
        }

        s.netring_detail.apply(Err("no sensor".into()));
        let mut ui = simulator(netring_sensor_view(&s, None));
        assert!(ui.find("Fetch failed: no sensor").is_ok());
    }

    // Overview (default): header + flow-volume + TCP health + tab strip.
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Netring: wiretap1").is_ok());
        assert!(ui.find("Flows").is_ok()); // tab label
        assert!(ui.find("TCP Health").is_ok());
        assert!(ui.find("bytes (total)").is_ok());
    }

    // Bandwidth tab: per-app throughput.
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Bandwidth;
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("https").is_ok());
    }

    // Flows tab: on-demand flow detail (fetch button + fetched row + orientation).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Flows;
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Recent Flows (on demand)").is_ok());
        assert!(ui.find("Fetch Flows").is_ok());
        assert!(ui.find("10.0.0.1:54321").is_ok());
        // #228 orientation: directed flows show initiator→responder columns + the
        // directed arrow + a per-direction byte split.
        assert!(ui.find("initiator").is_ok());
        assert!(ui.find("responder").is_ok());
        assert!(ui.find("out↑ / in↓").is_ok());
        assert!(ui.find("→").is_ok());
    }
}

/// Sensor-pushed alerts render in the alerts view's "Anomalies & Expectations"
/// section, and resolved alerts disappear.
#[test]
fn test_external_alerts_render_and_resolve() {
    use zensight::view::alerts::{AlertsState, alerts_view};
    use zensight_common::{Alert, AlertKind, AlertSeverity};

    let mut state = AlertsState::new();
    // Empty: section present, no alerts.
    {
        let mut ui = simulator(alerts_view(&state));
        assert!(ui.find("Anomalies & Expectations (0)").is_ok());
        assert!(ui.find("No active sensor alerts").is_ok());
    }

    // Ingest a firing expectation alert.
    let alert = Alert::new(
        "router01",
        Protocol::Netlink,
        AlertKind::Expectation,
        "ssh-listening",
        AlertSeverity::Critical,
        "sshd not listening on :22",
    );
    state.ingest_external(alert.clone());
    {
        let mut ui = simulator(alerts_view(&state));
        assert!(ui.find("Anomalies & Expectations (1)").is_ok());
        assert!(ui.find("sshd not listening on :22").is_ok());
        assert!(ui.find("netlink/router01").is_ok());
    }

    // Resolve it → section back to empty.
    state.ingest_external(alert.resolved());
    {
        let mut ui = simulator(alerts_view(&state));
        assert!(ui.find("Anomalies & Expectations (0)").is_ok());
    }
}

/// The netlink and netring overviews render real aggregates (replacing the old
/// "not implemented" placeholders).
#[test]
fn test_netlink_netring_overviews_render() {
    use std::collections::HashMap;
    use zensight::view::dashboard::DeviceState;
    use zensight::view::overview::{netlink::netlink_overview, netring::netring_overview};
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    // Netlink host with an up interface + established sockets.
    let nl_id = DeviceId::fixture(Protocol::Netlink, "router01");
    let mut nl = DeviceState::new(nl_id.clone());
    nl.metrics.insert(
        "iface/eth0/up".into(),
        TelemetryPoint::new(
            "router01",
            Protocol::Netlink,
            "iface/eth0/up",
            TelemetryValue::Boolean(true),
        ),
    );
    nl.metrics.insert(
        "sockets/tcp/established".into(),
        TelemetryPoint::new(
            "router01",
            Protocol::Netlink,
            "sockets/tcp/established",
            TelemetryValue::Gauge(7.0),
        ),
    );
    let nl_map: HashMap<&DeviceId, &DeviceState> = std::iter::once((&nl_id, &nl)).collect();
    let mut ui = simulator(netlink_overview(&nl_map));
    assert!(ui.find("Interfaces up").is_ok());
    assert!(ui.find("TCP established").is_ok());

    // Netring sensor with flow + reset metrics.
    let nr_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut nr = DeviceState::new(nr_id.clone());
    nr.metrics.insert(
        "flow/active".into(),
        TelemetryPoint::new(
            "wiretap1",
            Protocol::Netring,
            "flow/active",
            TelemetryValue::Gauge(3.0),
        ),
    );
    nr.metrics.insert(
        "tcp/resets_total".into(),
        TelemetryPoint::new(
            "wiretap1",
            Protocol::Netring,
            "tcp/resets_total",
            TelemetryValue::Counter(5),
        ),
    );
    let nr_map: HashMap<&DeviceId, &DeviceState> = std::iter::once((&nr_id, &nr)).collect();
    let mut ui = simulator(netring_overview(&nr_map));
    assert!(ui.find("Active flows").is_ok());
    assert!(ui.find("TCP resets").is_ok());
}

/// The Sensors view surfaces sensor health (previously collected but never shown).
#[test]
fn test_sensors_view() {
    use std::collections::{HashMap, VecDeque};
    use zensight::view::artifact_fetch::ArtifactFetch;
    use zensight::view::sensors::sensors_view;
    use zensight_common::{
        ErrorReport, ErrorType, HealthSnapshot, HealthStatus, KindAdvert, KindStatus,
    };

    let idle = ArtifactFetch::default();
    let no_kinds: HashMap<String, Vec<KindStatus>> = HashMap::new();
    let no_forms: HashMap<String, zensight::view::artifact_fetch::CaptureForm> = HashMap::new();

    // Empty state.
    let empty: HashMap<String, HealthSnapshot> = HashMap::new();
    let no_errors: HashMap<String, VecDeque<ErrorReport>> = HashMap::new();
    let mut ui = simulator(sensors_view(
        &empty, &no_errors, &idle, None, None, &no_kinds, &no_forms,
    ));
    assert!(ui.find("Sensors").is_ok());
    assert!(ui.find("No sensor health received yet.").is_ok());

    // Populated: a degraded sensor renders its name, badge, and stats.
    let mut health = HashMap::new();
    health.insert(
        "snmp".to_string(),
        HealthSnapshot {
            sensor: "snmp".into(),
            status: HealthStatus::Degraded,
            uptime_secs: 7200,
            devices_total: 10,
            devices_responding: 8,
            devices_failed: 2,
            last_poll_duration_ms: 42,
            errors_last_hour: 3,
            metrics_published: 1234,
            host_id: None,
            source: None,
        },
    );
    // ...with a recent error report.
    let mut errors = HashMap::new();
    let mut ring = VecDeque::new();
    ring.push_back(ErrorReport {
        timestamp: 1_700_000_000_000,
        device: Some("router01".into()),
        error_type: ErrorType::Timeout,
        message: "poll timed out".into(),
        retryable: true,
    });
    errors.insert("snmp".to_string(), ring);

    // snmp advertises a debug-report artifact kind.
    let mut kinds: HashMap<String, Vec<KindStatus>> = HashMap::new();
    kinds.insert(
        "snmp".to_string(),
        vec![KindStatus {
            kind: "report".into(),
            busy: false,
            current: None,
            max_bytes: 1 << 20,
            cooldown_secs: 30,
            advert: KindAdvert::Report {},
        }],
    );

    let mut ui = simulator(sensors_view(
        &health, &errors, &idle, None, None, &kinds, &no_forms,
    ));
    assert!(ui.find("snmp").is_ok());
    assert!(ui.find("Degraded").is_ok());
    assert!(ui.find("Responding").is_ok());
    assert!(ui.find("Recent errors (1)").is_ok());
    // The per-sensor debug-report download control is present (report advert).
    assert!(ui.find("Download debug report").is_ok());

    // While a download is active for this sensor, the card shows progress + Cancel.
    // A report is a blob delivery, so its progress label has no "chunks".
    let active = ArtifactFetch::Downloading { got: 1, total: 4 };
    let mut ui = simulator(sensors_view(
        &health,
        &errors,
        &active,
        Some("snmp"),
        Some("report"),
        &kinds,
        &no_forms,
    ));
    assert!(ui.find("Cancel").is_ok());
    assert!(ui.find("Downloading 1/4 (25%)").is_ok());
}

/// The Sensors view surfaces Tier-2 directory-snapshot download controls (#199):
/// a button per advertised directory, and progress + Cancel while a job runs.
#[test]
fn test_sensors_snapshot_dirs() {
    use std::collections::{HashMap, VecDeque};
    use zensight::message::Message;
    use zensight::view::artifact_fetch::ArtifactFetch;
    use zensight::view::sensors::sensors_view;
    use zensight_common::{
        ArtifactKind, ErrorReport, HealthSnapshot, HealthStatus, KindAdvert, KindStatus,
    };

    let idle = ArtifactFetch::default();
    let no_errors: HashMap<String, VecDeque<ErrorReport>> = HashMap::new();
    let no_forms: HashMap<String, zensight::view::artifact_fetch::CaptureForm> = HashMap::new();

    let mut health = HashMap::new();
    health.insert(
        "sysinfo".to_string(),
        HealthSnapshot {
            sensor: "sysinfo".into(),
            status: HealthStatus::Healthy,
            uptime_secs: 60,
            devices_total: 1,
            devices_responding: 1,
            devices_failed: 0,
            last_poll_duration_ms: 5,
            errors_last_hour: 0,
            metrics_published: 10,
            host_id: None,
            source: None,
        },
    );

    // sysinfo advertises a snapshot kind with two directories.
    let mut kinds: HashMap<String, Vec<KindStatus>> = HashMap::new();
    kinds.insert(
        "sysinfo".to_string(),
        vec![KindStatus {
            kind: "snapshot".into(),
            busy: false,
            current: None,
            max_bytes: 1 << 20,
            cooldown_secs: 30,
            advert: KindAdvert::Snapshot {
                dirs: vec!["etc".to_string(), "pcaps".to_string()],
            },
        }],
    );

    // Idle: a "Download <name>" button per directory, and clicking one emits a
    // StartArtifact with a Snapshot kind for that directory.
    let mut ui = simulator(sensors_view(
        &health, &no_errors, &idle, None, None, &kinds, &no_forms,
    ));
    assert!(ui.find("Directory snapshots").is_ok());
    assert!(ui.find("Download etc").is_ok());
    assert!(ui.find("Download pcaps").is_ok());
    let _ = ui.click("Download etc");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::StartArtifact { producer, kind: ArtifactKind::Snapshot { dir }, target_source: None }
            if producer == "sysinfo" && dir == "etc"
    )));

    // Downloading a snapshot (tree delivery): chunk progress + a Cancel button.
    let fetching = ArtifactFetch::Downloading { got: 2, total: 5 };
    let mut ui = simulator(sensors_view(
        &health,
        &no_errors,
        &fetching,
        Some("sysinfo"),
        Some("snapshot"),
        &kinds,
        &no_forms,
    ));
    assert!(ui.find("Downloading 2/5 chunks (40%)").is_ok());
    assert!(ui.find("Cancel").is_ok());
}

/// Settings shows an inline validation warning and disables Save on bad input.
#[test]
fn test_settings_invalid_disables_save() {
    let mut state = SettingsState::default();
    state.max_history = "abc".to_string(); // not a number

    let mut ui = simulator(settings_view(&state));
    // Inline warning is shown.
    assert!(ui.find("⚠ Max history must be a number").is_ok());
    // Clicking Save produces NO SaveSettings message (button disabled).
    let _ = ui.click("Save Settings");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(!messages.iter().any(|m| matches!(m, Message::SaveSettings)));
}

/// The netlink view shows Conntrack + WireGuard sections when those metrics
/// are present (NAT gateway / VPN host), and hides them otherwise.
#[test]
fn test_netlink_conntrack_wireguard_sections() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);

    // Without conntrack/wireguard metrics: sections absent.
    state.update(TelemetryPoint::new(
        "gw01",
        Protocol::Netlink,
        "iface/eth0/up",
        TelemetryValue::Boolean(true),
    ));
    {
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("Conntrack").is_err());
        assert!(ui.find("WireGuard").is_err());
    }

    // Add conntrack + a WireGuard peer.
    for (m, v) in [
        ("conntrack/entries", TelemetryValue::Gauge(1500.0)),
        ("conntrack/by_proto/tcp", TelemetryValue::Gauge(1000.0)),
        ("conntrack/utilization", TelemetryValue::Gauge(0.75)),
        ("wireguard/wg0/peers", TelemetryValue::Gauge(1.0)),
        (
            "wireguard/wg0/abcd1234/rx_bytes",
            TelemetryValue::Counter(1000),
        ),
        ("wireguard/wg0/abcd1234/up", TelemetryValue::Boolean(true)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    // #258: conntrack now lives under the Firewall & IPsec tab, WireGuard under
    // its own (now-visible) tab.
    use zensight::view::specialized::SpecializedTab;
    {
        state.specialized_tab = SpecializedTab::FirewallIpsec;
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("Conntrack").is_ok());
        assert!(ui.find("75%").is_ok()); // utilization as a proper gauge (#264)
    }
    {
        state.specialized_tab = SpecializedTab::WireGuard;
        let mut ui = simulator(netlink_host_view(&state));
        // The WireGuard tab is visible (capability-aware) and shows the peers.
        assert!(ui.find("WireGuard").is_ok());
        assert!(ui.find("wg0 — 1 peers").is_ok());
    }
}

/// #258: the netlink view is capability-aware (QoS/Firewall/WireGuard tabs only
/// when their data is present) and clicking an inactive tab emits a select.
#[test]
fn test_netlink_tabs_capability_and_switch() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    // Bare host: only base metrics, no tc/xfrm/conntrack/wireguard.
    state.update(TelemetryPoint::new(
        "gw01",
        Protocol::Netlink,
        "iface/eth0/up",
        TelemetryValue::Boolean(true),
    ));
    {
        let mut ui = simulator(netlink_host_view(&state));
        // Always-on tabs.
        assert!(ui.find("Overview").is_ok());
        assert!(ui.find("Interfaces").is_ok());
        assert!(ui.find("Sockets").is_ok());
        // Capability tabs hidden without data.
        assert!(ui.find("QoS / Queues").is_err());
        assert!(ui.find("Firewall & IPsec").is_err());
        assert!(ui.find("WireGuard").is_err());
    }

    // Add tc + xfrm + wireguard → those tabs appear.
    for (m, v) in [
        ("tc/eth0/fq_codel/drops", TelemetryValue::Counter(1)),
        ("xfrm/sa/total", TelemetryValue::Gauge(2.0)),
        ("wireguard/wg0/peers", TelemetryValue::Gauge(1.0)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    {
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("QoS / Queues").is_ok());
        assert!(ui.find("Firewall & IPsec").is_ok());
        assert!(ui.find("WireGuard").is_ok());
        // Clicking an inactive tab emits SelectSpecializedTab for it.
        let _ = ui.click("Sockets");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::SelectSpecializedTab(d, t)
                if d.source == "gw01"
                    && *t == zensight::view::specialized::SpecializedTab::Sockets
        )));
    }
}

/// #266: the WireGuard tab renders the summary line + per-peer handshake-age
/// chip + up/stale status.
#[test]
fn test_netlink_wireguard_tab() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::WireGuard;
    for (m, v) in [
        ("wireguard/wg0/peers", TelemetryValue::Gauge(1.0)),
        (
            "wireguard/wg0/abcd1234/last_handshake_age_s",
            TelemetryValue::Gauge(30.0),
        ),
        ("wireguard/wg0/abcd1234/up", TelemetryValue::Boolean(true)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    // rx_bytes carries the wg-quick AllowedIPs enrichment label (#268).
    state.update(
        TelemetryPoint::new(
            "gw01",
            Protocol::Netlink,
            "wireguard/wg0/abcd1234/rx_bytes",
            TelemetryValue::Counter(1000),
        )
        .with_labels(HashMap::from([(
            "allowed_ips".to_string(),
            "10.8.0.2/32".to_string(),
        )])),
    );

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("1 interfaces · 1 peers · 1 active").is_ok());
    assert!(ui.find("wg0 — 1 peers").is_ok());
    assert!(ui.find("handshake 30s ago").is_ok());
    assert!(ui.find("up").is_ok());
    // The peer is named by its AllowedIPs (from wg-quick), not the pubkey (#268).
    assert!(ui.find("10.8.0.2/32").is_ok());
}

/// #265: the Events tab renders the per-family context chart + a structured,
/// newest-first control-plane timeline DataTable.
#[test]
fn test_netlink_events_tab() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight::view::specialized::netlink_detail::{
        EventRecord, NetlinkDetailData, NetlinkDetailTopic,
    };
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Events;
    state.update(TelemetryPoint::new(
        "gw01",
        Protocol::Netlink,
        "events/link/added_total",
        TelemetryValue::Counter(4),
    ));
    state.netlink_detail.apply(
        NetlinkDetailTopic::Events,
        Ok(NetlinkDetailData::Events(vec![
            EventRecord {
                ts_unix: 100,
                family: "link".into(),
                action: "changed".into(),
                ifindex: Some(3),
                detail: "eth0".into(),
            },
            EventRecord {
                ts_unix: 200,
                family: "route".into(),
                action: "added".into(),
                ifindex: None,
                detail: "default".into(),
            },
        ])),
    );

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("Event families").is_ok());
    assert!(ui.find("Event timeline").is_ok());
    assert!(ui.find("eth0").is_ok());
    assert!(ui.find("default").is_ok());
    // Timeline is newest-first: the ts=200 route event sorted ahead of ts=100.
    assert_eq!(state.netlink_detail.events.ready().unwrap()[0].ts_unix, 200);
}

/// #264: the Firewall & IPsec tab renders the conntrack gauge + per-proto donut,
/// the nft rule DataTable, and the xfrm SA DataTable.
#[test]
fn test_netlink_firewall_tab() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight::view::specialized::netlink_detail::{NetlinkDetailData, NetlinkDetailTopic};
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::FirewallIpsec;
    for (m, v) in [
        ("conntrack/entries", TelemetryValue::Gauge(100.0)),
        ("conntrack/utilization", TelemetryValue::Gauge(0.5)),
        ("conntrack/by_proto/tcp", TelemetryValue::Gauge(80.0)),
        ("xfrm/sa/total", TelemetryValue::Gauge(1.0)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    state.netlink_detail.apply(
        NetlinkDetailTopic::Nft,
        Ok(NetlinkDetailData::Nft(vec![
            zensight::view::specialized::netlink_detail::NftRuleRecord {
                family: "inet".into(),
                table: "filter".into(),
                chain: "input".into(),
                handle: 4,
                comment: Some("drop-bad".into()),
                packets: 12,
                bytes: 900,
            },
        ])),
    );

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("Conntrack").is_ok());
    assert!(ui.find("50%").is_ok()); // conntrack utilization gauge
    // nft rule DataTable (per-rule hit counters).
    assert!(ui.find("drop-bad").is_ok());
    assert!(ui.find("nftables rules").is_ok());
    assert!(ui.find("IPsec SAs").is_ok());
}

/// #263: the QoS tab renders a per-qdisc health chip + AQM class + backlog
/// trend, plus the qdisc/class tree DataTable.
#[test]
fn test_netlink_qos_tab() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Qos;
    for (m, v) in [
        ("tc/eth0/fq_codel/drops", TelemetryValue::Counter(5)),
        (
            "tc/eth0/fq_codel/backlog_bytes",
            TelemetryValue::Gauge(2048.0),
        ),
        ("tc/eth0/fq_codel/health_score", TelemetryValue::Gauge(0.9)),
        ("tc/eth0/aqm_class", TelemetryValue::Text("aqm".into())),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("eth0 / fq_codel").is_ok());
    assert!(ui.find("health 0.90").is_ok());
    assert!(ui.find("AQM aqm").is_ok());
    assert!(ui.find("backlog bytes").is_ok());
    assert!(ui.find("Qdisc / class tree").is_ok());
}

/// #262: the Routing & Neighbors tab renders the routes DataTable (sortable),
/// the neighbor-state donut, and the default-route flap section.
#[test]
fn test_netlink_routing_tab() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight::view::specialized::netlink_detail::{NetlinkDetailData, NetlinkDetailTopic};
    use zensight_common::{Protocol, RouteRecord, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::RoutingNeighbors;
    for (m, v) in [
        ("routes/ipv4_count", TelemetryValue::Gauge(5.0)),
        ("routes/default_v4_flaps_total", TelemetryValue::Counter(3)),
        ("neighbors/total", TelemetryValue::Gauge(4.0)),
        ("neighbors/by_state/reachable", TelemetryValue::Gauge(3.0)),
        ("neighbors/by_state/stale", TelemetryValue::Gauge(1.0)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    state.netlink_detail.apply(
        NetlinkDetailTopic::Routes,
        Ok(NetlinkDetailData::Routes(vec![RouteRecord {
            family: 2,
            dst: "10.0.0.0/24".into(),
            gateway: Some("10.0.0.1".into()),
            oif: None,
            priority: None,
            protocol: "kernel".into(),
            scope: "link".into(),
            table: 254,
        }])),
    );

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("Default-route flaps").is_ok());
    assert!(ui.find("Neighbor states").is_ok());
    // Routes DataTable rendered (sortable "destination" header + fetched row);
    // header click→NetlinkTableSort wiring is covered by data_table's own tests.
    assert!(ui.find("destination").is_ok());
    assert!(ui.find("10.0.0.0/24").is_ok());
}

/// #260: the Interfaces tab renders per-iface throughput tiles + inline ethtool
/// link health, and the "View sockets →" pivot navigates to the Sockets tab.
#[test]
fn test_netlink_interfaces_tab_and_pivot() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Interfaces;
    for (m, v) in [
        ("iface/eth0/oper_state", TelemetryValue::Text("up".into())),
        ("iface/eth0/mtu", TelemetryValue::Gauge(1500.0)),
        ("iface/eth0/rx_bytes", TelemetryValue::Counter(1000)),
        ("iface/eth0/tx_bytes", TelemetryValue::Counter(2000)),
        ("ethtool/eth0/carrier", TelemetryValue::Gauge(1.0)),
        ("ethtool/eth0/speed_mbps", TelemetryValue::Gauge(1000.0)),
        ("ethtool/eth0/fec/modes", TelemetryValue::Text("RS".into())),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("eth0").is_ok());
    assert!(ui.find("rx ↓").is_ok());
    assert!(ui.find("FEC RS").is_ok());
    // iface → sockets pivot.
    let _ = ui.click("View sockets →");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::SelectSpecializedTab(d, t)
            if d.source == "gw01"
                && *t == zensight::view::specialized::SpecializedTab::Sockets
    )));
}

/// #259: the Overview hero renders the interface status strip + route/neighbor
/// health chips + TCP-health tiles from live data.
#[test]
fn test_netlink_overview_hero() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Overview;
    for (m, v) in [
        ("diagnostics/bottleneck_score", TelemetryValue::Gauge(0.4)),
        ("diagnostics/issues/critical", TelemetryValue::Gauge(1.0)),
        ("iface/eth0/oper_state", TelemetryValue::Text("up".into())),
        ("iface/wan0/oper_state", TelemetryValue::Text("down".into())),
        ("sockets/tcp/established", TelemetryValue::Gauge(12.0)),
        ("sockets/tcp/retransmits_total", TelemetryValue::Counter(50)),
        ("routes/default_v4_present", TelemetryValue::Boolean(true)),
        (
            "routes/default_v4_gw",
            TelemetryValue::Text("10.0.0.1".into()),
        ),
        ("neighbors/total", TelemetryValue::Gauge(8.0)),
        ("neighbors/by_state/failed", TelemetryValue::Gauge(2.0)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }

    let mut ui = simulator(netlink_host_view(&state));
    // Interface strip chips.
    assert!(ui.find("eth0").is_ok());
    assert!(ui.find("wan0").is_ok());
    // TCP-health tiles incl. retransmit rate label.
    assert!(ui.find("retransmits/s").is_ok());
    assert!(ui.find("RTT p95 (µs)").is_ok());
    // Route + neighbor chips.
    assert!(ui.find("default → 10.0.0.1").is_ok());
    assert!(ui.find("neighbors: 8 (2 failed)").is_ok());
}

/// #261: the Sockets explorer paginates (no silent .take(200)) and renders the
/// RTT-distribution + congestion charts.
#[test]
fn test_netlink_sockets_explorer_pagination_and_charts() {
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight::view::specialized::netlink_detail::{NetlinkDetailData, NetlinkDetailTopic};
    use zensight_common::{Protocol, SocketRecord};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Sockets;

    let socks: Vec<SocketRecord> = (0..3)
        .map(|i| SocketRecord {
            local: format!("10.0.0.1:{}", 1000 + i),
            remote: "1.1.1.1:443".into(),
            state: "established".into(),
            uid: 0,
            recv_q: 0,
            send_q: 0,
            rtt_us: 2_000 + i,
            retrans: 0,
            inode: 0,
            congestion: Some(if i % 2 == 0 { "cubic" } else { "bbr" }.into()),
            bbr_bw_bps: None,
            cc_min_rtt_us: None,
            snd_cwnd: 10,
            snd_buf: 0,
            rcv_buf: 0,
            delivery_rate: 0,
            pacing_rate: 0,
            bytes_retrans: 0,
            bytes_acked: 0,
            bytes_received: 0,
            bytes_sent: 0,
            total_retrans: 0,
            rcv_rtt_us: 0,
            lost: 0,
            reord_seen: 0,
            cookie: 0,
            cgroup_id: None,
            cgroup: None,
            pid: None,
            process: None,
            proc_start_time: None,
        })
        .collect();
    state.netlink_detail.apply(
        NetlinkDetailTopic::Sockets,
        Ok(NetlinkDetailData::Sockets(socks)),
    );
    // Force a small page cap so the "Show more" footer sits within the test
    // viewport (the default 200-row cap would push it off-screen).
    state.netlink_detail.sockets_table.limit = 2;

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("RTT distribution").is_ok());
    assert!(ui.find("Congestion control").is_ok());
    assert!(ui.find("showing 2 of 3 sockets").is_ok());
    // Load-more affordance emits NetlinkSocketsMore.
    let _ = ui.click("Show more");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::NetlinkSocketsMore))
    );
}

/// #269: the Sockets tab surfaces the eBPF section (connect-latency percentiles,
/// top-retransmit peers, tcplife connections) when the sensor's eBPF module
/// answered; it stays hidden on the unprivileged baseline.
#[test]
fn test_netlink_sockets_ebpf_section() {
    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netlink::netlink_host_view;
    use zensight::view::specialized::netlink_detail::{
        ConnRecord, NetlinkDetailData, NetlinkDetailTopic, RetransRecord,
    };
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = SpecializedTab::Sockets;
    // A socket aggregate so the Sockets tab renders its base content.
    state.update(TelemetryPoint::new(
        "gw01",
        Protocol::Netlink,
        "sockets/tcp/established",
        TelemetryValue::Gauge(3.0),
    ));

    // Baseline (no eBPF): the section is absent.
    {
        let mut ui = simulator(netlink_host_view(&state));
        assert!(ui.find("eBPF socket internals").is_err());
    }

    // eBPF active: connlat metrics + retransmits + connections.
    for (m, v) in [
        ("sockets/tcp/connlat_us_p50", TelemetryValue::Gauge(120.0)),
        ("sockets/tcp/connlat_us_p95", TelemetryValue::Gauge(950.0)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    state.netlink_detail.apply(
        NetlinkDetailTopic::Retransmits,
        Ok(NetlinkDetailData::Retransmits(vec![RetransRecord {
            peer: "203.0.113.9".into(),
            family: 2,
            count: 42,
        }])),
    );
    state.netlink_detail.apply(
        NetlinkDetailTopic::Connections,
        Ok(NetlinkDetailData::Connections(vec![ConnRecord {
            pid: 1234,
            comm: "curl".into(),
            family: 2,
            local: "10.0.0.1".into(),
            lport: 5555,
            remote: "1.1.1.1".into(),
            rport: 443,
            duration_ms: 3200,
            tx_bytes: 8000,
            rx_bytes: 90000,
            segs_out: 60,
            segs_in: 80,
            retrans: 1,
        }])),
    );

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("eBPF socket internals").is_ok());
    assert!(ui.find("Top retransmit peers").is_ok());
    assert!(ui.find("203.0.113.9").is_ok());
    assert!(ui.find("Recent connections (tcplife)").is_ok());
    assert!(ui.find("curl").is_ok());
}

/// The netring view shows the TLS section (with a fetched inventory) and the
/// Capture Health section when capture/* metrics are present.
#[test]
fn test_netring_tls_capture_sections() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue, TlsRecord};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        ("tls/handshakes_total", TelemetryValue::Counter(12)),
        ("tls/distinct_fingerprints", TelemetryValue::Gauge(3.0)),
        ("capture/0/packets", TelemetryValue::Counter(100000)),
        ("capture/0/drops", TelemetryValue::Counter(5)),
        ("capture/0/drop_rate", TelemetryValue::Gauge(0.0001)),
    ] {
        state.update(TelemetryPoint::new("wiretap1", Protocol::Netring, m, v));
    }
    // Pre-populate the fetched TLS inventory.
    state.netring_detail.apply_tls(Ok(vec![TlsRecord {
        sni: Some("api.example.com".into()),
        alpn: Some("h2".into()),
        ja3: None,
        ja4: Some("t13d1516h2_8daaf6152771_b186095e22b6".into()),
        count: 7,
        pq_key_share: true,
        ..Default::default()
    }]));

    // TLS is on the HTTP/TLS tab; Capture Health on the Capture tab (#247).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::HttpTls;
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("TLS").is_ok());
        assert!(ui.find("Fetch inventory").is_ok());
        assert!(ui.find("api.example.com").is_ok());
        // #326: post-quantum readiness stat + the per-fingerprint PQ badge.
        assert!(ui.find("PQ readiness (ratio)").is_ok());
        assert!(ui.find("PQ").is_ok());
    }
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Capture;
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Capture Health").is_ok());
    }
}

/// #326: once the netring sensor streams `dns/encrypted/*` aggregates, the DNS tab
/// grows an "Encrypted DNS" panel with the DoT/DoQ/DoH transport split and the
/// un-known-resolver (policy-bypass) count. Hidden until such a metric arrives.
#[test]
fn test_netring_encrypted_dns_panel() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Dns;

    // Before any encrypted-DNS metric, the panel is absent.
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Encrypted DNS").is_err());
    }

    for (m, v) in [
        ("dns/encrypted/doh", TelemetryValue::Counter(12)),
        ("dns/encrypted/dot", TelemetryValue::Counter(3)),
        ("dns/encrypted/unknown_resolver", TelemetryValue::Counter(4)),
    ] {
        state.update(TelemetryPoint::new("wiretap1", Protocol::Netring, m, v));
    }
    let mut ui = simulator(netring_sensor_view(&state, None));
    assert!(ui.find("Encrypted DNS").is_ok());
    assert!(ui.find("DoH").is_ok());
    assert!(ui.find("unknown resolver").is_ok());
}

/// #72: the netring view surfaces the QUIC SNI/ALPN and SSH/HASSH inventories —
/// rendered when the sensor publishes their aggregate counts — with on-demand
/// tables and fetch affordances wired to the right messages.
#[test]
fn test_netring_quic_ssh_sections() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, QuicRecord, SshRecord, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        ("quic/distinct_sni", TelemetryValue::Gauge(5.0)),
        ("ssh/distinct_hassh", TelemetryValue::Gauge(3.0)),
    ] {
        state.update(TelemetryPoint::new("wiretap1", Protocol::Netring, m, v));
    }
    state.netring_detail.apply_quic(Ok(vec![QuicRecord {
        sni: Some("cloudflare-quic.com".into()),
        alpn: vec!["h3".into()],
        version: "v1".into(),
        count: 9,
        ..Default::default()
    }]));
    state.netring_detail.apply_ssh(Ok(vec![SshRecord {
        hassh: "06046964c022c6407d15a27b12a51c5b".into(),
        role: "client".into(),
        banner: Some("SSH-2.0-OpenSSH_9.6".into()),
        count: 2,
        ..Default::default()
    }]));

    // QUIC/SSH inventories live on the HTTP/TLS tab (#247).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::HttpTls;
    let mut ui = simulator(netring_sensor_view(&state, None));
    assert!(ui.find("QUIC (SNI / ALPN)").is_ok());
    assert!(ui.find("cloudflare-quic.com").is_ok());
    assert!(ui.find("SSH (HASSH)").is_ok());
    assert!(ui.find("SSH-2.0-OpenSSH_9.6").is_ok());

    let _ = ui.click("Fetch QUIC");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(m, Message::FetchNetringQuic)));
}

/// #71: capture health surfaces the honest drop breakdown (AF_PACKET freezes,
/// AF_XDP ring causes) and raises an OVERLOAD badge once a source's windowed
/// drop_rate crosses the threshold.
#[test]
fn test_netring_capture_overload_and_breakdown() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        ("capture/0/packets", TelemetryValue::Counter(100000)),
        ("capture/0/drops", TelemetryValue::Counter(9000)),
        ("capture/0/drop_rate", TelemetryValue::Gauge(0.09)), // 9% → overload
        ("capture/0/freezes", TelemetryValue::Counter(4)),
        ("capture/0/xdp/rx_ring_full", TelemetryValue::Counter(120)),
    ] {
        state.update(TelemetryPoint::new("wiretap1", Protocol::Netring, m, v));
    }

    state.specialized_tab = zensight::view::specialized::SpecializedTab::Capture;
    let mut ui = simulator(netring_sensor_view(&state, None));
    assert!(ui.find("Capture Health").is_ok());
    assert!(ui.find("⚠ OVERLOAD — losing packets").is_ok());
    assert!(ui.find("xdp/rx_ring_full").is_ok());
}

/// #228/#224: the capture panel shows the resolved-backend badge and, when the
/// sensor is deliberately load-shedding, an unmistakable "data is sampled"
/// banner so the operator knows the rest of the telemetry is a sample.
#[test]
fn test_netring_capture_backend_and_shedding() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        (
            "capture/backend",
            TelemetryValue::Text("af_xdp".to_string()),
        ),
        ("capture/0/packets", TelemetryValue::Counter(500_000)),
        ("capture/0/drops", TelemetryValue::Counter(10)),
        ("capture/0/shed/active", TelemetryValue::Gauge(1.0)),
        (
            "capture/0/shed/new_flows_total",
            TelemetryValue::Counter(321),
        ),
        // capture/focus must not appear as a per-source row.
        ("capture/focus/packets", TelemetryValue::Counter(42)),
    ] {
        state.update(TelemetryPoint::new("wiretap1", Protocol::Netring, m, v));
    }

    state.specialized_tab = zensight::view::specialized::SpecializedTab::Capture;
    let mut ui = simulator(netring_sensor_view(&state, None));
    assert!(ui.find("Capture Health").is_ok());
    assert!(ui.find("backend: af_xdp").is_ok());
    assert!(ui.find("⚠ SHEDDING — data is sampled").is_ok());
    // The reloadable-filter counter is not mistaken for a NIC source row.
    assert!(ui.find("focus").is_err());
}

/// #228/#225: the capture-focus box in the detection-tuning panel sends a
/// `set_packet_filter` command (ApplyPacketFilter) and surfaces the live filter
/// + any sensor-side validation error inline.
#[test]
fn test_capture_focus_panel() {
    use zensight::view::detection_tuning::{
        CaptureFilterView, DetectionTuningState, detection_tuning_panel,
    };

    let mut state = DetectionTuningState {
        packet_filter_input: "host 10.0.0.5".to_string(),
        capture_filter: Some(CaptureFilterView {
            enabled: true,
            reloadable: 1,
            current: "host 10.0.0.5".to_string(),
            base: "tcp or udp or icmp".to_string(),
            last_error: Some("unexpected token foo".to_string()),
        }),
        ..Default::default()
    };
    state.loaded = false; // even unloaded, the focus card renders.

    {
        let mut ui = simulator(detection_tuning_panel(&state));
        assert!(ui.find("Capture Focus (netring)").is_ok());
        assert!(ui.find("current: host 10.0.0.5").is_ok());
        assert!(ui.find("✕ rejected: unexpected token foo").is_ok());
    }

    // Clicking Apply emits ApplyPacketFilter.
    let mut ui = simulator(detection_tuning_panel(&state));
    let _ = ui.click("Apply");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ApplyPacketFilter))
    );
}

/// #70: the netring view shows the passive asset-inventory section — the
/// streamed discovered-count and an on-demand table of MAC / hostname /
/// platform / seen-via, plus a "Fetch assets" affordance and a click wiring it
/// to the fetch message.
#[test]
fn test_netring_assets_section() {
    use zensight::message::Message;
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{AssetRecord, Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    state.update(TelemetryPoint::new(
        "wiretap1",
        Protocol::Netring,
        "assets/discovered",
        TelemetryValue::Gauge(2.0),
    ));
    state.netring_detail.apply_assets(Ok(vec![AssetRecord {
        mac: "aa:bb:cc:dd:ee:ff".into(),
        ipv4: vec!["10.0.0.5".into()],
        ipv6: vec![],
        hostname: Some("switch01".into()),
        vendor: None,
        platform: Some("cisco WS-C2960X".into()),
        capabilities: vec!["switch".into(), "bridge".into()],
        seen_via: vec!["lldp".into()],
        last_seen: 1_700_000_000_000,
        ..Default::default()
    }]));

    state.specialized_tab = zensight::view::specialized::SpecializedTab::Assets;
    let mut ui = simulator(netring_sensor_view(&state, None));
    assert!(ui.find("Assets (passive discovery)").is_ok());
    assert!(ui.find("switch01").is_ok());
    assert!(ui.find("cisco WS-C2960X").is_ok());

    // The fetch button is wired to the asset-fetch message.
    let _ = ui.click("Fetch assets");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::FetchNetringAssets))
    );
}

/// #247: the netring view is tabbed — always-on tabs render, capability-gated
/// tabs (DNS) appear only with their data, and clicking a tab emits the select
/// message that the app persists per device.
#[test]
fn test_netring_tabs_capability_and_switch() {
    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    // No dns/ metrics → DNS tab hidden; always-on tabs present.
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Overview").is_ok());
        assert!(ui.find("Talkers & Matrix").is_ok());
        assert!(ui.find("HTTP/TLS").is_ok());
        assert!(ui.find("DNS").is_err());
    }
    // Add a dns/ metric → the DNS tab becomes visible.
    state.update(TelemetryPoint::new(
        "wiretap1",
        Protocol::Netring,
        "dns/queries_total",
        TelemetryValue::Counter(1),
    ));
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("DNS").is_ok());
    }
    // Clicking a tab emits SelectSpecializedTab for this device.
    let mut ui = simulator(netring_sensor_view(&state, None));
    let _ = ui.click("Talkers & Matrix");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::SelectSpecializedTab(d, SpecializedTab::TalkersMatrix) if d.source == "wiretap1"
    )));
}

/// #253: firing netring anomalies surface in-view — an Overview strip that
/// click-throughs to the Security tab, and a Security tab that rolls them up by
/// detector, deep-links to the global Security view, and pivots to flows.
#[test]
fn test_netring_security_tab_and_strip() {
    use std::collections::HashMap;

    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Alert, AlertKind, AlertSeverity, AlertState, Protocol};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    state.netring_detail.anomalies = vec![Alert {
        timestamp: 0,
        source: "wiretap1".into(),
        protocol: Protocol::Netring,
        kind: AlertKind::Anomaly,
        rule: "RitaBeacon".into(),
        severity: AlertSeverity::Critical,
        state: AlertState::Firing,
        summary: "periodic beaconing to 1.2.3.4".into(),
        labels: HashMap::from([
            ("technique".to_string(), "T1071".to_string()),
            ("src".to_string(), "10.0.0.9".to_string()),
        ]),
    }];

    // Overview: the anomaly strip is present and clicks through to Security.
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Security").is_ok()); // tab visible with a badge
        let _ = ui.click("⚠ 1 anomaly · highest critical · T1071");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::SelectSpecializedTab(_, SpecializedTab::Security)
        )));
    }

    // Security tab: rollup + deep-link + flow pivot.
    state.specialized_tab = SpecializedTab::Security;
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("Anomalies (1)").is_ok());
        assert!(ui.find("RitaBeacon").is_ok());
        assert!(ui.find("periodic beaconing to 1.2.3.4").is_ok());
        assert!(ui.find("Open Security view").is_ok());
    }
    // The per-anomaly flow pivot targets the offending src.
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        let _ = ui.click("flows →");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::NetringPivotToFlows(_, ep) if ep == "10.0.0.9"
        )));
    }
}

/// #247/#248: Overview shows a capture-health chip; the Talkers & Matrix tab
/// renders the ranked-bar + sortable table from fetched data.
#[test]
fn test_netring_overview_chip_and_talkers_tab() {
    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TalkerRecord, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        ("flow/started_total", TelemetryValue::Counter(3)),
        ("capture/backend", TelemetryValue::Text("af_xdp".into())),
        ("capture/0/packets", TelemetryValue::Counter(1000)),
        ("capture/0/drop_rate", TelemetryValue::Gauge(0.0)),
    ] {
        state.update(TelemetryPoint::new("wiretap1", Protocol::Netring, m, v));
    }
    // Overview: capture-health chip present.
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("capture: af_xdp · drop 0.00%").is_ok());
    }

    // Talkers & Matrix tab: ranked bar + table render from fetched talkers.
    state.netring_detail.talkers =
        zensight::view::specialized::fetch::Fetch::Ready(vec![TalkerRecord {
            src: "10.0.0.42".into(),
            bytes_per_sec: 4096.0,
            names: Vec::new(),
        }]);
    state.specialized_tab = SpecializedTab::TalkersMatrix;
    {
        let mut ui = simulator(netring_sensor_view(&state, None));
        assert!(ui.find("10.0.0.42").is_ok());
        assert!(ui.find("showing 1 of 1 talkers").is_ok());
    }
}

/// The top-level Logs view renders buffered log lines (the message text and the
/// originating host) — verifying the unified logs feed surfaces journald/syslog.
#[test]
fn test_logs_view_renders_lines() {
    use zensight::view::specialized::{logs_view, syslog_message_from_point};
    use zensight_common::{TelemetryPoint, TelemetryValue};

    let point = TelemetryPoint {
        timestamp: 1_700_000_000_000,
        source: "host01".to_string(),
        protocol: Protocol::Logs,
        metric: "auth/crit".to_string(),
        value: TelemetryValue::Text("INTRUDER ALERT from 10.0.0.9".to_string()),
        labels: HashMap::new(),
        unit: None,
    };
    let messages = vec![syslog_message_from_point(&point, &point.source)];
    let filter = SyslogFilterState::default();

    let mut ui = simulator(logs_view(&messages, &filter));
    assert!(ui.find("Logs").is_ok());
    assert!(ui.find("INTRUDER ALERT from 10.0.0.9").is_ok());
    assert!(ui.find("host01").is_ok());
}

/// #64: with the filter panel open, journald units surface as toggle chips, and
/// clicking one emits ToggleSyslogUnit. The provenance badge ("journald") renders
/// in the stream.
#[test]
fn test_logs_unit_filter_and_source_badge() {
    use zensight::view::specialized::{logs_view, syslog_message_from_point};
    use zensight_common::{TelemetryPoint, TelemetryValue};

    let mut labels = HashMap::new();
    labels.insert("source_type".to_string(), "journald".to_string());
    labels.insert("sd.journald.unit".to_string(), "nginx.service".to_string());
    let point = TelemetryPoint {
        timestamp: 1_700_000_000_000,
        source: "host01".to_string(),
        protocol: Protocol::Logs,
        metric: "daemon/err".to_string(),
        value: TelemetryValue::Text("upstream timed out".to_string()),
        labels,
        unit: None,
    };
    let messages = vec![syslog_message_from_point(&point, &point.source)];

    let mut filter = SyslogFilterState::default();
    filter.panel_open = true;

    let mut ui = simulator(logs_view(&messages, &filter));
    // Provenance badge + the unit chip render.
    assert!(ui.find("journald").is_ok());
    assert!(ui.find("nginx.service").is_ok());

    // Clicking the unit chip toggles the unit filter.
    let _ = ui.click("nginx.service");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::ToggleSyslogUnit(u) if u == "nginx.service"))
    );
}

/// #64: the per-device logs view renders the derived rollup panel from the
/// sensor's `logs/*` metrics (#63).
#[test]
fn test_logs_rollup_panel_renders() {
    use zensight::view::device::DeviceDetailState;
    use zensight::view::specialized::{SyslogFilterState, syslog_event_view};
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Logs, "host01");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        ("errors_total", TelemetryValue::Counter(42)),
        ("warnings_total", TelemetryValue::Counter(7)),
        ("units_in_failure", TelemetryValue::Gauge(2.0)),
        (
            "by_unit/nginx.service/messages_total",
            TelemetryValue::Counter(900),
        ),
    ] {
        state.update(TelemetryPoint::new("host01", Protocol::Logs, m, v));
    }

    // Collapsed by default (#350): the header renders, the rollup detail
    // doesn't — the log stream is on screen without scrolling past stats.
    let filter = SyslogFilterState::default();
    let mut ui = simulator(syslog_event_view(&state, &filter, &[]));
    assert!(ui.find("Log statistics").is_ok());
    assert!(ui.find("errors (total)").is_err());
    // The caret toggle dispatches the panel message.
    let _ = ui.click("\u{25b8}");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::ToggleLogStatsPanel))
    );

    // Expanded: KPI tiles + the top-3 by-unit list appear.
    let open = SyslogFilterState {
        stats_open: true,
        ..SyslogFilterState::default()
    };
    let mut ui = simulator(syslog_event_view(&state, &open, &[]));
    assert!(ui.find("errors (total)").is_ok());
    assert!(ui.find("by unit (top)").is_ok());
}

/// #350: with more than 3 noisy units the rollup shows top-3 plus a
/// "Show all N" affordance that dispatches the expand message.
#[test]
fn test_logs_rollup_show_all_units() {
    use zensight::view::device::DeviceDetailState;
    use zensight::view::specialized::{SyslogFilterState, syslog_event_view};
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Logs, "host01");
    let mut state = DeviceDetailState::new(device_id);
    for (unit, n) in [
        ("nginx.service", 900),
        ("sshd.service", 800),
        ("cron.service", 700),
        ("kernel", 600),
        ("systemd-journald.service", 500),
    ] {
        state.update(TelemetryPoint::new(
            "host01",
            Protocol::Logs,
            format!("by_unit/{unit}/messages_total"),
            TelemetryValue::Counter(n),
        ));
    }

    let open = SyslogFilterState {
        stats_open: true,
        ..SyslogFilterState::default()
    };
    let mut ui = simulator(syslog_event_view(&state, &open, &[]));
    // Top-3 shown, the 4th is behind "Show all".
    assert!(ui.find("  nginx.service").is_ok());
    assert!(ui.find("  kernel").is_err());
    let _ = ui.click("Show all 5");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::ToggleLogStatsAllUnits))
    );

    // With the flag set, every unit renders and the toggle collapses back.
    let all = SyslogFilterState {
        stats_open: true,
        stats_all_units: true,
        ..SyslogFilterState::default()
    };
    let mut ui = simulator(syslog_event_view(&state, &all, &[]));
    assert!(ui.find("  kernel").is_ok());
    assert!(ui.find("Show top 3").is_ok());
}

/// The Logs view shows an explicit empty state when no logs have arrived yet
/// (so an empty feed reads as "waiting", not "broken").
#[test]
fn test_logs_view_empty_state() {
    use zensight::view::specialized::logs_view;

    let filter = SyslogFilterState::default();
    let mut ui = simulator(logs_view(&[], &filter));
    assert!(ui.find("No log messages received yet...").is_ok());
}

/// The nav rail's "Logs" entry drives Message::OpenLogs.
#[test]
fn test_nav_opens_logs() {
    use zensight::view::shell::app_shell;

    let inner = iced::widget::text("x");
    let mut ui = simulator(app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        None,
        0,
        None,
        inner.into(),
    ));
    let _ = ui.click("Logs");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(m, Message::OpenLogs)));
}

/// The nav rail's "Incidents" entry drives Message::OpenIncidents (#129).
#[test]
fn test_nav_opens_incidents() {
    use zensight::view::shell::app_shell;

    let inner = iced::widget::text("x");
    let mut ui = simulator(app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        None,
        0,
        None,
        inner.into(),
    ));
    let _ = ui.click("Incidents");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(m, Message::OpenIncidents)));
}

/// The nav rail's "Inventory" entry drives Message::OpenInventory (#120).
#[test]
fn test_nav_opens_inventory() {
    use zensight::view::shell::app_shell;

    let inner = iced::widget::text("x");
    let mut ui = simulator(app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        None,
        0,
        None,
        inner.into(),
    ));
    let _ = ui.click("Inventory");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(m, Message::OpenInventory)));
}

/// The inventory view renders the asset table (with the previously-hidden vendor)
/// and the unified fingerprint explorer; an SNI-bearing fingerprint exposes an
/// allowlist action (#120).
#[test]
fn test_inventory_view_renders_assets_and_fingerprints() {
    use zensight::view::inventory::{InventoryData, InventoryState, inventory_view};
    use zensight_common::{AssetRecord, Ja4hRecord, TlsRecord};

    let mut state = InventoryState::default();
    state.apply(Ok(InventoryData {
        assets: vec![AssetRecord {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            ipv4: vec!["10.0.0.5".into()],
            ipv6: vec![],
            hostname: Some("printer1".into()),
            vendor: Some("AcmeCorp".into()),
            platform: None,
            capabilities: vec!["router".into()],
            seen_via: vec!["lldp".into()],
            last_seen: 1,
            // #329 enrichment: role classification + fingerprint pivot + first-seen.
            role: "iot".into(),
            first_seen: 1,
            source_count: 3,
            ja4: Some("t13d1516h2_assetja4_11".into()),
            ..Default::default()
        }],
        tls: vec![TlsRecord {
            sni: Some("login.example".into()),
            alpn: Some("h2".into()),
            ja3: None,
            ja4: Some("t13d1516h2_abc_def".into()),
            count: 3,
            ..Default::default()
        }],
        quic: vec![],
        ssh: vec![],
        // Count kept below the TLS row's so the JA4 row stays first (the
        // allowlist-click assertion below targets the top fingerprint row).
        ja4h: vec![Ja4hRecord {
            ja4h: "ge11nn05enus_ff01_aa02".into(),
            host: Some("api.example".into()),
            method: Some("GET".into()),
            user_agent: Some("curl/8.5".into()),
            count: 2,
        }],
        assets_responded: true,
    }));

    let entities = zensight::entity::EntityStore::default();
    let mut ui = simulator(inventory_view(&state, &entities, 0));
    assert!(ui.find("Inventory").is_ok());
    assert!(ui.find("AcmeCorp").is_ok(), "vendor must be rendered");
    assert!(ui.find("printer1").is_ok());
    // #329: the classified role renders as a filter chip (and a table cell), and
    // the asset's JA4 fingerprint pivot shows in the assets table.
    assert!(ui.find("iot").is_ok(), "role chip/cell must render");
    assert!(
        ui.find("t13d1516h2_assetja4_11").is_ok(),
        "asset fingerprint pivot"
    );
    assert!(ui.find("t13d1516h2_abc_def").is_ok());
    assert!(
        ui.find("ge11nn05enus_ff01_aa02").is_ok(),
        "JA4H fingerprint row must render"
    );
    // The SNI-bearing JA4 row offers an allowlist action.
    let _ = ui.click("allowlist");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::AddNetringAllowlistEntry(h) if h == "login.example"))
    );
}

/// Drilling into a syslog device shows the host's recent log *stream* from the
/// buffer — not just the latest line per facility/severity. Two messages with
/// the SAME metric must BOTH appear (the old metrics-map view kept only one).
#[test]
fn test_syslog_device_shows_host_history() {
    use zensight::view::specialized::syslog_message_from_point;
    use zensight_common::{TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::fixture(Protocol::Logs, "host9".to_string());
    let state = DeviceDetailState::new(device_id);

    let mk = |msg: &str| {
        let p = TelemetryPoint {
            timestamp: 1,
            source: "host9".to_string(),
            protocol: Protocol::Logs,
            metric: "daemon/err".to_string(),
            value: TelemetryValue::Text(msg.to_string()),
            labels: HashMap::new(),
            unit: None,
        };
        syslog_message_from_point(&p, "host9")
    };
    let host_logs = vec![mk("FIRST LINE alpha"), mk("SECOND LINE bravo")];

    let filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &filter, &host_logs));
    assert!(ui.find("host9").is_ok());
    assert!(ui.find("FIRST LINE alpha").is_ok());
    assert!(ui.find("SECOND LINE bravo").is_ok());
}

/// #281: the systemd specialized view renders its tab strip + overview, and
/// clicking a tab emits SelectSpecializedTab.
#[test]
fn test_systemd_specialized_view_tabs() {
    use zensight::view::specialized::{SpecializedTab, specialized_view};
    use zensight_common::{TelemetryPoint, TelemetryValue};

    let id = DeviceId::fixture(Protocol::Systemd, "server01".to_string());
    let mut state = DeviceDetailState::new(id.clone());
    for (metric, v) in [
        ("units/total", 300.0),
        ("units/active", 280.0),
        ("units/failed", 2.0),
        ("manager/n_failed_units", 2.0),
        ("boot/firmware_usec", 5_000_000.0),
        ("boot/userspace_usec", 12_000_000.0),
    ] {
        state.update(TelemetryPoint::new(
            "server01",
            Protocol::Systemd,
            metric,
            TelemetryValue::Gauge(v),
        ));
    }

    let view = specialized_view(&state, None).expect("systemd specialized view");
    let mut ui = simulator(view);

    // Tab strip + overview content.
    assert!(ui.find("Overview").is_ok());
    assert!(ui.find("Units").is_ok());
    assert!(ui.find("Timers").is_ok());
    assert!(ui.find("Sentinel").is_ok());
    assert!(ui.find("cgroups").is_ok());
    assert!(ui.find("System state").is_ok());
    assert!(ui.find("Boot performance").is_ok());

    // Clicking Units emits a tab-select for this device.
    let _ = ui.click("Units");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(
        m,
        Message::SelectSpecializedTab(d, SpecializedTab::Units) if d.protocol == Protocol::Systemd
    )));
}

/// #281: the systemd Units tab shows an on-demand load affordance emitting a fetch.
#[test]
fn test_systemd_units_tab_fetches_on_demand() {
    use zensight::view::specialized::specialized_view;
    use zensight::view::specialized::systemd_detail::SystemdDetailTopic;

    let id = DeviceId::fixture(Protocol::Systemd, "server01".to_string());
    let mut state = DeviceDetailState::new(id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Units;

    let view = specialized_view(&state, None).expect("systemd view");
    let mut ui = simulator(view);
    // Idle fetch panel offers a Load button.
    let _ = ui.click("Load");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::FetchSystemdDetail(SystemdDetailTopic::Units)))
    );
}

/// #283: Units-tab service controls use an inline two-step confirm — "start"
/// arms the action, then "confirm" emits SystemdUnitActionConfirm (which the app
/// sends to `@rpc/systemd/action/set`); "cancel" backs out.
#[test]
fn test_systemd_units_action_confirm_flow() {
    use zensight::view::specialized::specialized_view;
    use zensight::view::specialized::systemd_detail::{SystemdDetailData, SystemdDetailTopic};
    use zensight_common::query_detail::UnitRecord;

    let id = DeviceId::fixture(Protocol::Systemd, "server01".to_string());
    let mut state = DeviceDetailState::new(id);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Units;
    state.systemd_detail.apply(
        SystemdDetailTopic::Units,
        Ok(SystemdDetailData::Units(vec![UnitRecord {
            name: "nginx.service".into(),
            description: "web server".into(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            job: None,
        }])),
    );

    // Step 1: the row offers start/stop/restart; clicking "start" arms it.
    let mut ui = simulator(specialized_view(&state, None).expect("systemd view"));
    assert!(ui.find("restart").is_ok());
    let _ = ui.click("start");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(
        m,
        Message::SystemdUnitActionArm { verb, unit }
            if verb == "start" && unit == "nginx.service"
    )));

    // Step 2: with the action armed, the row swaps to confirm/cancel and
    // "confirm" emits the send.
    state.systemd_detail.pending_action = Some(("start".to_string(), "nginx.service".to_string()));
    let mut ui = simulator(specialized_view(&state, None).expect("systemd view"));
    assert!(ui.find("start?").is_ok());
    let _ = ui.click("confirm");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::SystemdUnitActionConfirm))
    );

    // Cancel path emits the disarm.
    let mut ui = simulator(specialized_view(&state, None).expect("systemd view"));
    let _ = ui.click("cancel");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::SystemdUnitActionCancel))
    );
}

/// #278: the expectations view can target the systemd sentinel — the systemd form
/// renders and "Add & Push" emits AddExpectation.
#[test]
fn test_systemd_expectations_authoring() {
    use zensight::view::expectations::{ExpTarget, ExpectationsState, expectations_view};

    let mut state = ExpectationsState::default();
    state.target = ExpTarget::Systemd;
    state.new_name = "sshd.service".to_string();

    let mut ui = simulator(expectations_view(&state));
    // Systemd-flavoured header + form.
    assert!(ui.find("Expectations (systemd sentinel)").is_ok());
    assert!(ui.find("Declare a systemd expectation").is_ok());

    let _ = ui.click("Add & Push");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::AddExpectation))
    );
}

/// #278: a systemd draft with entries shows them in the Configured list.
#[test]
fn test_systemd_expectations_configured_list() {
    use zensight::view::expectations::{ExpTarget, ExpectationsState, expectations_view};

    let mut state = ExpectationsState::default();
    state.target = ExpTarget::Systemd;
    state.systemd.services.push("sshd.service".to_string());
    state.systemd.forbid_failed = true;

    let mut ui = simulator(expectations_view(&state));
    assert!(ui.find("service:sshd.service").is_ok());
    assert!(ui.find("forbid:failed").is_ok());
}

// ---------------------------------------------------------------------------
// #306 — Host entity: dashboard grouping, degraded parity, resolution group.
// ---------------------------------------------------------------------------

use zensight::entity::EntityStore;
use zensight_common::{DeviceStatus, HostEntity, MemberClaim};

/// Build a test HostEntity merging the given `(sensor, source)` members.
fn test_entity(id: &str, hostname: &str, members: &[(&str, &str)]) -> HostEntity {
    HostEntity {
        entity_id: id.to_string(),
        aliases: vec![],
        host_id: None,
        boot_id: None,
        ips: vec!["10.0.0.5".to_string()],
        macs: vec![],
        container_ids: vec![],
        hostname: Some(hostname.to_string()),
        fqdn: None,
        names: vec![],
        vendor: None,
        platform: Some("linux".to_string()),
        members: members
            .iter()
            .map(|(sensor, source)| MemberClaim {
                sensor: (*sensor).to_string(),
                source: (*source).to_string(),
                rule: "machine-id".to_string(),
                confidence: 1.0,
                last_seen: 0,
            })
            .collect(),
        status: Some("online".to_string()),
        last_updated: i64::MAX / 2, // never stale in-test
    }
}

fn device(protocol: Protocol, source: &str) -> (DeviceId, DeviceState) {
    let id = DeviceId::fixture(protocol, source);
    let mut d = DeviceState::new(id.clone());
    d.metric_count = 3;
    d.is_healthy = true;
    d.update_from_liveness(DeviceStatus::Online, 0, None);
    (id, d)
}

/// #306: two sensor sources merged by one entity render as a single host card
/// with a "merged from 2 sources" caption.
#[test]
fn test_host_card_renders_entity_members() {
    let mut state = DashboardState::default();
    for (id, d) in [
        device(Protocol::Sysinfo, "srvA"),
        device(Protocol::Netlink, "srvB"),
    ] {
        state.devices.insert(id, d);
    }

    let mut entities = EntityStore::default();
    entities.upsert(test_entity(
        "h_web01",
        "web-01",
        &[("sysinfo", "srvA"), ("netlink", "srvB")],
    ));

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // One merged host card named by the entity hostname, both facet chips.
    assert!(ui.find("web-01").is_ok());
    assert!(ui.find("merged from 2 sources").is_ok());
    assert!(ui.find("sysinfo").is_ok());
    assert!(ui.find("netlink").is_ok());
}

/// #306: with an empty EntityStore, grouping falls back to per-source exactly as
/// before correlation existed — correlation is never a hard render dependency.
#[test]
fn test_dashboard_empty_entity_store_degraded_parity() {
    let mut state = DashboardState::default();
    for (id, d) in [
        device(Protocol::Sysinfo, "srvA"),
        device(Protocol::Netlink, "srvB"),
    ] {
        state.devices.insert(id, d);
    }

    let entities = EntityStore::default(); // empty ⇒ degraded path
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let firing: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
        &entities,
        &firing,
        true,
    ));

    // Two separate source-grouped cards, no merge caption.
    assert!(ui.find("srvA").is_ok());
    assert!(ui.find("srvB").is_ok());
    assert!(ui.find("merged from 2 sources").is_err());
}

/// #306: the host detail page shows the resolution-group drill-down with each
/// member's rule + confidence — the wrong-merge diagnosis affordance.
#[test]
fn test_host_detail_resolution_group() {
    let id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut detail = DeviceDetailState::new(id.clone());
    for point in mock::sysinfo::host("server01") {
        detail.update(point);
    }
    let facets = vec![FacetTab::live(id.clone(), DeviceStatus::Online, true)];
    let entity = test_entity(
        "h_web01",
        "web-01",
        &[("sysinfo", "server01"), ("netlink", "server01")],
    );

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &detail,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: Some(&entity),
        identity_expanded: true,
        artifact: None,
    }));

    assert!(ui.find("Resolution group").is_ok());
    assert!(ui.find("web-01").is_ok());
    // Member row shows the binding rule + confidence.
    assert!(
        ui.find("sysinfo/server01 · machine-id · confidence 1.00")
            .is_ok()
    );
}

/// #306: host-detail facet tabs include entity members that have no live
/// DeviceState (union), shown as disabled tabs — here a syslog member without a
/// live device still surfaces via the entity.
#[test]
fn test_host_detail_entity_facet_tabs() {
    let id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut detail = DeviceDetailState::new(id.clone());
    for point in mock::sysinfo::host("server01") {
        detail.update(point);
    }
    // Two facet tabs built from the entity: the live sysinfo one + a netlink
    // member (disabled, no live device).
    let facets = vec![
        FacetTab::live(id.clone(), DeviceStatus::Online, true),
        FacetTab {
            id: None,
            source: "server01".to_string(),
            protocol: Protocol::Netlink,
            status: DeviceStatus::Unknown,
            active: false,
        },
    ];
    let entity = test_entity(
        "h_srv01",
        "server01",
        &[("sysinfo", "server01"), ("netlink", "server01")],
    );

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &detail,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: Some(&entity),
        identity_expanded: true,
        artifact: None,
    }));

    assert!(ui.find("Facets").is_ok());
    assert!(ui.find("sysinfo").is_ok());
    assert!(ui.find("netlink").is_ok());
}

/// #350: the identity details are collapsed by default — one summary line in
/// the nav bar, no fact/member rows — and the toggle dispatches
/// `ToggleIdentityDetails`. Expanding shows everything (no data loss).
#[test]
fn test_host_identity_collapsed_by_default() {
    let id = DeviceId::fixture(Protocol::Sysinfo, "server01".to_string());
    let mut detail = DeviceDetailState::new(id.clone());
    for point in mock::sysinfo::host("server01") {
        detail.update(point);
    }
    let facets = vec![FacetTab::live(
        id,
        zensight_common::DeviceStatus::Online,
        true,
    )];
    let entity = test_entity(
        "h_web01",
        "web-01",
        &[("sysinfo", "server01"), ("netlink", "server01")],
    );

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &detail,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: Some(&entity),
        identity_expanded: false,
        artifact: None,
    }));

    // Summary present, details hidden.
    assert!(ui.find("web-01").is_ok());
    assert!(ui.find("2 sources \u{b7} 1 IPs").is_ok());
    assert!(ui.find("Resolution group").is_err());

    // The identity toggle dispatches the (persisted) expand message.
    let _ = ui.click("identity");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Message::ToggleIdentityDetails))
    );
}

/// #350: the syslog drill-down renders exactly one navigation layer — the
/// facet body carries no Back button of its own (the shared nav bar owns it).
#[test]
fn test_syslog_drilldown_single_back() {
    use zensight::view::specialized::syslog_event_view;

    let id = DeviceId::fixture(Protocol::Logs, "server01".to_string());
    let detail = DeviceDetailState::new(id.clone());
    let syslog_filter = SyslogFilterState::default();

    // The facet body alone: no Back button.
    let mut body = simulator(syslog_event_view(&detail, &syslog_filter, &[]));
    assert!(body.find("Back").is_err());

    // The host shell around it: exactly one nav layer with the Back button.
    let facets = vec![FacetTab::live(
        id,
        zensight_common::DeviceStatus::Online,
        true,
    )];
    let mut ui = simulator(host_detail_view(DeviceViewCtx {
        state: &detail,
        syslog_filter: &syslog_filter,
        host_logs: &[],
        facets: &facets,
        entity: None,
        identity_expanded: false,
        artifact: None,
    }));
    assert!(ui.find("Back").is_ok());
    let _ = ui.click("Back");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(m, Message::ClearSelection)));
}

// ---------------------------------------------------------------------------
// On-demand capture form (#333)
// ---------------------------------------------------------------------------

use zensight::view::artifact_fetch::{ArtifactFetch, CaptureForm, artifact_section};
use zensight_common::{KindAdvert, KindStatus};

fn capture_kinds(max_duration_secs: u32, filter_allowed: bool) -> Vec<KindStatus> {
    vec![KindStatus {
        kind: "capture".into(),
        busy: false,
        current: None,
        max_bytes: 256 * 1024 * 1024,
        cooldown_secs: 60,
        advert: KindAdvert::Capture {
            max_duration_secs,
            filter_allowed,
            snaplen_max: 0,
        },
    }]
}

/// #351 acceptance: the netring Capture tab hosts the real capture form when
/// the sensor advertises the Capture kind — an operator starts a pcap without
/// leaving the device view.
#[test]
fn netring_capture_tab_hosts_capture_form() {
    use std::collections::HashMap;
    use zensight::view::artifact_fetch::ArtifactCtx;
    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::ArtifactKind;

    let device_id = DeviceId::fixture(Protocol::Netring, "host01");
    let mut state = DeviceDetailState::new(device_id.clone());
    state.specialized_tab = SpecializedTab::Capture;
    // A capture/ metric so the tab is visible even without the advert path.
    state.update(zensight_common::TelemetryPoint::new(
        "host01",
        Protocol::Netring,
        "capture/eth0/packets",
        zensight_common::TelemetryValue::Counter(10),
    ));

    let kinds_map: HashMap<String, Vec<KindStatus>> =
        HashMap::from([("netring".to_string(), capture_kinds(300, true))]);
    let forms: HashMap<String, CaptureForm> =
        HashMap::from([("netring".to_string(), CaptureForm::default())]);
    let ctx = ArtifactCtx {
        fetch: &ArtifactFetch::Idle,
        kinds: &kinds_map,
        capture_forms: &forms,
        active_prefix: None,
        active_kind: None,
    };

    let mut ui = simulator(netring_sensor_view(&state, Some(ctx)));
    assert!(ui.find("On-demand packet capture").is_ok());
    let _ = ui.click("Start capture");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(
        m,
        Message::StartArtifact { producer, kind: ArtifactKind::Capture { .. }, .. }
            if producer == "netring"
    )));
}

/// #351: without a Capture advert the tab stays health-only with an honest
/// caption (no dead form).
#[test]
fn netring_capture_tab_without_advert_is_health_only() {
    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netring::netring_sensor_view;

    let device_id = DeviceId::fixture(Protocol::Netring, "host01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = SpecializedTab::Capture;
    state.update(zensight_common::TelemetryPoint::new(
        "host01",
        Protocol::Netring,
        "capture/eth0/packets",
        zensight_common::TelemetryValue::Counter(10),
    ));

    let mut ui = simulator(netring_sensor_view(&state, None));
    assert!(ui.find("Capture Health").is_ok());
    assert!(ui.find("Start capture").is_err());
    assert!(
        ui.find(
            "Live capture health. This sensor does not advertise on-demand captures \
             (enable `artifacts.capture` in its config)."
        )
        .is_ok()
    );
}

/// #351: the netring Bandwidth tab pivots to the global monitor pre-scoped to
/// this host, and the monitor's chip clears the scope.
#[test]
fn netring_bandwidth_pivot_and_chip_clear() {
    use zensight::view::bandwidth::{BandwidthState, bandwidth_view};
    use zensight::view::specialized::SpecializedTab;
    use zensight::view::specialized::netring::netring_sensor_view;

    let device_id = DeviceId::fixture(Protocol::Netring, "host01");
    let mut state = DeviceDetailState::new(device_id);
    state.specialized_tab = SpecializedTab::Bandwidth;
    state.update(zensight_common::TelemetryPoint::new(
        "host01",
        Protocol::Netring,
        "bandwidth/https/bytes_per_sec",
        zensight_common::TelemetryValue::Gauge(1000.0),
    ));

    let mut ui = simulator(netring_sensor_view(&state, None));
    let _ = ui.click("Open in Bandwidth monitor");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::OpenBandwidthForHost(h) if h == "host01"))
    );

    // The scoped monitor shows the chip; clearing dispatches the reset.
    let bw = BandwidthState {
        host_filter: Some("host01".to_string()),
        ..BandwidthState::default()
    };
    let mut ui = simulator(bandwidth_view(&bw));
    assert!(ui.find("Host: host01").is_ok());
    let _ = ui.click("clear");
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::ClearBandwidthHostFilter))
    );
}

#[test]
fn capture_form_renders_from_advert() {
    let kinds = capture_kinds(300, true);
    let form = CaptureForm::default();
    let mut ui = simulator(artifact_section(
        &ArtifactFetch::Idle,
        "netring",
        None,
        &kinds,
        None,
        None,
        Some(&form),
    ));
    assert!(ui.find("On-demand packet capture").is_ok());
    assert!(ui.find("Start capture").is_ok());
}

#[test]
fn capture_form_over_cap_duration_blocks_submit() {
    let kinds = capture_kinds(300, true);
    let form = CaptureForm {
        duration_secs: "99999".into(), // over the 300s advert max
        ..CaptureForm::default()
    };
    let mut ui = simulator(artifact_section(
        &ArtifactFetch::Idle,
        "netring",
        None,
        &kinds,
        None,
        None,
        Some(&form),
    ));
    // Inline validation error is shown …
    assert!(ui.find("duration exceeds the sensor max (300s)").is_ok());
    // … and the disabled submit emits no StartArtifact message.
    let _ = ui.click("Start capture");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, Message::StartArtifact { .. }))
    );
}

#[test]
fn capture_generating_progress_line_renders() {
    let kinds = capture_kinds(300, true);
    let form = CaptureForm::default();
    let fetch = ArtifactFetch::Generating {
        detail: Some("capturing 12s/30s · 3.1 MiB · 42 pkts".into()),
        progress: Some(0.4),
    };
    let mut ui = simulator(artifact_section(
        &fetch,
        "netring",
        None,
        &kinds,
        Some("netring"),
        Some("capture"),
        Some(&form),
    ));
    assert!(ui.find("capturing 12s/30s · 3.1 MiB · 42 pkts").is_ok());
}

#[test]
fn no_capture_advert_renders_no_form() {
    // A sensor advertising only a report kind shows no capture form.
    let kinds = vec![KindStatus {
        kind: "report".into(),
        busy: false,
        current: None,
        max_bytes: 1024,
        cooldown_secs: 30,
        advert: KindAdvert::Report {},
    }];
    let form = CaptureForm::default();
    let mut ui = simulator(artifact_section(
        &ArtifactFetch::Idle,
        "netlink",
        None,
        &kinds,
        None,
        None,
        Some(&form),
    ));
    assert!(ui.find("Start capture").is_err());
    assert!(ui.find("Download debug report").is_ok());
}

/// #408: the parallax view renders the catalogue with Open/Close controls,
/// clicking Open/Close dispatches the tile messages, an open tile renders a
/// waiting caption, and the empty state shows the no-previews placeholder.
#[test]
fn test_parallax_catalogue_and_tiles() {
    use zensight::view::specialized::parallax::parallax_view;

    let device_id = DeviceId::fixture(Protocol::Parallax, "hostA".to_string());
    let mut state = DeviceDetailState::new(device_id);

    // Idle catalogue → load affordance + empty-tiles placeholder.
    {
        let mut ui = simulator(parallax_view(&state));
        assert!(ui.find("Live media — hostA").is_ok());
        assert!(ui.find("Load streams").is_ok());
        assert!(
            ui.find("No previews open — open a stream above to watch its live preview.")
                .is_ok()
        );
    }

    // Ready catalogue renders one row per stream with its native geometry and
    // active badge. Per-tier Live buttons only render on `--features h264`
    // builds (this is a default build), so the tier-open click is asserted in
    // the h264 unit test `catalogue_row_opens_the_clicked_tier`.
    state.parallax_detail.apply(Ok(mock::parallax::streams()));
    {
        let mut ui = simulator(parallax_view(&state));
        assert!(ui.find("video0").is_ok());
        assert!(ui.find("door").is_ok());
        assert!(ui.find("test pattern smpte").is_ok());
        // Capability-bearing catalogue (#507): native geometry renders as a cell.
        assert!(ui.find("640×360").is_ok(), "native geometry cell");
        assert!(ui.find("live").is_ok(), "door is advertised active");
    }

    // An open tile renders its waiting caption; Close dispatches
    // ParallaxCloseTile.
    let generation = state.parallax_detail.allocate_generation();
    state
        .parallax_detail
        .open_tile("video0", generation, None, false, None);
    {
        let mut ui = simulator(parallax_view(&state));
        assert!(ui.find("video0 · waiting for frames…").is_ok());
        let _ = ui.click("Close");
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::ParallaxCloseTile { .. })),
            "clicking Close must dispatch ParallaxCloseTile"
        );
    }

    // A tile whose stream ended shows the reason instead of a frame.
    state
        .parallax_detail
        .end_tile("video0", generation, Some("stream ended".into()));
    let mut ui = simulator(parallax_view(&state));
    assert!(ui.find("video0 — stream ended").is_ok());
}

// ===========================================================================
// Tier-2 conversion regression net (#475).
//
// These views were switched from positional string-splitting to the registry's
// typed parse direction. The failure mode of that conversion is NOT a panic —
// it is a table that silently renders zero rows, because `parse_metric` returned
// `None` and the loop body never ran. Several of these sections still gate their
// *header* on a string prefix (`has_temperatures`, `has_disk_io`, the neighbor
// donut), so the title appears either way and a title-only assertion proves
// nothing.
//
// Every test below therefore feeds REAL registered subjects and asserts on a
// rendered ROW. If a registry pattern moves and a consumer is not updated with
// it, one of these goes red instead of a view going quietly blank in production.
// ===========================================================================

/// Build a sysinfo telemetry point with a registered subject.
fn sysinfo_point(
    metric: &str,
    value: zensight_common::TelemetryValue,
) -> zensight_common::TelemetryPoint {
    zensight_common::TelemetryPoint {
        timestamp: 0,
        source: "server01".to_string(),
        protocol: Protocol::Sysinfo,
        metric: metric.to_string(),
        value,
        labels: HashMap::new(),
        unit: None,
    }
}

fn sysinfo_state(points: &[(&str, zensight_common::TelemetryValue)]) -> DeviceDetailState {
    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
    for (metric, value) in points {
        state.update(sysinfo_point(metric, value.clone()));
    }
    state
}

/// A labelled telemetry point for [`sysinfo_state_labeled`]: `(metric, value,
/// labels)`.
type LabeledPoint<'a> = (
    &'a str,
    zensight_common::TelemetryValue,
    &'a [(&'a str, &'a str)],
);

/// `sysinfo_state`'s sibling for the views that read a *label* off the point
/// rather than the key — the RAPL rows prefer the `name` label ("package-0")
/// over the key's sanitized zone ("intel-rapl_0").
fn sysinfo_state_labeled(points: &[LabeledPoint<'_>]) -> DeviceDetailState {
    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
    for (metric, value, labels) in points {
        let mut point = sysinfo_point(metric, value.clone());
        point.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        state.update(point);
    }
    state
}

/// `disk/{mount}/used` + `/total` must produce a mount row. The mount is a slug
/// (`/home` → `home`, `/` → `_`), and the row only renders when BOTH keys parse.
#[test]
fn tier2_sysinfo_disk_rows_render() {
    let state = sysinfo_state(&[
        (
            "disk/home/used",
            zensight_common::TelemetryValue::Gauge(30_000_000_000.0),
        ),
        (
            "disk/home/total",
            zensight_common::TelemetryValue::Gauge(100_000_000_000.0),
        ),
    ]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(
        ui.find("home").is_ok(),
        "the disk section must render a row for the `home` mount, not just its header"
    );
}

/// `network/{iface}/rx_bytes` must produce an interface row — and the sibling
/// `network/tcp/*` / `network/sockets/*` literal families must NOT be mistaken
/// for interfaces named `tcp`/`sockets`. That confusion is exactly what the
/// typed parse exists to prevent, and nothing pinned it until now.
#[test]
fn tier2_sysinfo_network_rows_render_and_do_not_invent_an_iface() {
    let state = sysinfo_state(&[
        (
            "network/eth0/rx_bytes",
            zensight_common::TelemetryValue::Counter(1_000_000),
        ),
        (
            "network/eth0/tx_bytes",
            zensight_common::TelemetryValue::Counter(2_000_000),
        ),
        // Literal-headed siblings — a positional parse would read chunk 1 as an
        // interface name and invent an iface called "tcp".
        (
            "network/tcp/retrans_segs_total",
            zensight_common::TelemetryValue::Counter(12),
        ),
        (
            "network/sockets/tcp_inuse",
            zensight_common::TelemetryValue::Gauge(40.0),
        ),
    ]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(ui.find("eth0").is_ok(), "eth0 must render as an interface");

    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(
        ui.find("network/tcp").is_err(),
        "the `network/tcp/*` literal family must not surface as an interface"
    );
}

/// `disk/{device}/io/read_rate` must produce a device row. `has_disk_io()` gates
/// the header on a string prefix, so the header alone proves nothing.
#[test]
fn tier2_sysinfo_disk_io_rows_render() {
    let state = sysinfo_state(&[
        (
            "disk/sda/io/read_rate",
            zensight_common::TelemetryValue::Gauge(1_048_576.0),
        ),
        (
            "disk/sda/io/write_rate",
            zensight_common::TelemetryValue::Gauge(524_288.0),
        ),
    ]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(
        ui.find("sda").is_ok(),
        "the Disk I/O section must render the `sda` device row, not an empty section"
    );
}

/// `sensors/{chip}/{label}/temp` — two variables — must produce a sensor row.
/// If the two-var parse breaks, this section renders its title with zero rows.
#[test]
fn tier2_sysinfo_temperature_rows_render() {
    let state = sysinfo_state(&[(
        "sensors/coretemp/core_0/temp",
        zensight_common::TelemetryValue::Gauge(45.0),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    // The row is `{chip}/{label}` — both variables named by the registry rather
    // than read off parts[1]/parts[2].
    assert!(
        ui.find("coretemp/core_0").is_ok(),
        "the Temperatures section must render the chip/label row, not an empty section"
    );
}

// ===========================================================================
// Fans & power panel (#515).
//
// The sensor has published fan RPM, battery and RAPL watts since the v1
// keyspace; nothing rendered them until this panel. Two renderings here are
// easy to get wrong in opposite directions, and both are pinned below: a fan
// reading 0 is REAL DATA that must not be hidden, and absent RAPL watts are NOT
// a reading of zero.
//
// Note `Simulator::find` matches a text widget's content EXACTLY (iced_selector
// `content == *self`), so `find("0 RPM")` cannot be satisfied by "1200 RPM" —
// these assertions are sharp, provided each value stays its own text widget.
// ===========================================================================

/// A fan reading 0 RPM is a reading, not a gap: laptops stop their fans at idle,
/// and the collector publishes the zero deliberately (it used to drop it, which
/// made "idle" look like "dead"). A fan pinned at 0 *under load* is a dead fan —
/// the interesting case — so the panel must show the zero and must not let it
/// read as "no fan".
#[test]
fn sysinfo_fan_at_zero_rpm_renders_as_a_reading_not_absence() {
    let state = sysinfo_state(&[(
        "sensors/dell_ddv/cpu_fan/rpm",
        zensight_common::TelemetryValue::Gauge(0.0),
    )]);

    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(
        ui.find("dell_ddv/cpu_fan").is_ok(),
        "a fan at 0 RPM must still render its chip/label row"
    );
    assert!(
        ui.find("0 RPM").is_ok(),
        "0 RPM must render as a reading, not be hidden or shown as '-'"
    );
    assert!(
        ui.find("No fans reported by hwmon").is_err(),
        "a fan reporting 0 is not the same as no fan being reported"
    );
}

/// "The sensor cannot read this" and "the reading is zero" are different facts.
/// RAPL watts are usually absent — `energy_uj` is root-only (CVE-2020-8694) —
/// so the panel must say so rather than invent a 0 W measurement. Pinned in both
/// directions: the note must not appear when watts really are 0.0.
#[test]
fn sysinfo_rapl_absent_is_distinct_from_zero_watts() {
    // (a) Power collector running (fans present), but no RAPL zones reporting.
    let absent = sysinfo_state(&[(
        "sensors/dell_ddv/cpu_fan/rpm",
        zensight_common::TelemetryValue::Gauge(3000.0),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &absent,
    ));
    assert!(
        ui.find(
            "No RAPL zones are reporting. Either this CPU exposes none, or the sensor cannot \
             read them (/sys/class/powercap/*/energy_uj is root-only since CVE-2020-8694), or \
             it has just started and needs two readings to derive a rate. This is not a reading \
             of zero watts."
        )
        .is_ok(),
        "absent watts must be explained, not rendered as a measurement"
    );
    assert!(
        ui.find("0.0 W").is_err(),
        "absent watts must never be shown as 0.0 W — that is inventing a reading"
    );

    // (b) A zone genuinely reporting 0.0 W is a measurement, not an absence.
    let zero = sysinfo_state(&[(
        "power/rapl/intel-rapl_0/watts",
        zensight_common::TelemetryValue::Gauge(0.0),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &zero,
    ));
    assert!(
        ui.find("0.0 W").is_ok(),
        "a zone reporting 0.0 W must render the reading"
    );
    assert!(
        ui.find(
            "No RAPL zones are reporting. Either this CPU exposes none, or the sensor cannot \
             read them (/sys/class/powercap/*/energy_uj is root-only since CVE-2020-8694), or \
             it has just started and needs two readings to derive a rate. This is not a reading \
             of zero watts."
        )
        .is_err(),
        "a real 0.0 W reading must not be reported as 'cannot read'"
    );
}

/// The zone's friendly name (`package-0`) rides as a label; the key carries the
/// sanitized zone (`intel-rapl_0` — the colon is grammar-illegal). Prefer the
/// label, fall back to the key, so the row is meaningful on either path.
#[test]
fn sysinfo_rapl_zone_prefers_name_label_over_raw_zone() {
    let labeled = sysinfo_state_labeled(&[(
        "power/rapl/intel-rapl_0/watts",
        zensight_common::TelemetryValue::Gauge(14.2),
        &[("zone", "intel-rapl:0"), ("name", "package-0")],
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &labeled,
    ));
    assert!(
        ui.find("package-0").is_ok(),
        "the friendly zone name must be preferred when the label is present"
    );
    assert!(ui.find("14.2 W").is_ok());

    // Degraded path: no labels. The sanitized zone is the fallback.
    let bare = sysinfo_state(&[(
        "power/rapl/intel-rapl_0/watts",
        zensight_common::TelemetryValue::Gauge(14.2),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &bare,
    ));
    assert!(
        ui.find("intel-rapl_0").is_ok(),
        "without the name label the row must fall back to the zone from the key"
    );
}

/// `map_power` emits capacity and status under independent `if let Some`, so a
/// battery may publish either alone. Neither case may blank the row.
#[test]
fn sysinfo_battery_capacity_and_status_are_independently_optional() {
    let both = sysinfo_state(&[
        (
            "battery/bat0/capacity",
            zensight_common::TelemetryValue::Gauge(82.0),
        ),
        (
            "battery/bat0/status",
            zensight_common::TelemetryValue::Text("Discharging".to_string()),
        ),
    ]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &both,
    ));
    assert!(ui.find("bat0").is_ok());
    assert!(ui.find("82%").is_ok());
    assert!(
        ui.find("Discharging").is_ok(),
        "status is a Text value — get_metric_value would drop it"
    );

    let capacity_only = sysinfo_state(&[(
        "battery/bat0/capacity",
        zensight_common::TelemetryValue::Gauge(82.0),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &capacity_only,
    ));
    assert!(ui.find("bat0").is_ok());
    assert!(ui.find("82%").is_ok());

    let status_only = sysinfo_state(&[(
        "battery/bat0/status",
        zensight_common::TelemetryValue::Text("Full".to_string()),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &status_only,
    ));
    assert!(ui.find("bat0").is_ok());
    assert!(ui.find("Full").is_ok());
}

/// Regression for the gate shipped in 6f24a89: `has_temperatures` was a
/// `starts_with("sensors/")` prefix check, and fans publish
/// `sensors/{chip}/{label}/rpm` under that same prefix. Any host running
/// `collect.power` without `collect.temperatures` therefore grew a Temperatures
/// card that said "No temperature sensors found".
#[test]
fn sysinfo_temperatures_card_is_hidden_when_only_fans_are_present() {
    let state = sysinfo_state(&[(
        "sensors/dell_ddv/cpu_fan/rpm",
        zensight_common::TelemetryValue::Gauge(3000.0),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(
        ui.find("No temperature sensors found").is_err(),
        "a fans-only host must not render an empty Temperatures card"
    );
    assert!(
        ui.find("Temperatures").is_err(),
        "fans share the sensors/ prefix but are not temperatures"
    );
}

/// `system/entropy_avail` opens the panel on its own, and the coupling is
/// deliberate. It is the only subject `map_power` publishes unconditionally, so
/// it is the sole on-wire evidence the power collector ran: without it, a
/// fanless, batteryless host whose `energy_uj` is root-only renders no panel at
/// all — indistinguishable from `collect.power: false`, which is exactly the
/// confusion this panel exists to remove.
#[test]
fn sysinfo_power_panel_opens_on_entropy_alone() {
    let state = sysinfo_state(&[(
        "system/entropy_avail",
        zensight_common::TelemetryValue::Gauge(256.0),
    )]);
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(
        ui.find("Fans & power").is_ok(),
        "entropy alone proves collect.power ran; the panel must open to report what it found"
    );
    assert!(
        ui.find("No fans reported by hwmon").is_ok(),
        "a host that reports no fan must say so"
    );
}

/// Every sysinfo subject the demo fabricates must be a subject the real sensor
/// could publish.
///
/// The demo is the only telemetry source that reaches the GUI without passing a
/// registry check: `Metric::new`'s conformance assert lives in the sensor, and
/// `--demo` never runs one. So a typo'd mock key ("…/rpms") fails *silently* —
/// the panel's gate returns false, the card vanishes, and nothing goes red. That
/// is precisely how a mock drifts away from the contract it exists to mirror,
/// and it is the same class of bug as the `starts_with("sensors/")` gate this
/// panel had to fix.
///
/// `is_registered_telemetry` is non-vacuous for sysinfo: its telemetry tree is
/// registered as real subject families, not a `{metric...}` catch-all.
#[test]
fn demo_sysinfo_keys_are_registered_subjects() {
    let mut simulator = zensight::demo::DemoSimulator::new();
    let points = simulator.tick(0);

    let sysinfo: Vec<_> = points
        .iter()
        .filter(|p| p.protocol == Protocol::Sysinfo)
        .collect();
    assert!(
        !sysinfo.is_empty(),
        "the demo must emit sysinfo telemetry or this guard is vacuous"
    );

    for point in sysinfo {
        assert!(
            zensight_common::registry::is_registered_telemetry("sysinfo", &point.metric),
            "demo emits unregistered sysinfo subject {:?} — it cannot come from a real sensor, \
             so nothing will render it. Register it in zensight-common/registry/sysinfo.toml \
             or fix the mock to match the contract.",
            point.metric
        );
    }
}

/// The demo must exercise the families the Fans & power panel renders (#515) —
/// otherwise `just demo` shows an empty card and the mock has silently drifted
/// from what `just run` puts on the bus.
#[test]
fn demo_emits_the_fans_power_families() {
    let mut simulator = zensight::demo::DemoSimulator::new();
    let points = simulator.tick(0);
    let metrics: Vec<&str> = points
        .iter()
        .filter(|p| p.protocol == Protocol::Sysinfo)
        .map(|p| p.metric.as_str())
        .collect();

    // Suffix-matched, not `contains`: a "/rpm" substring probe is satisfied by a
    // typo'd "/rpms", which is exactly the drift this is meant to catch.
    for (family, suffix) in [
        ("fans", "/rpm"),
        ("temperatures", "/temp"),
        ("battery capacity", "battery/bat0/capacity"),
        ("battery status", "battery/bat0/status"),
        ("entropy", "system/entropy_avail"),
    ] {
        assert!(
            metrics.iter().any(|m| m.ends_with(suffix)),
            "the demo emits no {family} points ({suffix}) — the panel will be empty in --demo"
        );
    }
    assert!(
        metrics.iter().any(|m| m.starts_with("power/rapl/")),
        "the demo emits no RAPL points — the panel will show 'not available' in --demo"
    );
}

/// End-to-end for the `--demo` path: the simulator's own points, fed through the
/// real view, must actually render the panel.
///
/// The two guards above check the demo's keys in isolation; this one closes the
/// loop. A mock can emit perfectly registered subjects and still render nothing
/// — wrong value type, missing label, a gate that disagrees — and `just demo`
/// would show an empty card with every other test green.
#[test]
fn demo_points_render_the_fans_power_panel() {
    let mut simulator = zensight::demo::DemoSimulator::new();
    let points = simulator.tick(0);

    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
    for point in points
        .into_iter()
        .filter(|p| p.protocol == Protocol::Sysinfo && p.source == "server01")
    {
        state.update(point);
    }

    let mut ui = simulator_for(&state);
    assert!(
        ui.find("Fans & power").is_ok(),
        "the demo must open the Fans & power panel"
    );
    assert!(
        ui.find("dell_ddv/cpu_fan").is_ok(),
        "the demo must render a fan row"
    );
    assert!(
        ui.find("bat0").is_ok(),
        "the demo must render a battery row"
    );

    // The demo mocks readable RAPL, so the panel shows watts rather than the
    // not-available note — the note is pinned separately against real absence.
    let mut ui = simulator_for(&state);
    assert!(
        ui.find("package-0").is_ok(),
        "the demo's RAPL zone must display its friendly name label"
    );
    let mut ui = simulator_for(&state);
    assert!(
        ui.find(
            "No RAPL zones are reporting. Either this CPU exposes none, or the sensor cannot \
             read them (/sys/class/powercap/*/energy_uj is root-only since CVE-2020-8694), or \
             it has just started and needs two readings to derive a rate. This is not a reading \
             of zero watts."
        )
        .is_err(),
        "the demo publishes watts, so it must not show the unavailable note"
    );

    // The Temperatures card shares the sensors/ prefix with fans — the demo
    // mocks temps too, so it must be populated rather than empty.
    // The row is built from the *key*, so it shows the sanitized label
    // (`package_id_0`), not the raw hwmon label carried alongside it.
    let mut ui = simulator_for(&state);
    assert!(
        ui.find("coretemp/package_id_0").is_ok(),
        "the demo must populate the Temperatures card it opens"
    );
    let mut ui = simulator_for(&state);
    assert!(ui.find("No temperature sensors found").is_err());
}

fn simulator_for(
    state: &DeviceDetailState,
) -> iced_test::Simulator<'_, zensight::message::Message> {
    simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        state,
    ))
}

fn netlink_state(points: &[(&str, zensight_common::TelemetryValue)]) -> DeviceDetailState {
    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netlink, "server01"));
    for (metric, value) in points {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "server01".to_string(),
            protocol: Protocol::Netlink,
            metric: metric.to_string(),
            value: value.clone(),
            labels: HashMap::new(),
            unit: None,
        });
    }
    state
}

/// `sockets/tcp/by_cong/{algo}` must produce a per-algorithm row. Note the
/// section early-returns unless some other `sockets/tcp/*` key is present.
#[test]
fn tier2_netlink_congestion_rows_render() {
    let mut state = netlink_state(&[
        (
            "sockets/tcp/total",
            zensight_common::TelemetryValue::Gauge(10.0),
        ),
        (
            "sockets/tcp/by_cong/cubic",
            zensight_common::TelemetryValue::Gauge(7.0),
        ),
        (
            "sockets/tcp/by_cong/bbr",
            zensight_common::TelemetryValue::Gauge(3.0),
        ),
    ]);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Sockets;
    let mut ui = simulator(zensight::view::specialized::netlink::netlink_host_view(
        &state,
    ));
    // The row label is indented (`format!("  {algo}")`).
    assert!(
        ui.find("  cubic").is_ok(),
        "the congestion-algorithm rows must render the algo name"
    );
}

/// `neighbors/by_state/{state}` must produce a donut slice *label*, not just the
/// "Neighbor states" header — which is gated on a string prefix and would render
/// over an empty donut.
#[test]
fn tier2_netlink_neighbor_state_slices_render() {
    let mut state = netlink_state(&[
        (
            "neighbors/total",
            zensight_common::TelemetryValue::Gauge(5.0),
        ),
        (
            "neighbors/by_state/reachable",
            zensight_common::TelemetryValue::Gauge(4.0),
        ),
        (
            "neighbors/by_state/stale",
            zensight_common::TelemetryValue::Gauge(1.0),
        ),
    ]);
    state.specialized_tab = zensight::view::specialized::SpecializedTab::RoutingNeighbors;
    let mut ui = simulator(zensight::view::specialized::netlink::netlink_host_view(
        &state,
    ));
    assert!(
        ui.find("reachable").is_ok(),
        "the neighbor-state donut must render its slice labels, not just its header"
    );
}

fn netring_state(
    tab: zensight::view::specialized::SpecializedTab,
    points: &[(&str, zensight_common::TelemetryValue)],
) -> DeviceDetailState {
    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "wiretap1"));
    for (metric, value) in points {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "wiretap1".to_string(),
            protocol: Protocol::Netring,
            metric: metric.to_string(),
            value: value.clone(),
            labels: HashMap::new(),
            unit: None,
        });
    }
    state.specialized_tab = tab;
    state
}

/// `dns/responses_by_rcode/{rcode}` — the sensor publishes `<rcode>_total`, and
/// the view trims the suffix — must produce a ranked-bar row per rcode.
#[test]
fn tier2_netring_dns_rcode_rows_render() {
    let state = netring_state(
        zensight::view::specialized::SpecializedTab::Dns,
        &[
            (
                "dns/queries_total",
                zensight_common::TelemetryValue::Counter(100),
            ),
            (
                "dns/responses_by_rcode/nxdomain_total",
                zensight_common::TelemetryValue::Counter(12),
            ),
            (
                "dns/responses_by_rcode/noerror_total",
                zensight_common::TelemetryValue::Counter(88),
            ),
        ],
    );
    let mut ui = simulator(zensight::view::specialized::netring::netring_sensor_view(
        &state, None,
    ));
    assert!(
        ui.find("nxdomain").is_ok(),
        "the by-rcode bar must render a row per response code"
    );
}

/// `http/methods/{method}` (published as `<method>_total`) must produce a row.
#[test]
fn tier2_netring_http_method_rows_render() {
    let state = netring_state(
        zensight::view::specialized::SpecializedTab::HttpTls,
        &[
            (
                "http/requests_total",
                zensight_common::TelemetryValue::Counter(50),
            ),
            (
                "http/methods/get_total",
                zensight_common::TelemetryValue::Counter(40),
            ),
            (
                "http/methods/post_total",
                zensight_common::TelemetryValue::Counter(10),
            ),
        ],
    );
    let mut ui = simulator(zensight::view::specialized::netring::netring_sensor_view(
        &state, None,
    ));
    assert!(
        ui.find("get").is_ok(),
        "the by-method bar must render a row per HTTP method"
    );
}

/// The netlink overview's "Interfaces up" is an N/M stat derived from
/// `iface/{iface}/up`. Asserting the *label* proves nothing — it renders "0/0"
/// just as happily. Assert the value.
#[test]
fn tier2_netlink_overview_counts_interfaces() {
    use zensight::view::overview::netlink::netlink_overview;

    let id = DeviceId::fixture(Protocol::Netlink, "server01");
    let mut device = DeviceState::new(id.clone());
    for (metric, up) in [("iface/eth0/up", true), ("iface/wan0/up", false)] {
        device.metrics.insert(
            metric.to_string(),
            zensight_common::TelemetryPoint {
                timestamp: 0,
                source: "server01".to_string(),
                protocol: Protocol::Netlink,
                metric: metric.to_string(),
                value: zensight_common::TelemetryValue::Boolean(up),
                labels: HashMap::new(),
                unit: None,
            },
        );
    }
    let devices: HashMap<&DeviceId, &DeviceState> = HashMap::from([(&id, &device)]);

    let mut ui = simulator(netlink_overview(&devices));
    assert!(
        ui.find("1/2").is_ok(),
        "one of two interfaces is up — the stat must count, not just label"
    );
}

// ===========================================================================
// Phase-5 panels (#469): the three procedures that had no caller, and the Fleet
// view. Driven through the project's GUI harness (iced_test::simulator).
// ===========================================================================

/// The sysinfo latency panel renders percentiles, not a mean — the tail is the
/// finding. Sub-millisecond waits read in µs, longer ones in ms.
#[test]
fn sysinfo_latency_panel_renders_percentiles() {
    use zensight_common::{Histogram, LatencyReport};

    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
    state.sysinfo_detail.apply_latency(Ok(LatencyReport {
        available: true,
        window_secs: 10,
        runqlat: Histogram {
            unit: "us".into(),
            buckets: Vec::new(),
            total: 500,
            p50_us: 20,
            p95_us: 8_000,
            p99_us: 40_000,
            max_us: 60_000,
        },
        biolatency: Histogram::default(),
    }));

    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(ui.find("Run-queue delay (runqlat)").is_ok());

    // p50 is sub-millisecond → µs; p99 is a 40 ms stall → ms. A mean would have
    // hidden the second behind the first, which is the whole reason for the panel.
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(ui.find("20 µs").is_ok(), "p50 renders in µs");
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &state,
    ));
    assert!(ui.find("40.0 ms").is_ok(), "p99 renders in ms");
}

/// A sensor built without the `ebpf` feature still answers, with
/// `available: false`. "Cannot measure it" and "nothing answered" are different
/// problems and must not render the same.
#[test]
fn sysinfo_latency_panel_distinguishes_unavailable_from_no_answer() {
    use zensight_common::LatencyReport;

    let mut unavailable = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
    unavailable.sysinfo_detail.apply_latency(Ok(LatencyReport {
        available: false,
        ..Default::default()
    }));
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &unavailable,
    ));
    assert!(
        ui.find(
            "The sensor is not collecting these — they need `collect.ebpf`, a binary \
             built with the `ebpf` feature, a supported kernel, and CAP_BPF + \
             CAP_PERFMON (a rootless container cannot load BPF at all)."
        )
        .is_ok(),
        "an unavailable collector must say so, not look like a failed fetch"
    );

    let mut failed = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
    failed
        .sysinfo_detail
        .apply_latency(Err("No sysinfo sensor responded".into()));
    let mut ui = simulator(zensight::view::specialized::sysinfo::sysinfo_host_view(
        &failed,
    ));
    assert!(ui.find("Fetch failed: No sysinfo sensor responded").is_ok());
}

/// The encrypted-DNS destination inventory: an unrecognised resolver is what a
/// DNS tunnel looks like from the wire, so it is called out, not left as a
/// `false` in a cell.
#[test]
fn netring_encrypted_dns_destinations_flag_unknown_resolvers() {
    use zensight_common::EncryptedDnsRecord;

    let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Netring, "wiretap1"));
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Dns;
    // The DNS tab is capability-gated on `dns/` telemetry. A host doing *only*
    // encrypted DNS still publishes `dns/encrypted/*`, so it reaches the tab —
    // which is the case that matters, since that host has no cleartext DNS at all.
    state.update(zensight_common::TelemetryPoint {
        timestamp: 0,
        source: "wiretap1".to_string(),
        protocol: Protocol::Netring,
        metric: "dns/encrypted/doh".to_string(),
        value: zensight_common::TelemetryValue::Counter(43),
        labels: HashMap::new(),
        unit: None,
    });
    state.netring_detail.apply_encrypted_dns(Ok(vec![
        EncryptedDnsRecord {
            transport: "doh".into(),
            sni: Some("cloudflare-dns.com".into()),
            via_known_resolver: true,
            count: 40,
        },
        EncryptedDnsRecord {
            transport: "dot".into(),
            sni: Some("suspicious.example".into()),
            via_known_resolver: false,
            count: 3,
        },
    ]));

    let mut ui = simulator(zensight::view::specialized::netring::netring_sensor_view(
        &state, None,
    ));
    assert!(ui.find("suspicious.example").is_ok());

    let mut ui = simulator(zensight::view::specialized::netring::netring_sensor_view(
        &state, None,
    ));
    assert!(
        ui.find("⚠ 1 destination(s) are not a recognised public resolver")
            .is_ok(),
        "the un-known resolver must be called out, not buried in a column"
    );
}

/// The Fleet view's whole reason to exist is spotting the odd one out, so the
/// skew row is the case worth pinning — and it must sort above the healthy hosts.
#[test]
fn fleet_view_surfaces_a_skewed_host_above_the_healthy_ones() {
    use zensight::view::fleet::{FleetReply, FleetState, fleet_view};

    let sysinfo_slice = zensight_common::registry::REGISTRIES
        .iter()
        .find(|(n, _)| *n == "sysinfo")
        .map(|(_, t)| (*t).to_string())
        .expect("sysinfo registry");

    // edge01 serves a bumped registry version; server01 serves ours.
    let skewed = sysinfo_slice.replacen("version = \"1.2\"", "version = \"1.0\"", 1);
    assert_ne!(skewed, sysinfo_slice, "the fixture must actually differ");

    let mut state = FleetState::default();
    state.apply(
        Ok(vec![
            FleetReply {
                origin: "h-aaaaaaaaaaaa".into(),
                producer: "sysinfo".into(),
                toml: sysinfo_slice,
            },
            FleetReply {
                origin: "h-bbbbbbbbbbbb".into(),
                producer: "sysinfo".into(),
                toml: skewed,
            },
        ]),
        &[
            ("h-aaaaaaaaaaaa".into(), "sysinfo".into(), "server01".into()),
            ("h-bbbbbbbbbbbb".into(), "sysinfo".into(), "edge01".into()),
        ],
    );

    let rows = state.rows.ready().expect("rows");
    assert_eq!(
        rows[0].host, "edge01",
        "the skewed host must sort first — a drifting host buried under ten healthy \
         ones is the failure this view exists to prevent"
    );

    let mut ui = simulator(fleet_view(&state));
    assert!(ui.find("version skew").is_ok());
    let mut ui = simulator(fleet_view(&state));
    assert!(ui.find("in sync").is_ok(), "server01 agrees with us");
}

/// A producer that is alive on the bus but answers no `introspect` must appear as
/// `silent`, not vanish. Fanning out alone cannot tell "not deployed" from
/// "deployed and not answering", and the second is the row you need to see.
#[test]
fn fleet_view_shows_an_alive_but_silent_producer() {
    use zensight::view::fleet::{FleetState, fleet_view};

    let mut state = FleetState::default();
    state.apply(
        Ok(Vec::new()),
        &[("h-cccccccccccc".into(), "netring".into(), "edge01".into())],
    );

    let mut ui = simulator(fleet_view(&state));
    assert!(
        ui.find("edge01").is_ok(),
        "the silent host must still be listed"
    );
    let mut ui = simulator(fleet_view(&state));
    assert!(ui.find("silent").is_ok());
}

/// SNMP fleet overview (#533): rate-based top talkers, down hotlist, error
/// hotspots — all from the typed docs.
#[test]
fn test_snmp_overview_rate_based() {
    use std::collections::HashMap;
    use zensight::view::overview::snmp::snmp_overview;

    let mut docs = HashMap::new();
    // router01: iface 1 busy at 12.5 MB/s in, iface 2 has errors, iface 3 down.
    docs.insert(
        "router01".to_string(),
        mock::snmp::interface_table("router01", 3),
    );
    // A freshly-rebooted device with huge *lifetime* counters but no current
    // rate must NOT outrank the busy one (the raw-counter bug this fixes).
    docs.insert(
        "idle01".to_string(),
        mock::snmp::interface_table_no_hc("idle01"),
    );

    let devices: HashMap<&DeviceId, &DeviceState> = HashMap::new();
    let events = std::collections::VecDeque::new();
    let mut ui = simulator(snmp_overview(&devices, &docs, &events));

    // Top talker is router01's busiest interface (rates, humanized).
    assert!(ui.find("1.").is_ok());
    assert!(ui.find("router01/eth2").is_ok(), "busiest iface ranks");
    // Down hotlist: router01's last interface is admin-up/oper-down.
    assert!(ui.find("Down Interfaces (1)").is_ok());
    // Error hotspots by rate.
    assert!(ui.find("3.5 errs/s").is_ok());
    // Fleet throughput tile present.
    assert!(ui.find("Throughput").is_ok());
}

/// Empty fleet renders the empty state.
#[test]
fn test_snmp_overview_empty() {
    use std::collections::HashMap;
    use zensight::view::overview::snmp::snmp_overview;

    let devices: HashMap<&DeviceId, &DeviceState> = HashMap::new();
    let docs = HashMap::new();
    let events = std::collections::VecDeque::new();
    let mut ui = simulator(snmp_overview(&devices, &docs, &events));
    assert!(ui.find("No SNMP devices available").is_ok());
}

/// SNMP device view shows the trap/event feed (#536).
#[test]
fn test_snmp_device_event_feed() {
    let device_id = DeviceId::fixture(Protocol::Snmp, "router01".to_string());
    let mut state = DeviceDetailState::new(device_id);
    state.snmp_detail.events.push_back(mock::snmp::trap_event(
        "router01",
        "trap/link_down",
        "01aaa",
    ));
    state.snmp_detail.events.push_back(mock::snmp::trap_event(
        "router01",
        "trap/cold_start",
        "01aab",
    ));

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    assert!(ui.find("Events").is_ok());
    assert!(ui.find("trap/link_down").is_ok());
    assert!(ui.find("if_index=3").is_ok(), "fields render");
}

/// Fleet overview shows the recent-trap feed with top emitters (#536).
#[test]
fn test_snmp_overview_trap_feed() {
    use std::collections::HashMap;
    use zensight::view::overview::snmp::snmp_overview;

    let devices: HashMap<&DeviceId, &DeviceState> = HashMap::new();
    let mut docs = HashMap::new();
    docs.insert(
        "router01".to_string(),
        mock::snmp::interface_table("router01", 1),
    );
    let mut events = std::collections::VecDeque::new();
    events.push_back(mock::snmp::trap_event(
        "router01",
        "trap/link_down",
        "01aac",
    ));
    events.push_back(mock::snmp::trap_event("router01", "trap/link_up", "01aad"));
    events.push_back(mock::snmp::trap_event("sw02", "trap/cold_start", "01aae"));

    let mut ui = simulator(snmp_overview(&devices, &docs, &events));
    assert!(
        ui.find("Recent Traps (3) — top: router01 (2), sw02 (1)")
            .is_ok()
    );
    assert!(ui.find("trap/cold_start").is_ok());
}

/// The fleet event ring dedups by ULID and keeps newest-first order (#536).
#[test]
fn test_snmp_event_ring_dedup_and_order() {
    use zensight::view::dashboard::DashboardState;

    let mut dash = DashboardState::default();
    dash.push_snmp_event(mock::snmp::trap_event("r1", "trap/a", "01aaa"));
    dash.push_snmp_event(mock::snmp::trap_event("r1", "trap/b", "01aac"));
    dash.push_snmp_event(mock::snmp::trap_event("r1", "trap/c", "01aab")); // out of order
    dash.push_snmp_event(mock::snmp::trap_event("r1", "trap/b", "01aac")); // duplicate

    let ids: Vec<&str> = dash.snmp_events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["01aac", "01aab", "01aaa"],
        "newest first, deduped"
    );
}
