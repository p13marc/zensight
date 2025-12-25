# zenoh-bridge-gnmi

gNMI (gRPC Network Management Interface) bridge for the ZenSight observability platform. Subscribes to streaming telemetry from network devices and publishes it to Zenoh.

## Features

- **gNMI Subscribe** - SAMPLE, ON_CHANGE, and TARGET_DEFINED modes
- **Multiple Targets** - Connect to multiple network devices
- **Path Wildcards** - Subscribe to paths with wildcards
- **Encoding Support** - JSON, JSON_IETF, and Protobuf encodings
- **TLS/mTLS** - Secure connections with certificate authentication

## Installation

```bash
cargo build -p zenoh-bridge-gnmi --release
```

## Usage

```bash
# Run with configuration file
zenoh-bridge-gnmi --config configs/gnmi.json5

# Run with custom config path
zenoh-bridge-gnmi --config /etc/zensight/gnmi.json5
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

  // gNMI settings
  gnmi: {
    key_prefix: "zensight/gnmi",

    // Target devices
    targets: [
      {
        name: "router01",
        address: "192.168.1.1:9339",
        
        // Authentication
        credentials: {
          username: "admin",
          password: "admin",
        },
        
        // Optional TLS settings
        tls: {
          enabled: true,
          ca_cert: "/etc/ssl/certs/ca.pem",
          // For mTLS:
          // client_cert: "/etc/ssl/certs/client.pem",
          // client_key: "/etc/ssl/private/client.key",
          skip_verify: false,
        },
        
        // Encoding preference
        encoding: "JSON_IETF",  // JSON, JSON_IETF, PROTO
        
        // Subscriptions
        subscriptions: [
          {
            path: "/interfaces/interface/state/counters",
            mode: "SAMPLE",
            sample_interval_ms: 10000,
          },
          {
            path: "/interfaces/interface/state/oper-status",
            mode: "ON_CHANGE",
          },
          {
            path: "/system/state",
            mode: "SAMPLE",
            sample_interval_ms: 60000,
          },
        ],
      },
    ],
  },

  // Logging
  logging: {
    level: "info",
  },
}
```

### Subscription Modes

| Mode | Description |
|------|-------------|
| `SAMPLE` | Periodic updates at specified interval |
| `ON_CHANGE` | Update only when value changes |
| `TARGET_DEFINED` | Device determines update frequency |

### Encoding Options

| Encoding | Description |
|----------|-------------|
| `JSON` | Standard JSON encoding |
| `JSON_IETF` | JSON with IETF model conformance |
| `PROTO` | Protobuf binary encoding |

## Key Expressions

Published telemetry uses the format:

```
zensight/gnmi/<device>/<path>
```

Examples:
- `zensight/gnmi/router01/interfaces/interface[name=eth0]/state/counters/in-octets`
- `zensight/gnmi/router01/interfaces/interface[name=eth0]/state/oper-status`
- `zensight/gnmi/router01/system/state/hostname`
- `zensight/gnmi/switch01/components/component[name=FAN1]/state/temperature`

Path keys (like `[name=eth0]`) are preserved in the key expression.

## Common gNMI Paths

### OpenConfig Interfaces

```
/interfaces/interface[name=*]/state/counters/in-octets
/interfaces/interface[name=*]/state/counters/out-octets
/interfaces/interface[name=*]/state/counters/in-errors
/interfaces/interface[name=*]/state/counters/out-errors
/interfaces/interface[name=*]/state/oper-status
/interfaces/interface[name=*]/state/admin-status
```

### OpenConfig System

```
/system/state/hostname
/system/state/current-datetime
/system/memory/state/physical
/system/memory/state/reserved
/system/cpus/cpu[index=*]/state/user
/system/cpus/cpu[index=*]/state/system
```

### OpenConfig Network Instance

```
/network-instances/network-instance[name=*]/protocols/protocol/bgp/neighbors/neighbor[neighbor-address=*]/state
/network-instances/network-instance[name=*]/protocols/protocol/ospf/areas/area[identifier=*]/state
```

### OpenConfig Components

```
/components/component[name=*]/state/temperature/instant
/components/component[name=*]/state/memory/utilized
/components/component[name=*]/fan/state/speed
```

## Telemetry Format

```json
{
  "timestamp": 1703500800000,
  "source": "router01",
  "protocol": "gnmi",
  "metric": "interfaces/interface[name=eth0]/state/counters/in-octets",
  "value": 123456789012,
  "labels": {
    "path": "/interfaces/interface[name=eth0]/state/counters/in-octets",
    "interface_name": "eth0"
  }
}
```

## Path Wildcards

Use wildcards in subscriptions:

```json5
{
  subscriptions: [
    // All interfaces
    {
      path: "/interfaces/interface[name=*]/state/counters",
      mode: "SAMPLE",
      sample_interval_ms: 10000,
    },
    // All BGP neighbors
    {
      path: "/network-instances/network-instance/protocols/protocol/bgp/neighbors/neighbor[neighbor-address=*]/state",
      mode: "ON_CHANGE",
    },
  ],
}
```

## TLS Configuration

### Server Certificate Verification

```json5
{
  tls: {
    enabled: true,
    ca_cert: "/etc/ssl/certs/ca.pem",
    skip_verify: false,
  },
}
```

### Mutual TLS (mTLS)

```json5
{
  tls: {
    enabled: true,
    ca_cert: "/etc/ssl/certs/ca.pem",
    client_cert: "/etc/ssl/certs/client.pem",
    client_key: "/etc/ssl/private/client.key",
  },
}
```

### Skip Verification (Development Only)

```json5
{
  tls: {
    enabled: true,
    skip_verify: true,  // NOT recommended for production
  },
}
```

## Architecture

```
zenoh-bridge-gnmi/
├── src/
│   ├── main.rs       # Entry point, CLI, orchestration
│   ├── config.rs     # Configuration structs
│   ├── subscriber.rs # gNMI subscription client
│   ├── path.rs       # Path parsing and conversion
│   └── tls.rs        # TLS configuration
└── Cargo.toml
```

## Testing

```bash
# Run all tests (8 total)
cargo test -p zenoh-bridge-gnmi

# Run path tests only
cargo test -p zenoh-bridge-gnmi path

# Run with verbose output
cargo test -p zenoh-bridge-gnmi -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Config | 3 | Configuration validation |
| Path | 3 | Path parsing and conversion |
| Subscriber | 2 | Subscription handling |

### Testing with gnmic

Use [gnmic](https://gnmic.kmrd.dev/) to test gNMI connectivity:

```bash
# Subscribe to interface counters
gnmic -a 192.168.1.1:9339 \
      -u admin -p admin \
      --skip-verify \
      subscribe \
      --path "/interfaces/interface/state/counters" \
      --mode sample \
      --sample-interval 10s

# Get system state
gnmic -a 192.168.1.1:9339 \
      -u admin -p admin \
      --skip-verify \
      get \
      --path "/system/state"

# Capabilities
gnmic -a 192.168.1.1:9339 \
      -u admin -p admin \
      --skip-verify \
      capabilities
```

## Device Configuration

### Cisco IOS-XR

```
grpc
 port 9339
 no-tls
 address-family ipv4
!
telemetry model-driven
 sensor-group INTERFACE
  sensor-path openconfig-interfaces:interfaces/interface/state/counters
 !
 subscription SUB1
  sensor-group-id INTERFACE sample-interval 10000
 !
!
```

### Arista EOS

```
management api gnmi
   transport grpc default
   port 9339
!
```

### Juniper JUNOS

```
set system services extension-service request-response grpc clear-text port 9339
set system services extension-service notification allow-clients address 0.0.0.0/0
```

### Nokia SR OS

```
configure system grpc admin-state enable
configure system grpc allow-unsecure-connection
configure system grpc tcp-keepalive admin-state enable
```

## Vendor-Specific Paths

### Cisco

```
Cisco-IOS-XR-infra-statsd-oper:infra-statistics/interfaces/interface/latest/generic-counters
```

### Arista

```
arista-exp:arista/exp/eos/interfaces/interface/state/counters
```

### Juniper

```
/junos/system/linecard/interface/
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tonic` | gRPC client |
| `prost` | Protocol Buffers |
| `zensight-common` | Shared data model |
| `zenoh` | Pub/sub messaging |
| `tokio` | Async runtime |
| `rustls` | TLS implementation |
| `clap` | CLI argument parsing |

## Troubleshooting

### Connection Refused

```
Error: Connection refused to 192.168.1.1:9339
```

- Verify gNMI is enabled on the device
- Check port number (common: 9339, 6030, 50051)
- Verify firewall rules

### Authentication Failed

```
Error: UNAUTHENTICATED: invalid credentials
```

- Verify username and password
- Check user has gNMI permissions
- Some devices require specific AAA configuration

### Path Not Found

```
Error: NOT_FOUND: path does not exist
```

- Verify path syntax (OpenConfig vs native)
- Use `gnmic capabilities` to check supported paths
- Some paths require specific features/licenses

### TLS Handshake Failed

```
Error: TLS handshake failed
```

- Verify CA certificate is correct
- Check certificate expiration
- For testing, try `skip_verify: true`

## License

MIT OR Apache-2.0
