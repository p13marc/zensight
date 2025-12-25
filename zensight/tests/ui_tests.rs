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
use zensight::view::settings::{SettingsState, settings_view};

use zensight_common::Protocol;

/// Test that the dashboard view renders correctly with no devices.
#[test]
fn test_dashboard_empty() {
    let state = DashboardState::default();
    let mut ui = simulator(dashboard_view(&state, AppTheme::Dark, 0));

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

    let mut ui = simulator(dashboard_view(&state, AppTheme::Dark, 0));

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
    let mut ui = simulator(dashboard_view(&state, AppTheme::Dark, 0));

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
    let mut ui = simulator(dashboard_view(&state, AppTheme::Dark, 0));

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
    // Should show some metrics
    assert!(ui.find("cpu/usage").is_ok());
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
