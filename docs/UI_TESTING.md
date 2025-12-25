# UI Testing Guide

This document describes how to test the ZenSight frontend application using Iced 0.14's testing framework.

## Overview

ZenSight uses two complementary testing approaches:

1. **Simulator Tests** - Headless unit tests using `iced_test::Simulator`
2. **Tester (F12)** - Interactive E2E recording and playback

## Quick Start

```bash
# Run all UI tests
cargo test -p zensight

# Run a specific test
cargo test -p zensight test_dashboard_empty

# Build with E2E recording enabled
cargo run -p zensight --features tester
```

## Simulator Tests

The `iced_test` crate provides a `Simulator` that renders UI components in memory without creating a window.

### Basic Usage

```rust
use iced_test::simulator;
use zensight::view::dashboard::{DashboardState, dashboard_view};
use zensight::message::Message;

#[test]
fn test_dashboard_empty() {
    // Create component state
    let state = DashboardState::default();
    
    // Create simulator from view function
    let mut ui = simulator(dashboard_view(&state));
    
    // Find elements by text content
    assert!(ui.find("Waiting for telemetry data...").is_ok());
}
```

### Selectors

The `Simulator` uses selectors to find and interact with elements. The most common selector is `&str`, which matches by text content:

```rust
// Find by exact text match
ui.find("Settings")           // Finds element containing "Settings"
ui.find("router01")           // Finds element containing "router01"
ui.find("5 metrics")          // Finds element containing "5 metrics"
```

Other selector types:
- `widget::Id` - Find by widget ID
- `Point` - Find by screen coordinates
- Custom closures implementing `Selector` trait

### Interactions

#### Click

```rust
// Click an element by text
let result = ui.click("Settings");
assert!(result.is_ok());

// The click may fail if element not found or not visible
match ui.click("NonExistent") {
    Ok(_) => panic!("Should not find this"),
    Err(e) => println!("Expected error: {:?}", e),
}
```

#### Type Text

```rust
// Type text into the focused input
ui.typewrite("router01");

// Tap a specific key
use iced::keyboard::Key;
ui.tap_key(Key::Named(iced::keyboard::key::Named::Enter));
```

### Checking Messages

After interactions, retrieve the messages produced:

```rust
#[test]
fn test_settings_button() {
    let state = DashboardState::default();
    let mut ui = simulator(dashboard_view(&state));
    
    // Perform interaction
    let _ = ui.click("Settings");
    
    // Get all messages produced
    let messages: Vec<Message> = ui.into_messages().collect();
    
    // Verify correct message was produced
    assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));
}
```

### Testing with Mock Data

Use the `zensight::mock` module to generate realistic test data:

```rust
use zensight::mock;
use zensight::view::device::{DeviceDetailState, device_view};
use zensight::message::DeviceId;
use zensight_common::Protocol;

#[test]
fn test_device_with_metrics() {
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
    let mut state = DeviceDetailState::new(device_id);
    
    // Add mock telemetry data
    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }
    
    let mut ui = simulator(device_view(&state));
    
    // Verify metrics are displayed
    assert!(ui.find("cpu/usage").is_ok());
    assert!(ui.find("memory/used").is_ok());
}
```

### Available Mock Generators

| Function | Description | Metrics Generated |
|----------|-------------|-------------------|
| `mock::snmp::router(name)` | SNMP router | sysUpTime, sysName, ifInOctets, ifOutOctets |
| `mock::snmp::switch(name, ports)` | SNMP switch | Per-port interface metrics |
| `mock::sysinfo::host(name)` | System metrics | cpu/usage, memory/used, disk/usage, network/rx_bytes |
| `mock::syslog::messages(host)` | Syslog messages | Various facilities and severities |
| `mock::modbus::plc(name)` | Modbus PLC | holding/temperature, coil/running, input/pressure |
| `mock::mock_environment()` | Full environment | All of the above combined |

### Snapshot Testing

Take visual snapshots for regression testing:

```rust
use iced::Theme;

#[test]
fn test_dashboard_snapshot() {
    let state = DashboardState::default();
    let mut ui = simulator(dashboard_view(&state));
    
    // Take snapshot and compare with saved image
    let snapshot = ui.snapshot(&Theme::Dark).unwrap();
    assert!(snapshot.matches_image("snapshots/dashboard_empty.png").unwrap());
}
```

Snapshots are saved on first run and compared on subsequent runs.

## E2E Recording with Tester

The `tester` feature enables an interactive developer tool for recording UI tests.

### Enabling Tester

```bash
# Build with tester feature
cargo build -p zensight --features tester

# Run with tester
cargo run -p zensight --features tester
```

### Using the Tester Panel

1. **Open**: Press **F12** to toggle the tester panel
2. **Record**: Click "Record" to start recording interactions
3. **Interact**: Use the application normally - clicks, typing, etc. are recorded
4. **Stop**: Click "Stop" to end recording
5. **Save**: Save the recording as an `.ice` file

### `.ice` File Format

Recorded tests are saved as `.ice` files with a simple text format:

```
# Test: Dashboard navigation
preset: default
viewport: 1024x768

click "Settings"
wait 100
find "Zenoh Connection"
click "Back"
wait 100
find "Dashboard"
```

### Running `.ice` Tests

Use `iced_test::run()` to execute `.ice` files:

```rust
use iced_test::run;
use zensight::ZenSight;

fn main() -> Result<(), iced_test::Error> {
    run(ZenSight::default(), "tests/ice/")
}
```

### Presets

Define application presets for reproducible test environments:

```rust
impl iced::program::Program for ZenSight {
    fn presets(&self) -> &[iced::program::Preset<Self>] {
        &[
            Preset::new("empty", || ZenSight::default()),
            Preset::new("with_devices", || {
                let mut app = ZenSight::default();
                // Add mock devices
                app
            }),
        ]
    }
}
```

Reference presets in `.ice` files:

```
preset: with_devices
click "router01"
find "Device Details"
```

## Test Organization

### File Structure

```
zensight/
├── src/
│   ├── lib.rs           # Exposes modules for testing
│   ├── mock.rs          # Mock data generators
│   └── view/            # View components to test
└── tests/
    ├── ui_tests.rs      # Simulator-based tests
    └── ice/             # E2E test recordings (optional)
        ├── navigation.ice
        └── settings.ice
```

### Test Categories

| Category | File | Description |
|----------|------|-------------|
| Dashboard | `ui_tests.rs` | Empty state, device cards, navigation buttons |
| Device | `ui_tests.rs` | Metrics display, back button, filtering |
| Settings | `ui_tests.rs` | Form rendering, save functionality |
| Alerts | `ui_tests.rs` | Alert rules, acknowledgment |

## Best Practices

### 1. Test View Functions Independently

Test view functions in isolation from the full application:

```rust
// Good: Test individual view
let mut ui = simulator(dashboard_view(&state));

// Avoid: Testing through full application (slower, more fragile)
```

### 2. Use Descriptive State Setup

Make test setup clear and explicit:

```rust
#[test]
fn test_device_with_warning_status() {
    let mut state = DeviceDetailState::new(device_id);
    state.is_healthy = false;  // Explicitly set warning state
    state.last_seen = now - Duration::from_secs(120);  // Stale data
    
    let mut ui = simulator(device_view(&state));
    assert!(ui.find("Warning").is_ok());
}
```

### 3. Check Specific Messages

Be specific about which messages you expect:

```rust
// Good: Check for specific message variant
assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));

// Better: Check message content when applicable
assert!(messages.iter().any(|m| matches!(
    m, 
    Message::SelectDevice(id) if id.source == "router01"
)));
```

### 4. Test Error States

Don't forget to test error and edge cases:

```rust
#[test]
fn test_empty_metrics_list() {
    let state = DeviceDetailState::new(device_id);
    // No metrics added
    let mut ui = simulator(device_view(&state));
    assert!(ui.find("No metrics available").is_ok());
}

#[test]
fn test_disconnected_state() {
    let mut state = DashboardState::default();
    state.connected = false;
    let mut ui = simulator(dashboard_view(&state));
    assert!(ui.find("Disconnected").is_ok());
}
```

### 5. Use Mock Environment for Integration

For tests that need multiple data sources:

```rust
#[test]
fn test_multi_protocol_dashboard() {
    let mut state = DashboardState::default();
    
    for point in mock::mock_environment() {
        state.process_telemetry(point);
    }
    
    let mut ui = simulator(dashboard_view(&state));
    
    // Verify all protocols appear
    assert!(ui.find("router01").is_ok());  // SNMP
    assert!(ui.find("server01").is_ok());  // Sysinfo
    assert!(ui.find("plc01").is_ok());     // Modbus
}
```

## Troubleshooting

### Test Fails to Find Element

```
Error: SelectorNotFound { selector: "text == \"Settings\"" }
```

**Causes:**
- Element text doesn't match exactly (check spacing, case)
- Element is not rendered in current state
- Element is hidden or off-screen

**Solutions:**
- Use `ui.snapshot()` to visually inspect the rendered UI
- Check the view function to verify element is included
- Verify state is set up correctly

### Message Not Produced

```rust
let messages: Vec<Message> = ui.into_messages().collect();
assert!(messages.is_empty());  // Unexpected!
```

**Causes:**
- Click target wasn't a button
- Button's `on_press` is `None`
- Element wasn't found (check click result)

**Solutions:**
- Check that `ui.click()` returns `Ok`
- Verify button has `on_press` handler in view code
- Use `ui.find()` first to confirm element exists

### Snapshot Mismatch

```
assertion failed: snapshot.matches_image("snapshots/test.png")
```

**Causes:**
- Intentional UI changes (update snapshot)
- Font rendering differences across platforms
- Timing-dependent content

**Solutions:**
- Delete old snapshot to regenerate
- Use hash-based comparison for cross-platform tests
- Mock time-dependent values

## API Reference

### Simulator Methods

| Method | Description |
|--------|-------------|
| `find(selector)` | Find element, returns `Result<Output, Error>` |
| `click(selector)` | Click element by selector |
| `point_at(position)` | Move cursor to position |
| `tap_key(key)` | Press and release a key |
| `typewrite(text)` | Type text character by character |
| `simulate(events)` | Send raw events |
| `snapshot(theme)` | Take visual snapshot |
| `into_messages()` | Get iterator of produced messages |

### Error Types

| Error | Description |
|-------|-------------|
| `SelectorNotFound` | No element matches the selector |
| `TargetNotVisible` | Element found but not visible |

## See Also

- [Iced Testing PR #3059](https://github.com/iced-rs/iced/pull/3059) - Original testing framework implementation
- [iced_test crate documentation](https://docs.rs/iced_test)
- [ZenSight README](../README.md) - Project overview
- [CLAUDE.md](../CLAUDE.md) - Development guide
