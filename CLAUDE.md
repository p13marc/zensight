# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
# Build all crates
cargo build --workspace

# Build release
cargo build --release --workspace

# Check compilation
cargo check --workspace

# Run tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p zensight-common

# Format and lint
cargo fmt --all
cargo clippy --workspace
```

## Architecture

ZenSight is a unified observability platform that bridges legacy monitoring protocols (SNMP, Syslog, gNMI, NetFlow, OPC UA, Modbus) into Zenoh's pub/sub infrastructure.

### Crate Structure

| Crate | Type | Purpose |
|-------|------|---------|
| `zensight-common` | Library | Shared types: telemetry model, Zenoh session helpers, JSON5 config, serialization (JSON/CBOR) |
| `zensight` | Binary | Iced 0.14 desktop frontend subscribing to `zensight/**` |
| `zenoh-bridge-snmp` | Binary | SNMP bridge with polling and trap receiver |

### Data Flow

```
[SNMP Devices] → [zenoh-bridge-snmp] → [Zenoh] → [zensight frontend]
                                              ↘ [other subscribers]
```

### Key Expression Pattern

All telemetry uses: `zensight/<protocol>/<source>/<metric>`

Examples:
- `zensight/snmp/router01/system/sysUpTime`
- `zensight/snmp/switch01/if/1/ifInOctets`

### Core Types (zensight-common)

- `TelemetryPoint` - Normalized telemetry with timestamp, source, protocol, metric, value, labels
- `TelemetryValue` - Enum: Counter(u64), Gauge(f64), Text, Boolean, Binary
- `Protocol` - Enum: Snmp, Syslog, Gnmi, Netflow, Opcua, Modbus
- `KeyExprBuilder` - Builds key expressions from protocol/source/metric
- `Format` - Serialization format: Json or Cbor

### Configuration

Bridges use JSON5 config files with:
- `zenoh` - Connection settings (mode: client/peer/router, connect/listen endpoints)
- `serialization` - "json" or "cbor"
- `logging` - Log level
- Protocol-specific settings (e.g., `snmp.devices`)

### Zenoh API Notes

Zenoh 1.0+ uses `insert_json5()` for configuration:
```rust
let mut config = zenoh::Config::default();
config.insert_json5("mode", "\"peer\"")?;
config.insert_json5("connect/endpoints", "[\"tcp/localhost:7447\"]")?;
```

### Frontend Pattern

The Iced frontend bridges Zenoh subscriptions into Iced's subscription system. It subscribes to `zensight/**` and auto-discovers all bridges/devices without configuration.

### Testing

```bash
# Run all 45 tests
cargo test --workspace

# Zenoh E2E tests require multi-thread tokio runtime
# They use unique key prefixes to avoid interference
```

### Key Files

| File | Purpose |
|------|---------|
| `zensight-common/src/telemetry.rs` | TelemetryPoint, TelemetryValue, Protocol |
| `zensight-common/src/serialization.rs` | JSON/CBOR encode/decode with auto-detection |
| `zensight-common/src/keyexpr.rs` | KeyExprBuilder and parse_key_expr |
| `zensight-common/src/session.rs` | Zenoh session connection helper |
| `zenoh-bridge-snmp/src/poller.rs` | SNMP GET/WALK polling loop |
| `zenoh-bridge-snmp/src/oid.rs` | OID parsing and name mapping |
| `zensight/src/subscription.rs` | Zenoh to Iced subscription bridge |
| `zensight/src/view/dashboard.rs` | Device grid with protocol filters |
