# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

ZenSight is a unified observability platform that bridges legacy monitoring protocols into Zenoh's pub/sub infrastructure. It consists of:

1. **zensight** - Iced 0.14 desktop frontend for visualizing telemetry
2. **zensight-common** - Shared library (telemetry model, Zenoh helpers, config)
3. **zenoh-bridge-*** - Protocol bridges publishing telemetry to Zenoh

## Build Commands

```bash
# Build everything (release mode)
cargo build --release --workspace

# Build with tester feature for UI recording (F12)
cargo build -p zensight --features tester

# Run the frontend
cargo run -p zensight --release

# Run a bridge
cargo run -p zenoh-bridge-snmp --release -- --config configs/snmp.json5
```

## Testing

```bash
# Run all tests (187 total)
cargo test --workspace

# Run specific crate tests
cargo test -p zensight              # 32 tests (23 unit + 9 UI)
cargo test -p zensight-common       # 33 tests
cargo test -p zenoh-bridge-snmp     # 25 tests
cargo test -p zenoh-bridge-syslog   # 52 tests
cargo test -p zenoh-bridge-netflow  # 16 tests
cargo test -p zenoh-bridge-modbus   # 11 tests
cargo test -p zenoh-bridge-sysinfo  # 10 tests
cargo test -p zenoh-bridge-gnmi     # 8 tests

# Run a single test
cargo test -p zensight test_dashboard_empty
```

## Linting and Formatting

```bash
# Format code
cargo fmt --all

# Lint
cargo clippy --workspace

# Check without building
cargo check --workspace
```

## Architecture

### Crate Organization

```
zensight/                    # Workspace root
├── zensight/                # Iced 0.14 frontend
│   ├── src/
│   │   ├── main.rs          # Binary entry point
│   │   ├── lib.rs           # Library for testing
│   │   ├── app.rs           # Iced Application
│   │   ├── message.rs       # Iced messages
│   │   ├── subscription.rs  # Zenoh subscription bridge
│   │   ├── mock.rs          # Mock telemetry generators
│   │   └── view/            # UI components
│   │       ├── dashboard.rs # Main dashboard
│   │       ├── device.rs    # Device detail view
│   │       ├── alerts.rs    # Alerts management
│   │       ├── settings.rs  # Settings page
│   │       ├── chart.rs     # Time-series charts
│   │       ├── formatting.rs# Value formatting utilities
│   │       └── icons/       # SVG icons (24 files)
│   └── tests/
│       └── ui_tests.rs      # Simulator-based UI tests
├── zensight-common/         # Shared library
│   └── src/
│       ├── telemetry.rs     # TelemetryPoint, Protocol
│       ├── config.rs        # JSON5 config loading
│       ├── session.rs       # Zenoh session helpers
│       ├── keyexpr.rs       # Key expression builders
│       └── serialization.rs # JSON/CBOR encoding
├── zenoh-bridge-snmp/       # SNMP v1/v2c/v3 bridge
├── zenoh-bridge-syslog/     # RFC 3164/5424 bridge
├── zenoh-bridge-netflow/    # NetFlow/IPFIX bridge
├── zenoh-bridge-modbus/     # Modbus TCP/RTU bridge
├── zenoh-bridge-sysinfo/    # System metrics bridge
├── zenoh-bridge-gnmi/       # gNMI streaming bridge
└── configs/                 # Example configurations
```

### Common Data Model

All bridges emit a unified `TelemetryPoint`:

```rust
pub struct TelemetryPoint {
    pub timestamp: i64,           // Unix epoch milliseconds
    pub source: String,           // Device/host identifier
    pub protocol: Protocol,       // snmp, syslog, netflow, modbus, sysinfo, gnmi
    pub metric: String,           // Metric name/path
    pub value: TelemetryValue,    // Counter, Gauge, Text, Boolean, Binary
    pub labels: HashMap<String, String>,
}
```

### Key Expression Hierarchy

All bridges publish to `zensight/<protocol>/<source>/<metric>`:

```
zensight/snmp/router01/system/sysUpTime
zensight/syslog/server01/daemon/warning
zensight/netflow/exporter01/10.0.0.1/10.0.0.2
zensight/modbus/plc01/holding/temperature
zensight/sysinfo/server01/cpu/usage
zensight/gnmi/router01/interfaces/interface[name=eth0]/state/counters
```

## UI Testing

ZenSight uses Iced 0.14's testing framework for UI tests.

### Simulator Tests

The `iced_test` crate provides a `Simulator` for headless UI testing:

```rust
use iced_test::simulator;

let state = DashboardState::default();
let mut ui = simulator(dashboard_view(&state));

// Find elements by text (uses &str as Selector)
assert!(ui.find("Settings").is_ok());

// Click and check messages
let _ = ui.click("Settings");
let messages: Vec<Message> = ui.into_messages().collect();
assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));
```

### Mock Data

Use `zensight::mock` for test data:

```rust
use zensight::mock;

let snmp_points = mock::snmp::router("router01");
let sysinfo_points = mock::sysinfo::host("server01");
let all_points = mock::mock_environment();
```

### E2E Recording

Build with `--features tester` and press F12 to:
- Record UI interactions as `.ice` files
- Replay tests for regression testing
- Take visual snapshots

## Feature Flags

| Feature | Purpose |
|---------|---------|
| `tester` | Enable F12 UI recorder (iced/tester) |

## Configuration

All bridges use JSON5 configuration. See `configs/` for examples.

Common Zenoh settings:
```json5
{
  zenoh: {
    mode: "peer",              // "client", "peer", or "router"
    connect: ["tcp/localhost:7447"],
    listen: ["tcp/0.0.0.0:7448"],
  },
  serialization: "json",       // "json" or "cbor"
}
```

## Key Concepts

### Zenoh Integration

- Frontend subscribes to `zensight/**` wildcard
- Bridges are auto-discovered via Zenoh
- No frontend config needed to add new bridges

### SVG Icons

Icons are in `zensight/src/view/icons/` as `.svg` files loaded via `include_bytes!`:

```rust
use crate::view::icons::{self, IconSize};

let icon = icons::settings::<Message>(IconSize::Medium);
let protocol_icon = icons::protocol_icon::<Message>(Protocol::Snmp, IconSize::Small);
```

### View State Pattern

Each view has its own state struct:
- `DashboardState` - Device list, connection status
- `DeviceDetailState` - Selected device metrics, chart data
- `AlertsState` - Alert rules, triggered alerts
- `SettingsState` - Zenoh connection settings

## Development Notes

- Rust edition 2024 is used
- Iced 0.14 with tokio, canvas, svg features
- All async code uses tokio runtime
- Zenoh 1.0 API
- JSON5 for human-readable configs
