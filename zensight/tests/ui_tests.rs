//! UI tests using iced_test Simulator.
//!
//! These tests verify the UI behavior without needing actual Zenoh connections
//! or hardware sensors.

use iced_test::simulator;

// Re-export view components for testing
use zensight::app::{AppTheme, CurrentView};
use zensight::message::{DeviceId, Message};
use zensight::mock;
use zensight::view::dashboard::{ConnectionState, DashboardState, DeviceState, dashboard_view};
use zensight::view::device::{DeviceDetailState, device_view_with_syslog_filter};
use zensight::view::groups::GroupsState;
use zensight::view::overview::OverviewState;
use zensight::view::settings::{SettingsState, settings_view};
use zensight::view::specialized::SyslogFilterState;
use zensight::view::topology::{TopologyState, topology_view};

use std::collections::HashMap;
use zensight_common::{HealthSnapshot, Protocol};

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
    ));

    // Should show the device name
    assert!(ui.find("router01").is_ok());
    // Should show metric count
    assert!(ui.find("5 metrics").is_ok());
    // (Connection status now lives in the app shell, not the dashboard view.)
}

/// Render the persistent app shell around a dummy page, for nav-rail tests.
fn shell_ui() -> iced_test::Simulator<'static, Message> {
    let content = iced::widget::text("content").into();
    simulator(zensight::view::shell::app_shell(
        CurrentView::Dashboard,
        None,
        ConnectionState::Connected,
        0,
        content,
    ))
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
    assert!(ui.find("Dashboard").is_ok());
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
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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

    let syslog_filter = SyslogFilterState::default();
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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
    let mut ui = simulator(device_view_with_syslog_filter(&state, &syslog_filter));

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

/// The nav rail's Topology button emits OpenTopology.
#[test]
fn test_shell_topology_button() {
    let mut ui = shell_ui();
    let _ = ui.click("Topology");
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
    use zensight::view::security::security_view;
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

    let mut ui = simulator(security_view(&alerts));
    assert!(ui.find("Security — Network Anomalies").is_ok());
    assert!(ui.find("PortScanTRW from 10.0.0.5").is_ok());
    assert!(ui.find("10.0.0.5").is_ok());
    // The expectation alert must NOT appear in the security lens.
    assert!(ui.find("sshd not listening").is_err());
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
        (
            "neighbors/by_state/reachable",
            TelemetryValue::Gauge(2.0),
        ),
        ("routes/ipv4_count", TelemetryValue::Gauge(5.0)),
        (
            "routes/default_v4_present",
            TelemetryValue::Boolean(true),
        ),
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
            Ok(NetlinkDetailData::Sockets(vec![zensight_common::SocketRecord {
                local: "10.0.0.1:5555".into(),
                remote: "1.1.1.1:443".into(),
                state: "established".into(),
                uid: 1000,
                recv_q: 0,
                send_q: 0,
                rtt_us: 1234,
                retrans: 0,
                inode: 9999,
            }])),
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
    state.netring_detail.apply(Ok(vec![zensight_common::FlowRecord {
        src: "10.0.0.1:54321".into(),
        dst: "10.0.0.2:80".into(),
        proto: "tcp".into(),
        bytes: 694,
        packets: 10,
        duration_ms: 100,
        reason: "fin".into(),
    }]));

    // Loading state: button reads "Fetching…" while a fetch is in flight; an
    // error renders inline. Use a fresh state so the main assertions below still
    // see the ready flow table.
    {
        let mut s = DeviceDetailState::new(DeviceId::new(Protocol::Netring, "wiretap1"));
        s.netring_detail.loading();
        {
            let mut ui = simulator(netring_sensor_view(&s));
            assert!(ui.find("Fetching…").is_ok());
        }

        s.netring_detail.apply(Err("no sensor".into()));
        let mut ui = simulator(netring_sensor_view(&s));
        assert!(ui.find("Fetch failed: no sensor").is_ok());
    }

    let mut ui = simulator(netring_sensor_view(&state));
    assert!(ui.find("Netring: wiretap1").is_ok());
    assert!(ui.find("Flows").is_ok());
    assert!(ui.find("https").is_ok());
    // enh-03 flow-volume + TCP health sections.
    assert!(ui.find("TCP Health").is_ok());
    assert!(ui.find("bytes (total)").is_ok());
    // enh-03 §D on-demand flow detail: fetch button + fetched flow row.
    assert!(ui.find("Recent Flows (on demand)").is_ok());
    assert!(ui.find("Fetch Flows").is_ok());
    assert!(ui.find("10.0.0.1:54321").is_ok());
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
        TelemetryPoint::new("router01", Protocol::Netlink, "iface/eth0/up", TelemetryValue::Boolean(true)),
    );
    nl.metrics.insert(
        "sockets/tcp/established".into(),
        TelemetryPoint::new("router01", Protocol::Netlink, "sockets/tcp/established", TelemetryValue::Gauge(7.0)),
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
        TelemetryPoint::new("wiretap1", Protocol::Netring, "flow/active", TelemetryValue::Gauge(3.0)),
    );
    nr.metrics.insert(
        "tcp/resets_total".into(),
        TelemetryPoint::new("wiretap1", Protocol::Netring, "tcp/resets_total", TelemetryValue::Counter(5)),
    );
    let nr_map: HashMap<&DeviceId, &DeviceState> = std::iter::once((&nr_id, &nr)).collect();
    let mut ui = simulator(netring_overview(&nr_map));
    assert!(ui.find("Active flows").is_ok());
    assert!(ui.find("TCP resets").is_ok());
}

/// The Sensors view surfaces sensor health (previously collected but never shown).
#[test]
fn test_sensors_view() {
    use std::collections::HashMap;
    use zensight::view::sensors::sensors_view;
    use zensight_common::{HealthSnapshot, HealthStatus};

    // Empty state.
    let empty: HashMap<String, HealthSnapshot> = HashMap::new();
    let mut ui = simulator(sensors_view(&empty));
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
    let mut ui = simulator(sensors_view(&health));
    assert!(ui.find("snmp").is_ok());
    assert!(ui.find("Degraded").is_ok());
    assert!(ui.find("Responding").is_ok());
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
