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
    DeviceDetailState, FacetTab, device_view_with_syslog_filter, host_detail_view,
};
use zensight::view::groups::GroupsState;
use zensight::view::overview::OverviewState;
use zensight::view::settings::{SettingsState, settings_view};
use zensight::view::specialized::SyslogFilterState;
use zensight::view::topology::{TopologyState, topology_view};

use std::collections::HashMap;
use zensight_common::Protocol;

/// Test that the dashboard view renders correctly with no devices.
#[test]
fn test_dashboard_empty() {
    let state = DashboardState::default();
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
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
    let device_id = DeviceId {
        protocol: Protocol::Snmp,
        source: "router01".to_string(),
    };
    let mut device = DeviceState::new(device_id.clone());
    device.metric_count = 5;
    device.is_healthy = true;
    state.devices.insert(device_id, device);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
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

    let degraded_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "host-sad".to_string(),
    };
    let mut degraded = DeviceState::new(degraded_id.clone());
    degraded.update_from_liveness(DeviceStatus::Degraded, 2, Some("flapping".into()));
    state.devices.insert(degraded_id.clone(), degraded);

    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let sensor_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
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
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
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
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        sparks,
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

    let device_id = DeviceId {
        protocol: Protocol::Snmp,
        source: "router01".to_string(),
    };
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
        },
    );

    let mut state = GlobalSearchState::default();
    state.open();
    state.query = "queue".to_string();
    let hits: Vec<SearchHit> = search::search([&device].into_iter(), &state.query);
    assert_eq!(hits.len(), 1);

    let mut ui = simulator(search::global_search_panel(&state, hits));
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
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
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

/// #133: a multi-sensor host renders one facet tab per sensor, and clicking an
/// inactive facet switches to it (`SelectDevice`). The protocol is a facet of the
/// host, not a top-level axis.
#[test]
fn test_host_detail_facet_tabs() {
    use zensight_common::DeviceStatus;

    let active = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
    let mut state = DeviceDetailState::new(active.clone());
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }

    let netlink_id = DeviceId {
        protocol: Protocol::Netlink,
        source: "server01".to_string(),
    };
    let facets = vec![
        FacetTab {
            id: active.clone(),
            protocol: Protocol::Sysinfo,
            status: DeviceStatus::Online,
            active: true,
        },
        FacetTab {
            id: netlink_id.clone(),
            protocol: Protocol::Netlink,
            status: DeviceStatus::Degraded,
            active: false,
        },
    ];

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(&state, &syslog_filter, &[], &facets));

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

    let id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
    let mut state = DeviceDetailState::new(id.clone());
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }
    let facets = vec![FacetTab {
        id,
        protocol: Protocol::Sysinfo,
        status: DeviceStatus::Online,
        active: true,
    }];

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(host_detail_view(&state, &syslog_filter, &[], &facets));

    // No "Facets" strip for a lone sensor; the detail still renders.
    assert!(ui.find("Facets").is_err());
    assert!(ui.find("server01").is_ok());
}

/// Test clicking Back button in device view.
#[test]
fn test_device_back_button() {
    let device_id = DeviceId {
        protocol: Protocol::Snmp,
        source: "router01".to_string(),
    };
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
    let device = DeviceId::new(Protocol::Sysinfo, "server01");
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
            Message::InvestigateAlert { device: d, metric: Some(metric) }
                if d.source == "server01" && metric == "cpu/usage"
        )),
        "expected InvestigateAlert for server01/cpu/usage, got {messages:?}"
    );
}

/// #50: a metric row's "alert" button emits PromoteMetricToAlert with the
/// metric path and current value.
#[test]
fn test_metric_promote_to_alert() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::new(Protocol::Sysinfo, "server01");
    let mut state = DeviceDetailState::new(device_id);
    let mut p = zensight_common::TelemetryPoint {
        timestamp: 0,
        source: "server01".to_string(),
        protocol: Protocol::Sysinfo,
        metric: "cpu/usage".to_string(),
        value: TelemetryValue::Gauge(91.0),
        labels: HashMap::new(),
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

    let device_id = DeviceId::new(Protocol::Sysinfo, "server01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: f64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "server01".to_string(),
            protocol: Protocol::Sysinfo,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(v),
            labels: HashMap::new(),
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

    let device_id = DeviceId::new(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: u64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "gw01".to_string(),
            protocol: Protocol::Netlink,
            metric: metric.to_string(),
            value: TelemetryValue::Counter(v),
            labels: HashMap::new(),
        });
    };
    put("tc/eth0/fq_codel/drops", 42);
    put("tc/eth0/fq_codel/overlimits", 7);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    assert!(ui.find("TC / QoS qdiscs").is_ok());
    assert!(ui.find("fq_codel").is_ok());
}

/// #46: netlink renders the IPsec/xfrm card and RTT-percentile socket lines.
#[test]
fn test_netlink_depth_cards() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::new(Protocol::Netlink, "gw01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: f64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "gw01".to_string(),
            protocol: Protocol::Netlink,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(v),
            labels: HashMap::new(),
        });
    };
    put("sockets/tcp/established", 10.0);
    put("sockets/tcp/rtt_p95_us", 1234.0);
    put("xfrm/sa/total", 4.0);

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter, &[]));
    assert!(ui.find("IPsec / xfrm").is_ok());
    assert!(ui.find("RTT p95 (us)").is_ok());
}

/// #45: netring renders DNS RED, HTTP RED, and per-L4 cards when present.
#[test]
fn test_netring_red_cards() {
    use zensight_common::TelemetryValue;

    let device_id = DeviceId::new(Protocol::Netring, "sensor01");
    let mut state = DeviceDetailState::new(device_id);
    let mut put = |metric: &str, v: f64| {
        state.update(zensight_common::TelemetryPoint {
            timestamp: 0,
            source: "sensor01".to_string(),
            protocol: Protocol::Netring,
            metric: metric.to_string(),
            value: TelemetryValue::Counter(v as u64),
            labels: HashMap::new(),
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

/// Test clicking Save Settings button.
#[test]
fn test_settings_save_button() {
    let state = SettingsState::default();
    let mut ui = simulator(settings_view(&state));

    // Click Save button
    let _ = ui.click("Save Settings");

    // Should have produced SaveSettings message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::SaveSettings)));
}

/// Test metric filtering in device view.
#[test]
fn test_device_metric_filter() {
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
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
    let device_id = DeviceId {
        protocol: Protocol::Snmp,
        source: "router01".to_string(),
    };
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

/// Test syslog specialized view renders with severity distribution.
#[test]
fn test_syslog_specialized_view() {
    use zensight_common::TelemetryPoint;
    use zensight_common::TelemetryValue;

    let device_id = DeviceId {
        protocol: Protocol::Logs,
        source: "server01".to_string(),
    };
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

    let device_id = DeviceId {
        protocol: Protocol::Modbus,
        source: "plc01".to_string(),
    };
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

    let device_id = DeviceId {
        protocol: Protocol::Netflow,
        source: "router01".to_string(),
    };
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

    let device_id = DeviceId {
        protocol: Protocol::Gnmi,
        source: "spine01".to_string(),
    };
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
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
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
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
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
    let device_id = DeviceId {
        protocol: Protocol::Snmp,
        source: "router01".to_string(),
    };
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
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
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
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
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
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &sensor_health,
        zensight::view::trend::DeviceSparks::new(),
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
    let mut ui = simulator(topology_view(&state, AppTheme::Dark));

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
    let mut ui = simulator(topology_view(&state, AppTheme::Dark));

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
    let mut ui = simulator(topology_view(&state, AppTheme::Dark));

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
    let mut ui = simulator(topology_view(&state, AppTheme::Dark));

    // Should show search placeholder
    assert!(ui.find("Search nodes...").is_ok());
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

    let device_id = DeviceId::new(Protocol::Netlink, "router01");
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

    let device_id = DeviceId::new(Protocol::Netlink, "router01");
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
                    snd_cwnd: 10,
                    snd_buf: 16384,
                    rcv_buf: 32768,
                    delivery_rate: 0,
                    pacing_rate: 0,
                    bytes_retrans: 0,
                    total_retrans: 0,
                    rcv_rtt_us: 0,
                    lost: 0,
                    reord_seen: 0,
                },
            ])),
        );
    }

    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("Netlink: router01").is_ok());
    assert!(ui.find("eth0").is_ok());
    assert!(ui.find("TCP Sockets").is_ok());
    // New enh-01 sections surface diagnostics, neighbors, and routes.
    assert!(ui.find("Diagnostics").is_ok());
    assert!(ui.find("Neighbors (ARP/NDP)").is_ok());
    assert!(ui.find("Routes").is_ok());
    // enh-02 §3 on-demand detail: fetch buttons + the fetched socket table.
    assert!(ui.find("On-demand Detail").is_ok());
    assert!(ui.find("Fetch Sockets").is_ok());
    assert!(ui.find("10.0.0.1:5555").is_ok());
}

/// The netring specialized view renders flows + top talkers.
#[test]
fn test_netring_specialized_view() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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

    // Pre-populate on-demand flow detail (as if @/query/flows had replied).
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
        }]));

    // #247: content is tabbed. Loading/error render inline on the Flows tab;
    // drive the active tab explicitly (view tests can't switch via click).
    {
        let mut s = DeviceDetailState::new(DeviceId::new(Protocol::Netring, "wiretap1"));
        s.specialized_tab = zensight::view::specialized::SpecializedTab::Flows;
        s.netring_detail.loading();
        {
            let mut ui = simulator(netring_sensor_view(&s));
            assert!(ui.find("Fetching…").is_ok());
        }

        s.netring_detail.apply(Err("no sensor".into()));
        let mut ui = simulator(netring_sensor_view(&s));
        assert!(ui.find("Fetch failed: no sensor").is_ok());
    }

    // Overview (default): header + flow-volume + TCP health + tab strip.
    {
        let mut ui = simulator(netring_sensor_view(&state));
        assert!(ui.find("Netring: wiretap1").is_ok());
        assert!(ui.find("Flows").is_ok()); // tab label
        assert!(ui.find("TCP Health").is_ok());
        assert!(ui.find("bytes (total)").is_ok());
    }

    // Bandwidth tab: per-app throughput.
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Bandwidth;
    {
        let mut ui = simulator(netring_sensor_view(&state));
        assert!(ui.find("https").is_ok());
    }

    // Flows tab: on-demand flow detail (fetch button + fetched row + orientation).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Flows;
    {
        let mut ui = simulator(netring_sensor_view(&state));
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
    let nl_id = DeviceId::new(Protocol::Netlink, "router01");
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
    let nr_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
    use zensight::view::blob_fetch::BlobFetch;
    use zensight::view::dir_fetch::DirFetch;
    use zensight::view::sensors::sensors_view;
    use zensight_common::{ErrorReport, ErrorType, HealthSnapshot, HealthStatus};

    let idle = BlobFetch::default();
    let dir_idle = DirFetch::default();
    let no_dirs: HashMap<String, Vec<String>> = HashMap::new();

    // Empty state.
    let empty: HashMap<String, HealthSnapshot> = HashMap::new();
    let no_errors: HashMap<String, VecDeque<ErrorReport>> = HashMap::new();
    let mut ui = simulator(sensors_view(
        &empty, &no_errors, &idle, None, &dir_idle, &no_dirs, None,
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

    let mut ui = simulator(sensors_view(
        &health, &errors, &idle, None, &dir_idle, &no_dirs, None,
    ));
    assert!(ui.find("snmp").is_ok());
    assert!(ui.find("Degraded").is_ok());
    assert!(ui.find("Responding").is_ok());
    assert!(ui.find("Recent errors (1)").is_ok());
    // The per-sensor debug-report download control is present (#197).
    assert!(ui.find("Download debug report").is_ok());

    // While a download is active for this sensor, the card shows progress + Cancel.
    let active = BlobFetch::Downloading { got: 1, total: 4 };
    let mut ui = simulator(sensors_view(
        &health,
        &errors,
        &active,
        Some("zensight/snmp"),
        &dir_idle,
        &no_dirs,
        None,
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
    use zensight::view::blob_fetch::BlobFetch;
    use zensight::view::dir_fetch::DirFetch;
    use zensight::view::sensors::sensors_view;
    use zensight_common::{ErrorReport, HealthSnapshot, HealthStatus};

    let idle = BlobFetch::default();
    let no_errors: HashMap<String, VecDeque<ErrorReport>> = HashMap::new();

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
        },
    );

    // sysinfo advertises two snapshot directories.
    let mut snapshot_dirs: HashMap<String, Vec<String>> = HashMap::new();
    snapshot_dirs.insert(
        "zensight/sysinfo".to_string(),
        vec!["etc".to_string(), "pcaps".to_string()],
    );

    // Idle: a "Download <name>" button per directory, and clicking one emits
    // DownloadSnapshot for that directory.
    let dir_idle = DirFetch::default();
    let mut ui = simulator(sensors_view(
        &health,
        &no_errors,
        &idle,
        None,
        &dir_idle,
        &snapshot_dirs,
        None,
    ));
    assert!(ui.find("Directory snapshots").is_ok());
    assert!(ui.find("Download etc").is_ok());
    assert!(ui.find("Download pcaps").is_ok());
    let _ = ui.click("Download etc");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::DownloadSnapshot { key_prefix, dir }
            if key_prefix == "zensight/sysinfo" && dir == "etc"
    )));

    // Fetching: the card shows chunk progress + a Cancel button.
    let fetching = DirFetch::Fetching { got: 2, total: 5 };
    let mut ui = simulator(sensors_view(
        &health,
        &no_errors,
        &idle,
        None,
        &fetching,
        &snapshot_dirs,
        Some("zensight/sysinfo"),
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

    let device_id = DeviceId::new(Protocol::Netlink, "gw01");
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
            "wireguard/wg0/AbCd1234/rx_bytes",
            TelemetryValue::Counter(1000),
        ),
        ("wireguard/wg0/AbCd1234/up", TelemetryValue::Boolean(true)),
    ] {
        state.update(TelemetryPoint::new("gw01", Protocol::Netlink, m, v));
    }
    let mut ui = simulator(netlink_host_view(&state));
    assert!(ui.find("Conntrack").is_ok());
    assert!(ui.find("WireGuard").is_ok());
    assert!(ui.find("75.0%").is_ok()); // utilization as a percentage
    assert!(ui.find("wg0 — 1 peers").is_ok());
}

/// The netring view shows the TLS section (with a fetched inventory) and the
/// Capture Health section when capture/* metrics are present.
#[test]
fn test_netring_tls_capture_sections() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue, TlsRecord};

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
    }]));

    // TLS is on the HTTP/TLS tab; Capture Health on the Capture tab (#247).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::HttpTls;
    {
        let mut ui = simulator(netring_sensor_view(&state));
        assert!(ui.find("TLS").is_ok());
        assert!(ui.find("Fetch inventory").is_ok());
        assert!(ui.find("api.example.com").is_ok());
    }
    state.specialized_tab = zensight::view::specialized::SpecializedTab::Capture;
    {
        let mut ui = simulator(netring_sensor_view(&state));
        assert!(ui.find("Capture Health").is_ok());
    }
}

/// #72: the netring view surfaces the QUIC SNI/ALPN and SSH/HASSH inventories —
/// rendered when the sensor publishes their aggregate counts — with on-demand
/// tables and fetch affordances wired to the right messages.
#[test]
fn test_netring_quic_ssh_sections() {
    use zensight::view::specialized::netring::netring_sensor_view;
    use zensight_common::{Protocol, QuicRecord, SshRecord, TelemetryPoint, TelemetryValue};

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
    }]));
    state.netring_detail.apply_ssh(Ok(vec![SshRecord {
        hassh: "06046964c022c6407d15a27b12a51c5b".into(),
        role: "client".into(),
        banner: Some("SSH-2.0-OpenSSH_9.6".into()),
        count: 2,
    }]));

    // QUIC/SSH inventories live on the HTTP/TLS tab (#247).
    state.specialized_tab = zensight::view::specialized::SpecializedTab::HttpTls;
    let mut ui = simulator(netring_sensor_view(&state));
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

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
    let mut ui = simulator(netring_sensor_view(&state));
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

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
    let mut ui = simulator(netring_sensor_view(&state));
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

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
    }]));

    state.specialized_tab = zensight::view::specialized::SpecializedTab::Assets;
    let mut ui = simulator(netring_sensor_view(&state));
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

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
    let mut state = DeviceDetailState::new(device_id);
    // No dns/ metrics → DNS tab hidden; always-on tabs present.
    {
        let mut ui = simulator(netring_sensor_view(&state));
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
        let mut ui = simulator(netring_sensor_view(&state));
        assert!(ui.find("DNS").is_ok());
    }
    // Clicking a tab emits SelectSpecializedTab for this device.
    let mut ui = simulator(netring_sensor_view(&state));
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

    let device_id = DeviceId::new(Protocol::Netring, "wiretap1");
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
        let mut ui = simulator(netring_sensor_view(&state));
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
        let mut ui = simulator(netring_sensor_view(&state));
        assert!(ui.find("Anomalies (1)").is_ok());
        assert!(ui.find("RitaBeacon").is_ok());
        assert!(ui.find("periodic beaconing to 1.2.3.4").is_ok());
        assert!(ui.find("Open Security view").is_ok());
    }
    // The per-anomaly flow pivot targets the offending src.
    {
        let mut ui = simulator(netring_sensor_view(&state));
        let _ = ui.click("flows →");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::NetringPivotToFlows(_, ep) if ep == "10.0.0.9"
        )));
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

    let device_id = DeviceId::new(Protocol::Logs, "host01");
    let mut state = DeviceDetailState::new(device_id);
    for (m, v) in [
        ("logs/errors_total", TelemetryValue::Counter(42)),
        ("logs/warnings_total", TelemetryValue::Counter(7)),
        ("logs/units_in_failure", TelemetryValue::Gauge(2.0)),
        (
            "logs/by_unit/nginx.service/messages_total",
            TelemetryValue::Counter(900),
        ),
    ] {
        state.update(TelemetryPoint::new("host01", Protocol::Logs, m, v));
    }

    let filter = SyslogFilterState::default();
    let mut ui = simulator(syslog_event_view(&state, &filter, &[]));
    assert!(ui.find("Log Rollups").is_ok());
    assert!(ui.find("errors (total)").is_ok());
    assert!(ui.find("by unit (top)").is_ok());
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
        }],
        tls: vec![TlsRecord {
            sni: Some("login.example".into()),
            alpn: Some("h2".into()),
            ja3: None,
            ja4: Some("t13d1516h2_abc_def".into()),
            count: 3,
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

    let mut ui = simulator(inventory_view(&state));
    assert!(ui.find("Inventory").is_ok());
    assert!(ui.find("AcmeCorp").is_ok(), "vendor must be rendered");
    assert!(ui.find("printer1").is_ok());
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

    let device_id = DeviceId {
        protocol: Protocol::Logs,
        source: "host9".to_string(),
    };
    let state = DeviceDetailState::new(device_id);

    let mk = |msg: &str| {
        let p = TelemetryPoint {
            timestamp: 1,
            source: "host9".to_string(),
            protocol: Protocol::Logs,
            metric: "daemon/err".to_string(),
            value: TelemetryValue::Text(msg.to_string()),
            labels: HashMap::new(),
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
