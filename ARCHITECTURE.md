# ZenSight Architecture Plan

## Overview

**ZenSight** is a unified observability platform consisting of:
1. **zensight** - Iced 0.14 desktop frontend for visualizing telemetry
2. **zensight-common** - Shared library (telemetry model, Zenoh helpers, config)
3. **zenoh-bridge-*** - Protocol bridges publishing telemetry to Zenoh

### Target Protocols
- **SNMP** (network device monitoring) - *First bridge implementation*
- Syslog (log aggregation)
- gNMI (streaming telemetry)
- NetFlow/IPFIX (flow analysis)
- OPC UA (industrial automation)
- Modbus (industrial devices)

---

## Project Structure

```
zensight/                            # Workspace root
├── Cargo.toml                       # Workspace manifest
├── zensight/                        # Iced 0.14 frontend application
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                  # Entry point
│       ├── app.rs                   # Iced Application impl
│       ├── message.rs               # Iced messages
│       ├── subscription.rs          # Zenoh subscription as Iced Subscription
│       └── view/                    # UI components
│           ├── mod.rs
│           ├── dashboard.rs
│           └── device.rs
├── zensight-common/                 # Shared library
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── config.rs                # Configuration framework (JSON5)
│       ├── keyexpr.rs               # Key expression builders
│       ├── session.rs               # Zenoh session management
│       ├── telemetry.rs             # Common telemetry data model
│       ├── serialization.rs         # JSON/CBOR encoding (configurable)
│       └── error.rs                 # Error types
├── zenoh-bridge-snmp/               # SNMP bridge (first bridge)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── config.rs                # SNMP-specific config
│       ├── poller.rs                # SNMP GET/WALK polling
│       ├── trap.rs                  # SNMP trap receiver
│       └── oid.rs                   # OID utilities and mapping
├── zenoh-bridge-syslog/             # (future)
├── zenoh-bridge-gnmi/               # (future)
├── zenoh-bridge-netflow/            # (future)
├── zenoh-bridge-opcua/              # (future)
├── zenoh-bridge-modbus/             # (future)
└── configs/
    └── snmp.json5                   # Example SNMP configuration
```

---

## Key Expression Hierarchy

All bridges publish to a unified `zensight/` prefix:

```
zensight/<protocol>/<source>/<entity>/<metric>
```

### SNMP Key Expressions

```
zensight/snmp/<device>/<oid_path>
```

**Examples:**
- `zensight/snmp/router01/system/sysUpTime`
- `zensight/snmp/switch01/if/1/ifInOctets`
- `zensight/snmp/switch01/if/1/ifOperStatus`

**OID Mapping**: OIDs are converted to readable paths via configuration or MIB lookup.

| OID | Key Expression |
|-----|----------------|
| `1.3.6.1.2.1.1.3.0` | `zensight/snmp/<device>/system/sysUpTime` |
| `1.3.6.1.2.1.2.2.1.10.1` | `zensight/snmp/<device>/if/1/ifInOctets` |

### Queryable Endpoints

Each bridge exposes:
- `zensight/snmp/<device>/**` - Query all metrics for a device
- `zensight/snmp/@/status` - Bridge health/status

---

## Common Data Model

All bridges emit normalized telemetry:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryPoint {
    pub timestamp: i64,                        // Unix epoch milliseconds
    pub source: String,                        // Device/host identifier
    pub protocol: Protocol,                    // Origin protocol
    pub metric: String,                        // Metric name/path
    pub value: TelemetryValue,                 // Typed value
    pub labels: HashMap<String, String>,       // Additional context
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TelemetryValue {
    Counter(u64),
    Gauge(f64),
    Text(String),
    Boolean(bool),
    Binary(Vec<u8>),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Snmp, Syslog, Gnmi, Netflow, Opcua, Modbus,
}
```

### Serialization

Configurable per bridge:
- **JSON** (default) - Human-readable, good for debugging
- **CBOR** - Compact binary, better for high-volume telemetry

---

## SNMP Bridge Design

### Features

1. **Multi-device polling**: Single bridge instance polls multiple SNMP devices
2. **SNMP trap receiver**: Listen for incoming traps (UDP 162)
3. **Configurable OID sets**: Define which OIDs to poll per device or device group
4. **OID-to-name mapping**: Convert numeric OIDs to readable metric names
5. **Polling intervals**: Per-device or per-OID-group intervals
6. **SNMPv1/v2c support**: (SNMPv3 planned for future)

### SNMP Configuration Example (JSON5)

```json5
{
  zenoh: {
    mode: "peer",
    connect: ["tcp/localhost:7447"],
  },
  serialization: "json",
  snmp: {
    key_prefix: "zensight/snmp",
    trap_listener: { enabled: true, bind: "0.0.0.0:162" },
    devices: [
      {
        name: "router01",
        address: "192.168.1.1:161",
        community: "public",
        version: "v2c",
        poll_interval_secs: 30,
        oids: ["1.3.6.1.2.1.1.3.0", "1.3.6.1.2.1.1.5.0"],
        walks: ["1.3.6.1.2.1.2.2.1"],
      },
    ],
    oid_groups: {
      system_info: {
        oids: ["1.3.6.1.2.1.1.1.0", "1.3.6.1.2.1.1.3.0"],
        walks: [],
      },
    },
    oid_names: {
      "1.3.6.1.2.1.1.3.0": "system/sysUpTime",
      "1.3.6.1.2.1.2.2.1.10": "if/{index}/ifInOctets",
    },
  },
  logging: { level: "info" },
}
```

---

## Iced Frontend Design

### Multi-Bridge Support

The frontend subscribes to `zensight/**` and handles telemetry from any number of bridges (including multiple instances of the same protocol type).

```
zensight/snmp/router01/...      # From bridge instance A
zensight/snmp/switch01/...      # From bridge instance A
zensight/snmp/datacenter/...    # From bridge instance B (different network)
zensight/syslog/server01/...    # From syslog bridge
```

The frontend organizes data by:
1. **Protocol** (snmp, syslog, etc.)
2. **Source** (device/host name from key expression)

No configuration needed in frontend to add new bridges - they're discovered automatically via Zenoh subscriptions.

### Views

- **Dashboard**: Grid/list of all monitored devices across all bridges, grouped by protocol
- **Device Detail**: Metrics, graphs, and raw telemetry for a selected device
- **Protocol Filter**: Filter dashboard by protocol type

---

## Dependencies

### zensight-common
- `zenoh = "1.0"`
- `tokio`, `serde`, `serde_json`, `ciborium`, `json5`
- `tracing`, `tracing-subscriber`, `thiserror`

### zensight (frontend)
- `zensight-common`
- `iced = "0.14"` with tokio feature
- `zenoh = "1.0"`

### zenoh-bridge-snmp
- `zensight-common`
- `rasn-snmp = "0.15"` (Pure Rust SNMP)
- `tokio`, `clap`

---

## Future Considerations

- **SNMPv3**: USM authentication and encryption
- **Backpressure handling**: Rate limiting when Zenoh can't keep up
- **Authentication**: Zenoh TLS/auth
- **Bidirectional operations**: SNMP SET, Modbus writes, etc.
- **MIB loading**: Automatic OID-to-name resolution from MIB files
