# zensight

Desktop frontend application for the ZenSight observability platform. Built with [Iced 0.14](https://iced.rs/), it provides real-time visualization of telemetry from all ZenSight sensors.

## Features

- **Host/incident-centric UI** - persistent app shell (nav rail + top bar), host
  cards with composite health, and a unified Incident object (grouped alerts +
  timeline + evidence pivots)
- **Device details** - metrics table, time-series charts (booleans as 0/1 step
  series, log-rate trends), metric favorites, "alert on this metric"
- **Alerts** - threshold rules plus sensor/external alerts, severity/source
  filter pills, and saved filter presets
- **Security (NDR)** - anomaly lens with a MITRE ATT&CK by-tactic rollup and
  runtime detection tuning
- **Expectations** - author sentinel expectations pushed to the netlink sensor
- **Topology** - force-directed graph of sysinfo/netlink hosts with an alert overlay
- **Logs** - structured drill-down, MESSAGE_ID catalog, follow/pause, boot lens
- **Inventory & fingerprint explorer** - passive assets + JA3/JA4/JA4H/SNI/HASSH
- **Productivity** - command palette (Ctrl+P), fuzzy global search (Ctrl+K),
  keyboard help overlay (`?`), light/dark theme, desktop notifications
- **Local store** - redb-backed history that survives restart

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

# The frontend auto-discovers sensors via Zenoh subscription
```

### Demo Mode (No Sensors Required)

```bash
# Run with mock data - no Zenoh connection needed
zensight --demo

# Or use short flag
zensight -d
```

Demo mode is perfect for:
- Testing the UI without hardware or sensors
- Demonstrating features to users
- Development and debugging
- Learning how ZenSight works

In demo mode:
- Mock telemetry is generated for all sensors (SNMP, sysinfo, logs, Modbus, netflow, gnmi, netlink, netring)
- Data updates every 0.5-1.5 seconds with realistic variations
- All UI features work normally (alerts, charts, settings, export)

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
│   ├── store.rs         # redb-backed local store (hot ring + tiered retention)
│   └── view/
│       ├── shell.rs      # Persistent app shell (nav rail + top bar)
│       ├── dashboard.rs  # Host cards / fleet overview
│       ├── device.rs     # Device detail view
│       ├── alerts.rs / incident.rs  # Alerts + unified Incident object
│       ├── security.rs   # NDR anomaly + ATT&CK lens
│       ├── expectations.rs / inventory.rs / sensors.rs / settings.rs
│       ├── palette.rs / search.rs / help.rs  # Command palette, search, help
│       ├── topology/ specialized/ overview/ components/
│       ├── chart.rs / trend.rs / theme.rs / tokens.rs
│       └── icons/        # 44 SVG icon files
├── tests/
│   └── ui_tests.rs       # Simulator-based UI tests
└── Cargo.toml
```

See the repository root `README.md` (Frontend section) and `CLAUDE.md` for the
full view/feature map.

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

### Additional views

- **Security** - NDR anomaly lens, ATT&CK by-tactic rollup, detection tuning
- **Expectations** - author sentinel expectations for the netlink sensor
- **Topology** - force-directed host graph with an alert overlay
- **Logs** - structured log drill-down with MESSAGE_ID catalog and boot lens
- **Inventory / Incidents** - passive assets + fingerprints, grouped incidents

## Testing

```bash
# Run all tests (~330 total: unit + Simulator UI tests)
cargo test -p zensight

# Run UI tests only
cargo test -p zensight --test ui_tests

# Run unit tests only
cargo test -p zensight --lib
```

Tests cover the dashboard, device, alerts/incidents, security, topology, logs,
and settings views, plus charts, search, the command palette, formatting, and the
local store. UI tests use Iced's `Simulator` (see [`docs/UI_TESTING.md`](../docs/UI_TESTING.md)).

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

44 SVG icons in `src/view/icons/`:

| Category | Icons |
|----------|-------|
| Navigation | arrow-left/right/up/down, arrow-stable, tree, table, toggle |
| Status | status-healthy, status-warning, status-degraded, status-error, status-unknown |
| Actions | settings, alert, chart, export, edit, close, check, trash, search |
| Theme / connection | sun, moon, connected, disconnected, subscription |
| Resources | cpu, memory, disk, network, log, info |
| Protocols | protocol-snmp, protocol-syslog, protocol-netflow, protocol-modbus, protocol-sysinfo, protocol-gnmi, protocol-netlink, protocol-netring, protocol-opcua, protocol-generic |

Usage:
```rust
use crate::view::icons::{self, IconSize};

let icon = icons::settings::<Message>(IconSize::Medium);
let proto = icons::protocol_icon::<Message>(Protocol::Snmp, IconSize::Small);
```

## License

MIT OR Apache-2.0
