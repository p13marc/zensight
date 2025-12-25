# zensight

Desktop frontend application for the ZenSight observability platform. Built with [Iced 0.14](https://iced.rs/), it provides real-time visualization of telemetry from all ZenSight bridges.

## Features

- **Dashboard** - Overview of all monitored devices grouped by protocol
- **Device Details** - Detailed metrics view with time-series charts
- **Alerts** - Threshold-based alerting with rule management
- **Settings** - Zenoh connection configuration with persistence
- **SVG Icons** - Clean, scalable UI icons for all elements

## Installation

```bash
# Build release binary
cargo build -p zensight --release

# Run
./target/release/zensight
```

## Usage

### Basic

```bash
# Run with default settings (peer mode, local discovery)
zensight

# The frontend auto-discovers bridges via Zenoh subscription
```

### With Tester Feature

```bash
# Enable F12 developer tool for UI recording
cargo run -p zensight --features tester
```

Press **F12** to open the tester panel for recording and replaying UI interactions.

## Screenshots

The dashboard shows all monitored devices:
- Device name and protocol icon
- Metric count and health status
- Last seen timestamp
- Click to view details

Device detail view shows:
- All metrics with current values
- Time-series chart for selected metric
- Metric search/filter
- Export capabilities

## Architecture

```
zensight/
├── src/
│   ├── main.rs           # Binary entry point
│   ├── lib.rs            # Library for testing
│   ├── app.rs            # Iced Application implementation
│   ├── message.rs        # Message enum for Iced updates
│   ├── subscription.rs   # Zenoh → Iced subscription bridge
│   ├── mock.rs           # Mock telemetry generators
│   └── view/
│       ├── mod.rs
│       ├── dashboard.rs  # Main dashboard view
│       ├── device.rs     # Device detail view
│       ├── alerts.rs     # Alerts management
│       ├── settings.rs   # Settings page
│       ├── chart.rs      # Time-series charts
│       ├── formatting.rs # Value formatting utilities
│       └── icons/        # 24 SVG icon files
├── tests/
│   └── ui_tests.rs       # Simulator-based UI tests
└── Cargo.toml
```

## Configuration

Settings are persisted to `~/.config/zensight/settings.json5`:

```json5
{
  zenoh_mode: "peer",
  zenoh_connect: [],
  zenoh_listen: [],
  stale_threshold_secs: 60,
}
```

### Zenoh Modes

| Mode | Description |
|------|-------------|
| `peer` | Discover and connect to other peers (default) |
| `client` | Connect to specified router(s) |
| `router` | Act as a router for other nodes |

## Views

### Dashboard

- Lists all discovered devices grouped by protocol
- Shows connection status (Connected/Disconnected)
- Navigation to Settings and Alerts
- Click device card to view details

### Device Detail

- Displays all metrics for selected device
- Search/filter metrics by name
- Select metric to view time-series chart
- Chart statistics (min, max, avg, current)
- Configurable time window (1m, 5m, 15m, 1h)

### Alerts

- Create threshold-based alert rules
- Comparison operators: >, <, >=, <=, ==, !=
- Alert history with acknowledgment
- Visual indicators for active alerts

### Settings

- Zenoh connection mode selection
- Connect/listen endpoint configuration
- Stale threshold (when to mark devices as unhealthy)
- Save/load persistent settings

## Testing

```bash
# Run all tests (32 total)
cargo test -p zensight

# Run UI tests only
cargo test -p zensight --test ui_tests

# Run unit tests only
cargo test -p zensight --lib
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Dashboard | 4 | Empty state, device display, navigation |
| Device | 3 | Metrics, navigation, filtering |
| Settings | 2 | Form rendering, save functionality |
| Unit tests | 23 | Views, alerts, charts, formatting |

### Mock Data

Use mock generators for testing:

```rust
use zensight::mock;

// SNMP router metrics
let points = mock::snmp::router("router01");

// System metrics
let points = mock::sysinfo::host("server01");

// Full mock environment
let points = mock::mock_environment();
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `iced` | GUI framework (0.14) |
| `zenoh` | Pub/sub messaging |
| `zensight-common` | Shared data model |
| `tokio` | Async runtime |
| `dirs` | Platform config directories |
| `serde` | Serialization |

## Feature Flags

| Feature | Description |
|---------|-------------|
| `tester` | Enable F12 UI recorder (requires `iced/tester`) |

## Icons

24 SVG icons in `src/view/icons/`:

| Category | Icons |
|----------|-------|
| Navigation | arrow-left, arrow-up, arrow-down |
| Status | status-healthy, status-warning, status-error |
| Actions | settings, alert, chart, export, close, check, trash |
| Connection | connected, disconnected |
| Protocols | protocol-snmp, protocol-syslog, protocol-netflow, protocol-modbus, protocol-sysinfo, protocol-gnmi, protocol-opcua, protocol-generic |

Usage:
```rust
use crate::view::icons::{self, IconSize};

let icon = icons::settings::<Message>(IconSize::Medium);
let proto = icons::protocol_icon::<Message>(Protocol::Snmp, IconSize::Small);
```

## License

MIT OR Apache-2.0
