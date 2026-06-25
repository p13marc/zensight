# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

ZenSight is a unified observability platform that sensors legacy monitoring protocols into Zenoh's pub/sub infrastructure. It consists of:

1. **zensight** - Iced 0.14 desktop frontend for visualizing telemetry
2. **zensight-common** - Shared library (telemetry model, Zenoh helpers, config)
3. **zenoh-sensor-*** - Protocol sensors publishing telemetry to Zenoh
4. **zensight-exporter-*** - Exporters forwarding Zenoh telemetry to external systems

## Build Commands

```bash
# Build everything (release mode)
cargo build --release --workspace

# Build with tester feature for UI recording (F12)
cargo build -p zensight --features tester

# Run the frontend
cargo run -p zensight --release

# Run a sensor
cargo run -p zensight-sensor-snmp --release -- --config configs/snmp.json5

# Run an exporter
cargo run -p zensight-exporter-prometheus --release -- --config configs/prometheus.json5
cargo run -p zensight-exporter-otel --release -- --config configs/otel.json5
```

## Testing

```bash
# Run all tests
cargo test --workspace

# Run specific crate tests
cargo test -p zensight              # 139 tests (unit + UI)
cargo test -p zensight-common       # 47 tests
cargo test -p zensight-sensor-core  # 23 tests
cargo test -p zensight-sensor-snmp     # 22 tests
cargo test -p zensight-sensor-logs   # 106 tests (parser, receiver, filtering)
cargo test -p zensight-sensor-netflow  # 16 tests
cargo test -p zensight-sensor-modbus   # 11 tests
cargo test -p zensight-sensor-sysinfo  # 15 tests
cargo test -p zensight-sensor-gnmi     # 8 tests
cargo test -p zensight-exporter-prometheus  # 50 tests (mapping, collector, HTTP)
cargo test -p zensight-exporter-otel        # 41 tests (metrics, logs, severity)

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
│   │   ├── subscription.rs  # Zenoh subscription sensor
│   │   ├── mock.rs          # Mock telemetry generators
│   │   └── view/            # UI components
│   │       ├── dashboard.rs # Main dashboard
│   │       ├── device.rs    # Device detail view
│   │       ├── alerts.rs    # Alerts management
│   │       ├── settings.rs  # Settings page
│   │       ├── topology/    # Network topology visualization
│   │       │   ├── mod.rs   # TopologyState, node info panel
│   │       │   ├── graph.rs # Canvas-based graph rendering
│   │       │   └── layout.rs# Force-directed layout algorithm
│   │       ├── toast.rs     # Toast notification system
│   │       ├── chart.rs     # Time-series charts
│   │       ├── formatting.rs# Value formatting utilities
│   │       └── icons/       # SVG icons
│   └── tests/
│       └── ui_tests.rs      # Simulator-based UI tests
├── zensight-common/         # Shared library
│   └── src/
│       ├── telemetry.rs     # TelemetryPoint, Protocol
│       ├── health.rs        # DeviceStatus, HealthSnapshot, DeviceLiveness
│       ├── config.rs        # JSON5 config loading
│       ├── session.rs       # Zenoh session helpers
│       ├── keyexpr.rs       # Key expression builders
│       └── serialization.rs # JSON/CBOR encoding
├── zensight-sensor-core/ # Shared sensor framework
│   └── src/
│       ├── publisher.rs     # Zenoh publisher with key building
│       ├── advanced_publisher.rs # Publisher with caching registry
│       ├── health.rs        # HealthReporter with rolling error window
│       ├── correlation.rs   # Cross-sensor device correlation
│       ├── error.rs         # Sensor error categorization
│       └── liveliness.rs    # Device liveness tracking
├── zensight-sensor-snmp/       # SNMP v1/v2c/v3 sensor
├── zensight-sensor-logs/     # Logs sensor: RFC 3164/5424 (UDP/TCP/Unix) + systemd-journald
├── zensight-sensor-netflow/    # NetFlow/IPFIX sensor
├── zensight-sensor-modbus/     # Modbus TCP/RTU sensor
├── zensight-sensor-sysinfo/    # System metrics sensor
├── zensight-sensor-gnmi/       # gNMI streaming sensor
├── zensight-exporter-prometheus/  # Prometheus metrics exporter
│   └── src/
│       ├── config.rs        # Configuration parsing
│       ├── mapping.rs       # TelemetryPoint to Prometheus conversion
│       ├── collector.rs     # Metric storage with staleness
│       ├── subscriber.rs    # Zenoh subscriber
│       └── http.rs          # Axum HTTP server (/metrics endpoint)
├── zensight-exporter-otel/  # OpenTelemetry exporter
│   └── src/
│       ├── config.rs        # OTEL configuration
│       ├── metrics.rs       # TelemetryPoint to OTEL metrics
│       ├── logs.rs          # Syslog to OTEL logs
│       └── exporter.rs      # OTLP exporter setup
└── configs/                 # Example configurations
```

### Common Data Model

All sensors emit a unified `TelemetryPoint`:

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

All sensors publish to `zensight/<protocol>/<source>/<metric>`:

```
zensight/snmp/router01/system/sysUpTime
zensight/syslog/server01/daemon/warning
zensight/netflow/exporter01/10.0.0.1/10.0.0.2
zensight/modbus/plc01/holding/temperature
zensight/sysinfo/server01/cpu/usage
zensight/gnmi/router01/interfaces/interface[name=eth0]/state/counters
```

### Health & Liveness Data

Sensors also publish health/liveness metadata:

```
zensight/<protocol>/@/health              # Sensor health snapshots
zensight/<protocol>/@/devices/*/liveness  # Per-device liveness status
zensight/<protocol>/@/errors              # Error reports
zensight/_meta/sensors/*                  # Sensor registration info
zensight/_meta/correlation/*              # Cross-sensor device correlation
```

### Device Status Model

Devices display a 4-color status based on sensor liveness reports:

| Status | Color | Meaning |
|--------|-------|---------|
| Online | Green | Device responding normally |
| Degraded | Orange | Device responding with issues (high latency, partial failures) |
| Offline | Red | Device not responding |
| Unknown | Gray | No liveness data received yet |

The frontend combines local staleness detection (no data received) with sensor-reported status to determine the effective display status.

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

All sensors use JSON5 configuration. See `configs/` for examples.

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
- Sensors are auto-discovered via Zenoh

### Syslog Filtering

The syslog sensor supports message filtering at multiple levels:

**Static Filters** (configured in JSON5):
- `min_severity`: Filter by severity level (0=emergency to 7=debug)
- `include/exclude_facilities`: Filter by syslog facility
- `include/exclude_app_patterns`: Filter by app name (glob or regex)
- `include/exclude_hostname_patterns`: Filter by hostname
- `include/exclude_message_patterns`: Filter by message content

**Dynamic Filters** (via Zenoh commands at runtime):
- Command key: `zensight/syslog/@/commands/filter`
- Status queryable: `zensight/syslog/@/status`
- Commands: `add_filter`, `remove_filter`, `clear_filters`

**Frontend Integration**:
- `SyslogFilterState` in `zensight/src/view/specialized/syslog.rs`
- Filter panel with severity dropdown, facility toggles, pattern inputs
- Filters applied locally in frontend before display
- No frontend config needed to add new sensors

### SVG Icons

Icons are in `zensight/src/view/icons/` as `.svg` files loaded via `include_bytes!`:

```rust
use crate::view::icons::{self, IconSize};

let icon = icons::settings::<Message>(IconSize::Medium);
let protocol_icon = icons::protocol_icon::<Message>(Protocol::Snmp, IconSize::Small);
```

### View State Pattern

Each view has its own state struct:
- `DashboardState` - Device list, connection status, sensor health
- `DeviceDetailState` - Selected device metrics, chart data
- `AlertsState` - Alert rules, triggered alerts
- `SettingsState` - Zenoh connection settings
- `TopologyState` - Network topology graph, nodes, edges, layout

### Sensor Health Summary

The dashboard displays a health summary bar showing all connected sensors with:
- Sensor name and status (healthy/degraded/unhealthy)
- Device counts (total, responding, failed)
- Last poll duration
- Error count in the last hour

### Demo Mode

Run with `--demo` flag to simulate a full environment without real sensors:
- Generates realistic telemetry from mock devices (routers, servers, PLCs)
- Simulates periodic anomalies (CPU spikes, interface down, memory pressure)
- Publishes health snapshots and liveness updates reflecting device conditions
- Useful for UI development and demonstrations

### Network Topology View

The topology view (`view/topology/`) displays host interconnections as an interactive graph:
- Force-directed layout algorithm positions nodes automatically
- Nodes represent sysinfo hosts with CPU, memory, network metrics
- Edges show network connections with bandwidth-based thickness
- Click nodes to see info panel, "View Details" to navigate to device view
- Supports zoom, pan, search, and manual node positioning

### Observability Exporters

ZenSight includes exporters that forward Zenoh telemetry to external observability systems:

**Prometheus Exporter** (`zensight-exporter-prometheus`):
- Subscribes to `zensight/**` and exposes metrics via HTTP `/metrics` endpoint
- Converts TelemetryPoint to Prometheus types (Counter → counter, Gauge → gauge, Text → info)
- Metric name sanitization for Prometheus compatibility (valid chars: `[a-zA-Z0-9_:]`)
- Staleness-based expiry to prevent unbounded memory growth
- Configurable filtering by protocol, source, and metric patterns

**OpenTelemetry Exporter** (`zensight-exporter-otel`):
- Subscribes to `zensight/**` and exports via OTLP (gRPC or HTTP)
- Exports both metrics and logs signals
- Syslog messages converted to OTEL logs with severity mapping
- Resource attributes for service identification
- Same filtering capabilities as Prometheus exporter

Key files:
- `zensight-exporter-prometheus/src/mapping.rs` - Metric type conversion and name sanitization
- `zensight-exporter-prometheus/src/collector.rs` - Metric storage with staleness tracking
- `zensight-exporter-otel/src/logs.rs` - Syslog severity to OTEL severity mapping

## Development Notes

- Rust edition 2024 is used
- Iced 0.14 with tokio, canvas, svg features
- All async code uses tokio runtime
- Zenoh 1.0 API
- JSON5 for human-readable configs
