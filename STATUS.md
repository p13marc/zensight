# ZenSight Development Status

## Current Progress

### Phase 1: Foundation (zensight-common) - COMPLETE

| Component | Status | Tests |
|-----------|--------|-------|
| `zensight-common/src/error.rs` | Done | - |
| `zensight-common/src/telemetry.rs` | Done | 3 unit tests |
| `zensight-common/src/serialization.rs` | Done | 5 unit tests |
| `zensight-common/src/config.rs` | Done | 2 unit tests |
| `zensight-common/src/session.rs` | Done | - |
| `zensight-common/src/keyexpr.rs` | Done | 4 unit tests + 5 doc tests |
| `zensight-common/src/lib.rs` | Done | - |

**Total: 19 tests passing**

### Phase 2: SNMP Bridge (zenoh-bridge-snmp) - COMPLETE

| Component | Status | Tests |
|-----------|--------|-------|
| `zenoh-bridge-snmp/src/main.rs` | Done | - |
| `zenoh-bridge-snmp/src/config.rs` | Done | 2 unit tests |
| `zenoh-bridge-snmp/src/poller.rs` | Done | - |
| `zenoh-bridge-snmp/src/trap.rs` | Done | - |
| `zenoh-bridge-snmp/src/oid.rs` | Done | 4 unit tests |
| `configs/snmp.json5` | Done | - |

**Total: 6 tests passing**

### Phase 3: Iced Frontend (zensight) - COMPLETE

| Component | Status | Description |
|-----------|--------|-------------|
| `zensight/src/main.rs` | Done | Entry point with tracing setup |
| `zensight/src/app.rs` | Done | Iced Application implementation |
| `zensight/src/message.rs` | Done | Message types and DeviceId |
| `zensight/src/subscription.rs` | Done | Zenoh to Iced subscription bridge |
| `zensight/src/view/mod.rs` | Done | View module exports |
| `zensight/src/view/dashboard.rs` | Done | Dashboard with device grid |
| `zensight/src/view/device.rs` | Done | Device detail with metrics |

### Phase 4: Testing & Validation - COMPLETE

| Test Suite | Tests | Description |
|------------|-------|-------------|
| zensight-common unit tests | 14 | Telemetry, serialization, config, keyexpr |
| zensight-common integration | 10 | Full workflow, value types, key expressions |
| zensight-common Zenoh E2E | 4 | Pub/sub, CBOR, wildcards, multi-publisher |
| zenoh-bridge-snmp unit tests | 6 | Config parsing, OID utilities |
| zenoh-bridge-snmp integration | 6 | Telemetry encoding, key expressions |
| Doc tests | 5 | API usage examples |

**Total: 45 tests passing**

---

## What's Implemented

### zensight-common

**Telemetry Model** (`telemetry.rs`):
- `TelemetryPoint` - normalized telemetry data structure
- `TelemetryValue` - typed values (Counter, Gauge, Text, Boolean, Binary)
- `Protocol` - enum for all supported protocols

**Serialization** (`serialization.rs`):
- JSON and CBOR encoding/decoding
- Auto-detection of format from payload
- Configurable per bridge

**Configuration** (`config.rs`):
- JSON5 config file loading
- `ZenohConfig` - Zenoh connection settings (mode, connect, listen)
- `LoggingConfig` - tracing log level
- `BaseConfig` - common config shared by all bridges

**Session Management** (`session.rs`):
- `connect()` - async function to establish Zenoh session
- Supports client/peer/router modes
- Configurable endpoints

**Key Expressions** (`keyexpr.rs`):
- `KeyExprBuilder` - builds `zensight/<protocol>/<source>/<metric>` keys
- `parse_key_expr()` - extracts protocol/source/metric from key
- Wildcard builders for subscriptions

**Error Handling** (`error.rs`):
- `Error` enum with variants for Config, Zenoh, JSON, CBOR, IO, KeyExpr (thiserror)
- `Result<T>` type alias

### zenoh-bridge-snmp

**Configuration** (`config.rs`):
- `SnmpBridgeConfig` - root config with Zenoh, serialization, logging, SNMP settings
- `DeviceConfig` - per-device settings (address, community, version, OIDs)
- `OidGroup` - reusable OID groups
- SNMPv1/v2c support (v3 planned)

**OID Utilities** (`oid.rs`):
- `parse_oid()` - parse dotted string to snmp2::Oid
- `oid_to_string()` - convert Oid back to string
- `oid_starts_with()` - check OID prefix for WALK
- `OidNameMapper` - map OIDs to human-readable names with {index} patterns

**Poller** (`poller.rs`):
- Per-device async polling loop
- SNMP GET for individual OIDs
- SNMP WALK (GETNEXT) for subtrees
- Publishes `TelemetryPoint` to Zenoh

**Trap Receiver** (`trap.rs`):
- UDP listener for SNMP traps
- Basic trap notification publishing (full PDU parsing planned)

**Main** (`main.rs`):
- CLI with clap (--config flag)
- Spawns poller task per device
- Optional trap receiver
- Graceful shutdown on Ctrl+C

### zensight (Iced Frontend)

**Application** (`app.rs`):
- `ZenSight` struct implementing Iced Application pattern
- Dashboard and device detail state management
- Telemetry processing and device health tracking
- Dark theme

**Messages** (`message.rs`):
- `Message` enum for all UI and Zenoh events
- `DeviceId` type (protocol + source identifier)

**Subscription** (`subscription.rs`):
- Bridges Zenoh subscriber to Iced subscription system
- Subscribes to `zensight/**` for all telemetry
- Auto-reconnect on disconnection
- Auto-decode JSON/CBOR payloads

**Dashboard View** (`view/dashboard.rs`):
- Device grid with health status indicators
- Protocol filter buttons
- Connection status display
- Metric preview per device

**Device View** (`view/device.rs`):
- Full metrics list with values and types
- Relative timestamps ("5s ago")
- Trend arrows for numeric values
- Labels display

---

## Build & Test

```bash
# Check all crates compile
cargo check --workspace

# Run all tests
cargo test --workspace

# Run specific crate tests
cargo test -p zensight-common
cargo test -p zenoh-bridge-snmp

# Build release
cargo build --release --workspace

# Run SNMP bridge
./target/release/zenoh-bridge-snmp --config configs/snmp.json5
```

---

## Next Steps

1. Integration testing with real SNMP devices
2. Full SNMP trap PDU parsing
3. Add more protocol bridges (Syslog, gNMI, etc.)
