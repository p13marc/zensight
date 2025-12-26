//! UI tests using iced_test Simulator.
//!
//! These tests verify the UI behavior without needing actual Zenoh connections
//! or hardware bridges.

use iced_test::simulator;

// Re-export view components for testing
use zensight::app::AppTheme;
use zensight::message::{DeviceId, Message};
use zensight::mock;
use zensight::view::dashboard::{DashboardState, DeviceState, dashboard_view};
use zensight::view::device::{DeviceDetailState, device_view};
use zensight::view::groups::GroupsState;
use zensight::view::overview::OverviewState;
use zensight::view::settings::{SettingsState, settings_view};
use zensight::view::topology::{TopologyState, topology_view};

use std::collections::HashMap;
use zensight_common::{HealthSnapshot, Protocol};

/// Test that the dashboard view renders correctly with no devices.
#[test]
fn test_dashboard_empty() {
    let state = DashboardState::default();
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
    ));

    // Should show "Waiting for telemetry data..." message
    assert!(ui.find("Waiting for telemetry data...").is_ok());
}

/// Test that the dashboard shows devices when populated.
#[test]
fn test_dashboard_with_devices() {
    let mut state = DashboardState::default();
    state.connected = true;

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
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
    ));

    // Should show the device name
    assert!(ui.find("router01").is_ok());
    // Should show metric count
    assert!(ui.find("5 metrics").is_ok());
    // Should show Connected status
    assert!(ui.find("Connected").is_ok());
}

/// Test clicking the Settings button.
#[test]
fn test_dashboard_settings_button() {
    let state = DashboardState::default();
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
    ));

    // Click Settings button
    let _ = ui.click("Settings");

    // Should have produced OpenSettings message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));
}

/// Test clicking the Alerts button.
#[test]
fn test_dashboard_alerts_button() {
    let state = DashboardState::default();
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
    ));

    // Click Alerts button
    let _ = ui.click("Alerts");

    // Should have produced OpenAlerts message
    let messages: Vec<Message> = ui.into_messages().collect();
    assert!(messages.iter().any(|m| matches!(m, Message::OpenAlerts)));
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

    let mut ui = simulator(device_view(&state));

    // Should show the device name
    assert!(ui.find("server01").is_ok());
    // Should show Back button
    assert!(ui.find("Back").is_ok());
    // Should show section headers (specialized view shows CPU, Memory, etc.)
    assert!(ui.find("CPU").is_ok());
    assert!(ui.find("Memory").is_ok());
}

/// Test clicking Back button in device view.
#[test]
fn test_device_back_button() {
    let device_id = DeviceId {
        protocol: Protocol::Snmp,
        source: "router01".to_string(),
    };
    let state = DeviceDetailState::new(device_id);

    let mut ui = simulator(device_view(&state));

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

    let mut ui = simulator(device_view(&state));

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
        protocol: Protocol::Syslog,
        source: "server01".to_string(),
    };
    let mut state = DeviceDetailState::new(device_id);

    // Add a syslog message
    let mut point = TelemetryPoint::new(
        "server01",
        Protocol::Syslog,
        "message/1",
        TelemetryValue::Text("Test log message".to_string()),
    );
    point.labels.insert("severity".to_string(), "4".to_string()); // Warning
    point
        .labels
        .insert("app_name".to_string(), "test".to_string());
    state.update(point);

    let mut ui = simulator(device_view(&state));

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

    let mut ui = simulator(device_view(&state));

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

    let mut ui = simulator(device_view(&state));

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

    let mut ui = simulator(device_view(&state));

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
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
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
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
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
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
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

/// Test clicking Topology button in dashboard.
#[test]
fn test_dashboard_topology_button() {
    let state = DashboardState::default();
    let groups = GroupsState::default();
    let overview = OverviewState::default();
    let bridge_health = HashMap::new();
    let mut ui = simulator(dashboard_view(
        &state,
        AppTheme::Dark,
        0,
        &groups,
        &overview,
        &bridge_health,
    ));

    // Click Topology button
    let _ = ui.click("Topology");

    // Should have produced OpenTopology message
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
