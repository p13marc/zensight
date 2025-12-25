# zenoh-bridge-snmp

SNMP bridge for the ZenSight observability platform. Polls SNMP devices and publishes telemetry to Zenoh.

## Features

- **SNMP v1/v2c/v3** - Full protocol version support
- **Polling** - Configurable per-device poll intervals
- **SNMP Walk** - Bulk retrieval of OID subtrees
- **Trap Receiver** - Listen for SNMP traps (UDP 162)
- **MIB Loading** - Auto-resolve OID names from MIB files
- **OID Mapping** - Manual OID-to-name configuration

## Installation

```bash
cargo build -p zenoh-bridge-snmp --release
```

## Usage

```bash
# Run with configuration file
zenoh-bridge-snmp --config configs/snmp.json5

# Run with custom config path
zenoh-bridge-snmp --config /etc/zensight/snmp.json5
```

## Configuration

Create a JSON5 configuration file:

```json5
{
  // Zenoh connection
  zenoh: {
    mode: "peer",
    connect: [],
    listen: [],
  },

  // Serialization format
  serialization: "json",  // or "cbor"

  // SNMP settings
  snmp: {
    key_prefix: "zensight/snmp",

    // Trap receiver (optional)
    trap_listener: {
      enabled: true,
      bind: "0.0.0.0:162",
    },

    // Devices to poll
    devices: [
      {
        name: "router01",
        address: "192.168.1.1:161",
        version: "v2c",
        community: "public",
        poll_interval_secs: 30,
        oids: [
          "1.3.6.1.2.1.1.3.0",  // sysUpTime
          "1.3.6.1.2.1.1.5.0",  // sysName
        ],
        walks: [
          "1.3.6.1.2.1.2.2.1",  // ifTable
        ],
      },
    ],

    // OID groups (reusable)
    oid_groups: {
      system_info: {
        oids: [
          "1.3.6.1.2.1.1.1.0",  // sysDescr
          "1.3.6.1.2.1.1.3.0",  // sysUpTime
          "1.3.6.1.2.1.1.5.0",  // sysName
        ],
        walks: [],
      },
      interfaces: {
        oids: [],
        walks: ["1.3.6.1.2.1.2.2.1"],
      },
    },

    // OID-to-name mapping
    oid_names: {
      "1.3.6.1.2.1.1.1.0": "system/sysDescr",
      "1.3.6.1.2.1.1.3.0": "system/sysUpTime",
      "1.3.6.1.2.1.1.5.0": "system/sysName",
      "1.3.6.1.2.1.2.2.1.10": "if/{index}/ifInOctets",
      "1.3.6.1.2.1.2.2.1.16": "if/{index}/ifOutOctets",
    },

    // MIB directories for auto-resolution
    mib_dirs: [
      "/usr/share/snmp/mibs",
      "./mibs",
    ],
  },

  // Logging
  logging: {
    level: "info",
  },
}
```

### SNMPv3 Configuration

```json5
{
  devices: [
    {
      name: "secure-router",
      address: "192.168.1.1:161",
      version: "v3",
      security: {
        username: "admin",
        auth_protocol: "SHA",      // MD5, SHA, SHA256
        auth_password: "authpass",
        priv_protocol: "AES",      // DES, AES, AES256
        priv_password: "privpass",
      },
      poll_interval_secs: 30,
      oids: ["1.3.6.1.2.1.1.3.0"],
    },
  ],
}
```

### Using OID Groups

Reference predefined groups in device config:

```json5
{
  devices: [
    {
      name: "switch01",
      address: "192.168.1.2:161",
      version: "v2c",
      community: "public",
      poll_interval_secs: 60,
      oid_group: "interfaces",  // Use predefined group
    },
  ],
}
```

## Key Expressions

Published telemetry uses the format:

```
zensight/snmp/<device>/<metric_path>
```

Examples:
- `zensight/snmp/router01/system/sysUpTime`
- `zensight/snmp/router01/system/sysName`
- `zensight/snmp/switch01/if/1/ifInOctets`
- `zensight/snmp/switch01/if/1/ifOutOctets`
- `zensight/snmp/switch01/if/2/ifOperStatus`

## Telemetry Format

```json
{
  "timestamp": 1703500800000,
  "source": "router01",
  "protocol": "snmp",
  "metric": "system/sysUpTime",
  "value": 123456789,
  "labels": {
    "oid": "1.3.6.1.2.1.1.3.0"
  }
}
```

## MIB Loading

The bridge can load MIB files to automatically resolve OID names:

```json5
{
  snmp: {
    mib_dirs: [
      "/usr/share/snmp/mibs",  // System MIBs
      "./mibs",                 // Local MIBs
    ],
  },
}
```

With MIB loading:
- OID `1.3.6.1.2.1.1.3.0` → `system/sysUpTime`
- OID `1.3.6.1.2.1.2.2.1.10.1` → `if/1/ifInOctets`

## Trap Handling

When `trap_listener` is enabled, the bridge:
1. Listens for SNMP traps on specified port (default: UDP 162)
2. Parses trap PDU and variable bindings
3. Publishes trap data to Zenoh

Trap key expression:
```
zensight/snmp/<source>/trap/<trap_oid>
```

## Architecture

```
zenoh-bridge-snmp/
├── src/
│   ├── main.rs      # Entry point, CLI, orchestration
│   ├── config.rs    # Configuration structs
│   ├── poller.rs    # Per-device polling task
│   ├── trap.rs      # Trap receiver
│   ├── oid.rs       # OID parsing and mapping
│   └── mib.rs       # MIB file loading
└── Cargo.toml
```

## Testing

```bash
# Run all tests (25 total)
cargo test -p zenoh-bridge-snmp

# Run with verbose output
cargo test -p zenoh-bridge-snmp -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Config | 6 | Configuration parsing |
| OID | 5 | OID parsing and mapping |
| MIB | 8 | MIB file loading |
| Integration | 6 | End-to-end polling |

## Common OIDs

| OID | Name | Description |
|-----|------|-------------|
| `1.3.6.1.2.1.1.1.0` | sysDescr | System description |
| `1.3.6.1.2.1.1.3.0` | sysUpTime | Uptime in ticks |
| `1.3.6.1.2.1.1.5.0` | sysName | System name |
| `1.3.6.1.2.1.2.2.1.10` | ifInOctets | Interface input bytes |
| `1.3.6.1.2.1.2.2.1.16` | ifOutOctets | Interface output bytes |
| `1.3.6.1.2.1.2.2.1.8` | ifOperStatus | Interface status |

## Dependencies

| Crate | Purpose |
|-------|---------|
| `rasn-snmp` | Pure Rust SNMP encoding/decoding |
| `zensight-common` | Shared data model |
| `zenoh` | Pub/sub messaging |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |

## License

MIT OR Apache-2.0
