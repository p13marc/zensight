# Zensight

A unified observability platform that bridges legacy monitoring protocols into [Zenoh](https://zenoh.io/)'s pub/sub infrastructure.

## Overview

Zensight provides a suite of protocol bridges that collect telemetry from various sources and publish it to Zenoh using a unified data model. A desktop frontend allows real-time visualization of all collected metrics.

## Components

| Crate | Description | Status |
|-------|-------------|--------|
| `zensight` | Iced 0.14 desktop frontend for visualizing telemetry | Complete |
| `zensight-common` | Shared library (telemetry model, Zenoh helpers, config) | Complete |
| `zenoh-bridge-snmp` | SNMP bridge (v1/v2c/v3 polling + trap receiver, MIB loading) | Complete |
| `zenoh-bridge-syslog` | Syslog receiver (RFC 3164/5424, UDP/TCP) | Complete |
| `zenoh-bridge-netflow` | NetFlow/IPFIX receiver (v5, v7, v9, IPFIX) | Complete |
| `zenoh-bridge-modbus` | Modbus bridge (TCP/RTU, all register types) | Complete |
| `zenoh-bridge-sysinfo` | System monitoring (CPU, memory, disk, network) | Complete |

## Supported Protocols

| Protocol | Description | Key Expression |
|----------|-------------|----------------|
| **SNMP** | Network device monitoring (v1/v2c/v3) | `zensight/snmp/<device>/<oid_path>` |
| **Syslog** | Log aggregation (RFC 3164/5424) | `zensight/syslog/<host>/<facility>/<severity>` |
| **NetFlow/IPFIX** | Network flow telemetry | `zensight/netflow/<exporter>/<src>/<dst>` |
| **Modbus** | Industrial device monitoring | `zensight/modbus/<device>/<register_type>/<addr>` |
| **Sysinfo** | Host system metrics | `zensight/sysinfo/<hostname>/<metric>` |

### Planned Protocols
- gNMI - Streaming telemetry
- OPC UA - Industrial automation

## Key Expression Hierarchy

All bridges publish to a unified `zensight/` prefix:

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
```

## Quick Start

### Build

```bash
cargo build --release --workspace
```

### Run Bridges

```bash
# SNMP bridge - monitor network devices
./target/release/zenoh-bridge-snmp --config configs/snmp.json5

# Syslog bridge - collect log messages
./target/release/zenoh-bridge-syslog --config configs/syslog.json5

# NetFlow bridge - collect flow data
./target/release/zenoh-bridge-netflow --config configs/netflow.json5

# Modbus bridge - monitor industrial devices
./target/release/zenoh-bridge-modbus --config configs/modbus.json5

# Sysinfo bridge - monitor local system
./target/release/zenoh-bridge-sysinfo --config configs/sysinfo.json5
```

### Run Frontend

```bash
./target/release/zensight
```

## Configuration

All bridges use JSON5 configuration files. See the `configs/` directory for examples.

### SNMP Bridge

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

### Syslog Bridge

```json5
{
  zenoh: { mode: "peer" },
  syslog: {
    listeners: [
      { protocol: "udp", bind: "0.0.0.0:514" },
      { protocol: "tcp", bind: "0.0.0.0:514" },
    ],
  },
}
```

### NetFlow Bridge

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

### Modbus Bridge

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

### Sysinfo Bridge

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

## Data Model

All bridges emit a common `TelemetryPoint` structure:

```rust
pub struct TelemetryPoint {
    pub timestamp: i64,           // Unix epoch milliseconds
    pub source: String,           // Device/host identifier
    pub protocol: Protocol,       // snmp, syslog, netflow, modbus, sysinfo
    pub metric: String,           // Metric name/path
    pub value: TelemetryValue,    // Counter, Gauge, Text, Boolean, Binary
    pub labels: HashMap<String, String>,  // Additional context
}
```

## Development

```bash
# Run all tests (146 tests)
cargo test --workspace

# Run specific bridge tests
cargo test -p zenoh-bridge-snmp      # 25 tests
cargo test -p zenoh-bridge-syslog    # 26 tests
cargo test -p zenoh-bridge-netflow   # 8 tests
cargo test -p zenoh-bridge-modbus    # 11 tests
cargo test -p zenoh-bridge-sysinfo   # 10 tests

# Check all crates
cargo check --workspace

# Format code
cargo fmt --all

# Lint
cargo clippy --workspace
```

## Test Coverage

| Crate | Tests | Description |
|-------|-------|-------------|
| zensight-common | 14 + 10 + 4 + 5 | Unit, integration, E2E, doctests |
| zensight (frontend) | 13 | Views, alerts, charts, settings |
| zenoh-bridge-snmp | 19 + 6 | Unit + integration |
| zenoh-bridge-syslog | 26 | Parser, receiver, config |
| zenoh-bridge-netflow | 8 | Config, receiver, flow parsing |
| zenoh-bridge-modbus | 11 | Config, register decoding |
| zenoh-bridge-sysinfo | 10 | Config, filters, collection |

## License

MIT OR Apache-2.0
