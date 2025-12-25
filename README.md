# Zensight

A unified observability platform that bridges legacy monitoring protocols into [Zenoh](https://zenoh.io/)'s pub/sub infrastructure.

## Components

| Crate | Description |
|-------|-------------|
| `zensight` | Iced 0.14 desktop frontend for visualizing telemetry |
| `zensight-common` | Shared library (telemetry model, Zenoh helpers, config) |
| `zenoh-bridge-snmp` | SNMP bridge (polling + trap receiver) |

## Supported Protocols

- **SNMP** (v1/v2c) - Network device monitoring
- Syslog - Log aggregation *(planned)*
- gNMI - Streaming telemetry *(planned)*
- NetFlow/IPFIX - Flow analysis *(planned)*
- OPC UA - Industrial automation *(planned)*
- Modbus - Industrial devices *(planned)*

## Key Expression Hierarchy

All bridges publish to a unified `zensight/` prefix:

```
zensight/<protocol>/<source>/<metric>
```

Examples:
- `zensight/snmp/router01/system/sysUpTime`
- `zensight/snmp/switch01/if/1/ifInOctets`
- `zensight/syslog/server01/daemon/warning`

## Quick Start

### Build

```bash
cargo build --release --workspace
```

### Run SNMP Bridge

```bash
./target/release/zenoh-bridge-snmp --config configs/snmp.json5
```

### Run Frontend

```bash
./target/release/zensight
```

## Configuration

Bridges use JSON5 configuration files. Example for SNMP:

```json5
{
  zenoh: {
    mode: "peer",
    connect: ["tcp/localhost:7447"],
  },
  serialization: "json",  // or "cbor"
  snmp: {
    devices: [
      {
        name: "router01",
        address: "192.168.1.1:161",
        community: "public",
        version: "v2c",
        poll_interval_secs: 30,
        oids: ["1.3.6.1.2.1.1.3.0"],
      },
    ],
  },
  logging: { level: "info" },
}
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for full configuration reference.

## Development

```bash
# Run tests
cargo test --workspace

# Check all crates
cargo check --workspace

# Format code
cargo fmt --all

# Lint
cargo clippy --workspace
```

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed design documentation.

See [STATUS.md](STATUS.md) for current development progress.

## License

MIT OR Apache-2.0
