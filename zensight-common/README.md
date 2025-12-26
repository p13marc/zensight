# zensight-common

Shared library for the ZenSight observability platform. Provides the common data model, Zenoh helpers, and configuration utilities used by all bridges and the frontend.

## Features

- **Telemetry Model** - Unified `TelemetryPoint` structure for all protocols
- **Health Model** - Device status, liveness, and bridge health types
- **Zenoh Integration** - Session management and connection helpers
- **Key Expressions** - Builder utilities for consistent key expression format
- **Serialization** - JSON and CBOR encoding/decoding
- **Configuration** - JSON5 configuration loading

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
zensight-common = { path = "../zensight-common" }
```

## Usage

### Telemetry Model

```rust
use zensight_common::{TelemetryPoint, TelemetryValue, Protocol};
use std::collections::HashMap;

let point = TelemetryPoint {
    timestamp: 1703500800000,  // Unix epoch milliseconds
    source: "router01".to_string(),
    protocol: Protocol::Snmp,
    metric: "system/sysUpTime".to_string(),
    value: TelemetryValue::Counter(123456),
    labels: HashMap::new(),
};
```

### Telemetry Values

```rust
use zensight_common::TelemetryValue;

// Counter - monotonically increasing value
let counter = TelemetryValue::Counter(1000);

// Gauge - value that can go up or down
let gauge = TelemetryValue::Gauge(45.7);

// Text - string value
let text = TelemetryValue::Text("running".to_string());

// Boolean - true/false
let boolean = TelemetryValue::Boolean(true);

// Binary - raw bytes
let binary = TelemetryValue::Binary(vec![0x01, 0x02, 0x03]);
```

### Protocols

```rust
use zensight_common::Protocol;

let protocols = [
    Protocol::Snmp,
    Protocol::Syslog,
    Protocol::Netflow,
    Protocol::Modbus,
    Protocol::Sysinfo,
    Protocol::Gnmi,
    Protocol::Opcua,
];
```

### Key Expressions

Build consistent Zenoh key expressions:

```rust
use zensight_common::keyexpr::KeyExprBuilder;

// Build a specific key
let key = KeyExprBuilder::new()
    .protocol("snmp")
    .source("router01")
    .metric("system/sysUpTime")
    .build();
// Result: "zensight/snmp/router01/system/sysUpTime"

// Wildcard for all sources of a protocol
let wildcard = KeyExprBuilder::new()
    .protocol("snmp")
    .source_wildcard()
    .build();
// Result: "zensight/snmp/*/**"

// Wildcard for all protocols
let all = zensight_common::keyexpr::all_telemetry_wildcard();
// Result: "zensight/**"
```

### Zenoh Session

```rust
use zensight_common::{ZenohConfig, connect};

let config = ZenohConfig {
    mode: "peer".to_string(),
    connect: vec![],
    listen: vec![],
};

let session = connect(&config).await?;
```

### Serialization

```rust
use zensight_common::serialization::{Format, encode, decode};

let point = TelemetryPoint { /* ... */ };

// Encode to JSON
let json_bytes = encode(&point, Format::Json)?;

// Encode to CBOR (more compact)
let cbor_bytes = encode(&point, Format::Cbor)?;

// Decode
let decoded: TelemetryPoint = decode(&json_bytes, Format::Json)?;
```

### Configuration

```rust
use zensight_common::config::load_config;
use serde::Deserialize;

#[derive(Deserialize)]
struct MyConfig {
    zenoh: ZenohConfig,
    // ... other fields
}

let config: MyConfig = load_config("config.json5")?;
```

### Health & Liveness

```rust
use zensight_common::{DeviceStatus, HealthSnapshot, DeviceLiveness};

// Device status (4-color model)
let status = DeviceStatus::Online;    // Green - responding normally
let status = DeviceStatus::Degraded;  // Orange - responding with issues
let status = DeviceStatus::Offline;   // Red - not responding
let status = DeviceStatus::Unknown;   // Gray - no data yet

// Bridge health snapshot
let health = HealthSnapshot {
    bridge: "snmp-bridge".to_string(),
    status: "healthy".to_string(),
    uptime_secs: 3600,
    devices_total: 10,
    devices_responding: 9,
    devices_failed: 1,
    last_poll_duration_ms: 150,
    errors_last_hour: 2,
    metrics_published: 5000,
};

// Per-device liveness
let liveness = DeviceLiveness {
    device: "router01".to_string(),
    status: DeviceStatus::Online,
    last_seen: 1703500800000,
    consecutive_failures: 0,
    last_error: None,
};
```

### Health Key Expressions

```rust
use zensight_common::{
    all_health_wildcard,
    all_liveness_wildcard,
    all_errors_wildcard,
    all_bridges_wildcard,
    all_correlation_wildcard,
};

// Subscribe to all bridge health snapshots
let health_key = all_health_wildcard();
// Result: "zensight/*/@/health"

// Subscribe to all device liveness updates
let liveness_key = all_liveness_wildcard();
// Result: "zensight/*/@/devices/*/liveness"

// Subscribe to error reports
let errors_key = all_errors_wildcard();
// Result: "zensight/*/@/errors"
```

## Data Model

### TelemetryPoint

| Field | Type | Description |
|-------|------|-------------|
| `timestamp` | `i64` | Unix epoch milliseconds |
| `source` | `String` | Device/host identifier |
| `protocol` | `Protocol` | Origin protocol |
| `metric` | `String` | Metric name/path |
| `value` | `TelemetryValue` | Typed value |
| `labels` | `HashMap<String, String>` | Additional context |

### Key Expression Format

All ZenSight data uses the key expression format:

```
zensight/<protocol>/<source>/<metric>
```

Examples:
- `zensight/snmp/router01/system/sysUpTime`
- `zensight/syslog/server01/daemon/warning`
- `zensight/sysinfo/host01/cpu/usage`

### Health Key Expression Format

Health and metadata use special key patterns:

```
zensight/<protocol>/@/health              # Bridge health snapshots
zensight/<protocol>/@/devices/*/liveness  # Per-device liveness
zensight/<protocol>/@/errors              # Error reports
zensight/_meta/bridges/*                  # Bridge registration
zensight/_meta/correlation/*              # Cross-bridge correlation
```

### DeviceStatus

| Status | Description |
|--------|-------------|
| `Online` | Device responding normally |
| `Degraded` | Device has issues (high latency, partial failures) |
| `Offline` | Device not responding |
| `Unknown` | No liveness data received |

### HealthSnapshot

| Field | Type | Description |
|-------|------|-------------|
| `bridge` | `String` | Bridge identifier |
| `status` | `String` | "healthy", "degraded", or "unhealthy" |
| `uptime_secs` | `u64` | Bridge uptime in seconds |
| `devices_total` | `u64` | Total configured devices |
| `devices_responding` | `u64` | Devices responding |
| `devices_failed` | `u64` | Devices not responding |
| `last_poll_duration_ms` | `u64` | Last poll cycle duration |
| `errors_last_hour` | `u64` | Error count in last hour |
| `metrics_published` | `u64` | Total metrics published |

### DeviceLiveness

| Field | Type | Description |
|-------|------|-------------|
| `device` | `String` | Device identifier |
| `status` | `DeviceStatus` | Current status |
| `last_seen` | `i64` | Last successful poll (epoch ms) |
| `consecutive_failures` | `u32` | Failed poll attempts |
| `last_error` | `Option<String>` | Most recent error message |

## Testing

```bash
cargo test -p zensight-common
```

33 tests covering:
- Telemetry model serialization
- Key expression building
- Configuration parsing
- Zenoh integration

## License

MIT OR Apache-2.0
