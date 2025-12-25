# zensight-common

Shared library for the ZenSight observability platform. Provides the common data model, Zenoh helpers, and configuration utilities used by all bridges and the frontend.

## Features

- **Telemetry Model** - Unified `TelemetryPoint` structure for all protocols
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
