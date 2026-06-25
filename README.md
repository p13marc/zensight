# ZenSight

A unified observability platform that sensors legacy monitoring protocols into [Zenoh](https://zenoh.io/)'s pub/sub infrastructure.

## Overview

ZenSight provides a suite of protocol sensors that collect telemetry from various sources and publish it to Zenoh using a unified data model. A desktop frontend allows real-time visualization of all collected metrics.

## Components

| Crate | Description | Status |
|-------|-------------|--------|
| `zensight` | Iced 0.14 desktop frontend for visualizing telemetry | Complete |
| `zensight-common` | Shared library (telemetry model, Zenoh helpers, config) | Complete |
| `zensight-sensor-core` | Shared sensor framework (publisher, health, correlation) | Complete |
| `zensight-exporter-prometheus` | Prometheus metrics exporter (HTTP /metrics endpoint) | Complete |
| `zensight-exporter-otel` | OpenTelemetry exporter (OTLP gRPC/HTTP) | Complete |
| `zensight-sensor-snmp` | SNMP sensor (v1/v2c/v3 polling + trap receiver, MIB loading) | Complete |
| `zensight-sensor-logs` | Logs sensor — network syslog (RFC 3164/5424, UDP/TCP/Unix, filtering) + systemd journald (known-event alerts) | Complete |
| `zensight-sensor-netflow` | NetFlow/IPFIX receiver (v5, v7, v9, IPFIX) | Complete |
| `zensight-sensor-modbus` | Modbus sensor (TCP/RTU, all register types) | Complete |
| `zensight-sensor-sysinfo` | System monitoring (CPU, memory, disk, network) | Complete |
| `zensight-sensor-gnmi` | gNMI streaming telemetry (gRPC) | Complete |
| `zensight-sensor-netlink` | Linux kernel networking (RTNETLINK/sock_diag) + sentinel expectation alerts | Complete |
| `zensight-sensor-netring` | Wire-level flow/L7/NDR (AF_PACKET/AF_XDP or pcap) + detectors & threat-intel alerts | Complete |

## Supported Protocols

| Protocol | Description | Key Expression |
|----------|-------------|----------------|
| **SNMP** | Network device monitoring (v1/v2c/v3) | `zensight/snmp/<device>/<oid_path>` |
| **Syslog / journald** | Log aggregation (RFC 3164/5424) + systemd journal | `zensight/syslog/<host>/<facility>/<severity>` |
| **NetFlow/IPFIX** | Network flow telemetry | `zensight/netflow/<exporter>/<src>/<dst>` |
| **Modbus** | Industrial device monitoring | `zensight/modbus/<device>/<register_type>/<addr>` |
| **Sysinfo** | Host system metrics | `zensight/sysinfo/<hostname>/<metric>` |
| **gNMI** | Streaming telemetry (gRPC) | `zensight/gnmi/<device>/<path>` |
| **netlink** | Linux kernel networking | `zensight/netlink/<host>/<metric>` |
| **netring** | Wire-level flow/L7/NDR | `zensight/netring/<sensor>/<metric>` |

## Documentation

Detailed documentation lives in [`docs/`](docs/):

- [Architecture](docs/ARCHITECTURE.md) — system overview, data flow, lifecycle, health model
- [Sensors](docs/SENSORS.md) — per-sensor reference (sources, config, Zenoh keys)
- [Keyspace](docs/KEYSPACE.md) — the canonical Zenoh key reference
- [UI Testing](docs/UI_TESTING.md) — frontend testing with `iced_test`

## Key Expression Hierarchy

All sensors publish to a unified `zensight/` prefix; the full key tree
(telemetry, control-plane, metadata, wildcards) is in
[docs/KEYSPACE.md](docs/KEYSPACE.md).

```
zensight/<protocol>/<source>/<metric>
```

Examples:
```
zensight/snmp/router01/system/sysUpTime
zensight/snmp/switch01/if/1/ifInOctets
zensight/syslog/server01/daemon/warning
zensight/netflow/exporter01/10.0.0.1/10.0.0.2
zensight/modbus/plc01/holding/temperature
zensight/sysinfo/server01/cpu/usage
zensight/sysinfo/server01/memory/used
zensight/gnmi/router01/interfaces/interface[name=eth0]/state/counters
```

## Quick Start

### Build

```bash
cargo build --release --workspace
```

### Run everything (recommended)

The `justfile` builds, grants capabilities, generates run configs, and launches
the GUI with the local sensors (netring, netlink, sysinfo, and **logs** via
journald). Close the GUI to stop everything.

```bash
just run                 # GUI + local sensors
just <sensor>            # one sensor: netring | netlink | sysinfo | logs
```

`just run` pins an explicit loopback rendezvous (the GUI listens on
`tcp/127.0.0.1:7447`; sensors connect to it) so the pieces always find each
other **without** relying on multicast peer discovery — which is unreliable on
hosts with a VPN or extra interfaces (tailscale, docker, …). To point the GUI or
a sensor at specific endpoints yourself, set `ZENSIGHT_ZENOH_CONNECT`,
`ZENSIGHT_ZENOH_LISTEN`, or `ZENSIGHT_ZENOH_MODE` (comma-separated endpoint
lists), which override the config.

> **Seeing no metrics/logs in the GUI?** It's almost always discovery: the GUI
> and sensors didn't form a Zenoh session. `just run` fixes this with the
> loopback rendezvous; if you launch pieces by hand, give them matching
> `connect`/`listen` endpoints (or the env vars above) instead of bare `peer`
> mode.

### Run individual sensors

```bash
# SNMP sensor - monitor network devices
./target/release/zensight-sensor-snmp --config configs/snmp.json5

# Logs sensor - network syslog, or systemd journald (see configs/logs.json5)
./target/release/zensight-sensor-logs --config configs/syslog.json5

# NetFlow sensor - collect flow data
./target/release/zensight-sensor-netflow --config configs/netflow.json5

# Modbus sensor - monitor industrial devices
./target/release/zensight-sensor-modbus --config configs/modbus.json5

# Sysinfo sensor - monitor local system
./target/release/zensight-sensor-sysinfo --config configs/sysinfo.json5

# gNMI sensor - streaming telemetry from network devices
./target/release/zensight-sensor-gnmi --config configs/gnmi.json5

# netlink sensor - Linux kernel networking
./target/release/zensight-sensor-netlink --config configs/netlink.json5

# netring sensor - wire-level flow/L7/NDR (live capture needs CAP_NET_RAW)
./target/release/zensight-sensor-netring --config configs/netring.json5
```

### Run Frontend

```bash
./target/release/zensight
```

## Configuration

All sensors use JSON5 configuration files. See the `configs/` directory for examples.

### SNMP Sensor

```json5
{
  zenoh: { mode: "peer" },
  snmp: {
    devices: [
      {
        name: "router01",
        address: "192.168.1.1:161",
        community: "public",
        version: "v2c",
        poll_interval_secs: 30,
        oids: ["1.3.6.1.2.1.1.3.0"],  // sysUpTime
        walks: ["1.3.6.1.2.1.2.2.1"], // ifTable
      },
    ],
    trap_listener: { enabled: true, bind: "0.0.0.0:162" },
  },
}
```

### Syslog Sensor

```json5
{
  zenoh: { mode: "peer" },
  syslog: {
    listeners: [
      { protocol: "udp", bind: "0.0.0.0:514" },
      { protocol: "tcp", bind: "0.0.0.0:514" },
      { protocol: "unix", bind: "/var/run/zensight-syslog.sock" },
    ],
    // Optional: message filtering
    filter: {
      min_severity: 4,  // Warning and above
      exclude_facilities: ["local7"],
      exclude_app_patterns: [
        { pattern: "systemd-*", pattern_type: "glob" },
      ],
    },
    enable_dynamic_filters: true,
  },
}
```

### NetFlow Sensor

```json5
{
  zenoh: { mode: "peer" },
  netflow: {
    listeners: [
      { bind: "0.0.0.0:2055" },  // NetFlow
      { bind: "0.0.0.0:4739" },  // IPFIX
    ],
  },
}
```

### Modbus Sensor

```json5
{
  zenoh: { mode: "peer" },
  modbus: {
    devices: [
      {
        name: "plc01",
        connection: { type: "tcp", host: "192.168.1.10", port: 502 },
        unit_id: 1,
        poll_interval_secs: 10,
        registers: [
          { type: "holding", address: 0, count: 4, name: "temperature", data_type: "f32" },
        ],
      },
    ],
  },
}
```

### Sysinfo Sensor

```json5
{
  zenoh: { mode: "peer" },
  sysinfo: {
    hostname: "auto",
    poll_interval_secs: 5,
    collect: {
      cpu: true,
      memory: true,
      disk: true,
      network: true,
      system: true,
      processes: false,
    },
  },
}
```

### gNMI Sensor

```json5
{
  zenoh: { mode: "peer" },
  gnmi: {
    targets: [
      {
        name: "router01",
        address: "192.168.1.1:9339",
        credentials: { username: "admin", password: "admin" },
        encoding: "JSON_IETF",
        subscriptions: [
          { path: "/interfaces/interface/state/counters", mode: "SAMPLE", sample_interval_ms: 10000 },
          { path: "/interfaces/interface/state/oper-status", mode: "ON_CHANGE" },
        ],
      },
    ],
  },
}
```

## Exporters

ZenSight includes exporters that subscribe to Zenoh telemetry and forward it to external observability systems.

### Prometheus Exporter

Exposes metrics via HTTP `/metrics` endpoint in Prometheus exposition format.

```json5
{
  zenoh: { mode: "peer" },
  prometheus: {
    listen: "0.0.0.0:9090",
    metrics_path: "/metrics",
    // Metric filtering
    filter: {
      include_protocols: ["snmp", "sysinfo"],
      include_sources: ["router01", "server01"],
    },
    // Aggregation settings
    aggregation: {
      staleness_secs: 300,      // Remove stale metrics after 5 minutes
      max_series: 100000,       // Memory protection
    },
  },
  serialization: "json",
}
```

Run the exporter:
```bash
./target/release/zensight-exporter-prometheus --config configs/prometheus.json5
```

### OpenTelemetry Exporter

Exports metrics and logs via OTLP (gRPC or HTTP).

```json5
{
  zenoh: { mode: "peer" },
  otel: {
    endpoint: "http://localhost:4317",  // OTLP gRPC endpoint
    protocol: "grpc",                   // "grpc" or "http"
    export_metrics: true,               // Export TelemetryPoint as metrics
    export_logs: true,                  // Export syslog messages as logs
    resource_attributes: {
      "service.name": "zensight",
      "deployment.environment": "production",
    },
    // Metric filtering (same as Prometheus)
    filter: {
      include_protocols: ["snmp", "sysinfo", "modbus"],
    },
  },
  serialization: "json",
}
```

Run the exporter:
```bash
./target/release/zensight-exporter-otel --config configs/otel.json5
```

## Data Model

All sensors emit a common `TelemetryPoint` structure:

```rust
pub struct TelemetryPoint {
    pub timestamp: i64,           // Unix epoch milliseconds
    pub source: String,           // Device/host identifier
    pub protocol: Protocol,       // snmp, syslog, netflow, modbus, sysinfo, gnmi
    pub metric: String,           // Metric name/path
    pub value: TelemetryValue,    // Counter, Gauge, Text, Boolean, Binary
    pub labels: HashMap<String, String>,  // Additional context
}
```

## Development

```bash
# Run all tests
cargo test --workspace

# Run specific crate tests
cargo test -p zensight-sensor-snmp      # 22 tests
cargo test -p zensight-sensor-logs    # 106 tests
cargo test -p zensight-sensor-netflow   # 16 tests
cargo test -p zensight-sensor-modbus    # 11 tests
cargo test -p zensight-sensor-sysinfo   # 15 tests
cargo test -p zensight-sensor-gnmi      # 8 tests

# Run frontend tests (includes UI tests with Simulator)
cargo test -p zensight               # 139 tests

# Check all crates
cargo check --workspace

# Format code
cargo fmt --all

# Lint
cargo clippy --workspace
```

## Testing

ZenSight uses Iced 0.14's testing framework for UI tests.

### Unit Tests with Simulator

The `iced_test` crate provides a `Simulator` for testing UI components without a window:

```rust
use iced_test::simulator;

let state = DashboardState::default();
let mut ui = simulator(dashboard_view(&state));

// Find elements by text
assert!(ui.find("Settings").is_ok());

// Click buttons and check messages
let _ = ui.click("Settings");
let messages: Vec<Message> = ui.into_messages().collect();
assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));
```

### E2E Recording with Tester (F12)

Build with the `tester` feature to enable the developer tool:

```bash
cargo run -p zensight --features tester
```

Press **F12** to open the tester panel, which allows you to:
- Record UI interactions as `.ice` test files
- Replay recorded tests for regression testing
- Take snapshots for visual comparison

### Mock Data

The `zensight::mock` module provides test data generators:

```rust
use zensight::mock;

// Generate SNMP router metrics
let points = mock::snmp::router("router01");

// Generate system metrics
let points = mock::sysinfo::host("server01");

// Generate a full mock environment
let points = mock::mock_environment();
```

## Test Coverage

| Crate | Tests | Description |
|-------|-------|-------------|
| zensight (frontend) | 139 | Unit + UI tests (Simulator) |
| zensight-common | 47 | Telemetry, config, key expressions |
| zensight-sensor-core | 23 | Publisher, health, correlation |
| zensight-exporter-prometheus | 50 | Metric mapping, sanitization, collector, HTTP |
| zensight-exporter-otel | 41 | OTEL metrics, logs, severity mapping |
| zensight-sensor-snmp | 22 | Polling, traps, MIB loading |
| zensight-sensor-logs | 106 | Parser, receiver, filtering |
| zensight-sensor-netflow | 16 | Flow parsing, templates |
| zensight-sensor-modbus | 11 | Config, register decoding |
| zensight-sensor-sysinfo | 15 | Config, collectors, metrics |
| zensight-sensor-gnmi | 8 | Config, path parsing, subscriber |
| **Total** | **478** | All tests passing |

## License

MIT OR Apache-2.0
