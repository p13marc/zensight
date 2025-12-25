# zenoh-bridge-netflow

NetFlow/IPFIX bridge for the ZenSight observability platform. Receives network flow data and publishes it to Zenoh.

## Features

- **NetFlow v5** - Classic fixed-format flow records
- **NetFlow v7** - Catalyst switch format
- **NetFlow v9** - Template-based flexible format
- **IPFIX** - IP Flow Information Export (NetFlow v10)
- **Template Caching** - Automatic template management for v9/IPFIX

## Installation

```bash
cargo build -p zenoh-bridge-netflow --release
```

## Usage

```bash
# Run with configuration file
zenoh-bridge-netflow --config configs/netflow.json5

# Run with custom config path
zenoh-bridge-netflow --config /etc/zensight/netflow.json5
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

  // NetFlow settings
  netflow: {
    key_prefix: "zensight/netflow",

    // Listeners
    listeners: [
      { bind: "0.0.0.0:2055" },   // NetFlow standard port
      { bind: "0.0.0.0:4739" },   // IPFIX standard port
      { bind: "0.0.0.0:9995" },   // Alternative port
    ],

    // Template cache timeout (seconds)
    template_timeout_secs: 1800,  // 30 minutes
  },

  // Logging
  logging: {
    level: "info",
  },
}
```

## Key Expressions

Published telemetry uses the format:

```
zensight/netflow/<exporter>/<src_ip>/<dst_ip>
```

Examples:
- `zensight/netflow/router01/192.168.1.100/10.0.0.50`
- `zensight/netflow/switch01/10.0.1.5/8.8.8.8`
- `zensight/netflow/firewall/172.16.0.10/192.168.1.1`

## Protocol Versions

### NetFlow v5

Fixed 48-byte flow records with common fields:

| Field | Description |
|-------|-------------|
| srcaddr | Source IP address |
| dstaddr | Destination IP address |
| srcport | Source port |
| dstport | Destination port |
| prot | IP protocol |
| dPkts | Packet count |
| dOctets | Byte count |
| first | Flow start time |
| last | Flow end time |

### NetFlow v9

Template-based format with flexible fields. Common templates include:

| Template ID | Fields |
|-------------|--------|
| 256 | IPv4 flow with ports and counters |
| 257 | IPv6 flow with ports and counters |
| 258 | Interface statistics |

### IPFIX (NetFlow v10)

Enterprise-defined information elements with IANA registry:

| Element ID | Name | Description |
|------------|------|-------------|
| 8 | sourceIPv4Address | Source IPv4 |
| 12 | destinationIPv4Address | Destination IPv4 |
| 7 | sourceTransportPort | Source port |
| 11 | destinationTransportPort | Destination port |
| 4 | protocolIdentifier | IP protocol |
| 1 | octetDeltaCount | Bytes transferred |
| 2 | packetDeltaCount | Packets transferred |

## Telemetry Format

```json
{
  "timestamp": 1703500800000,
  "source": "router01",
  "protocol": "netflow",
  "metric": "192.168.1.100/10.0.0.50",
  "value": 1234567,
  "labels": {
    "version": "9",
    "src_ip": "192.168.1.100",
    "dst_ip": "10.0.0.50",
    "src_port": "45678",
    "dst_port": "443",
    "protocol": "TCP",
    "packets": "150",
    "bytes": "1234567",
    "start_time": "1703500700000",
    "end_time": "1703500800000"
  }
}
```

## Flow Fields

Common fields extracted from flows:

| Label | Description | NetFlow v5 | v9/IPFIX |
|-------|-------------|------------|----------|
| `src_ip` | Source IP | srcaddr | element 8/27 |
| `dst_ip` | Destination IP | dstaddr | element 12/28 |
| `src_port` | Source port | srcport | element 7 |
| `dst_port` | Destination port | dstport | element 11 |
| `protocol` | IP protocol name | prot | element 4 |
| `packets` | Packet count | dPkts | element 2 |
| `bytes` | Byte count | dOctets | element 1 |
| `tcp_flags` | TCP flags | tcp_flags | element 6 |

## Architecture

```
zenoh-bridge-netflow/
├── src/
│   ├── main.rs      # Entry point, CLI, orchestration
│   ├── config.rs    # Configuration structs
│   ├── receiver.rs  # UDP listener task
│   ├── v5.rs        # NetFlow v5 parsing
│   ├── v9.rs        # NetFlow v9 parsing
│   └── ipfix.rs     # IPFIX parsing
└── Cargo.toml
```

## Testing

```bash
# Run all tests (16 total)
cargo test -p zenoh-bridge-netflow

# Run parser tests only
cargo test -p zenoh-bridge-netflow v5
cargo test -p zenoh-bridge-netflow v9

# Run with verbose output
cargo test -p zenoh-bridge-netflow -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Config | 3 | Configuration validation |
| v5 Parser | 4 | NetFlow v5 decoding |
| v9 Parser | 4 | NetFlow v9 with templates |
| IPFIX | 3 | IPFIX decoding |
| Integration | 2 | End-to-end flow |

### Testing with nfgen

Generate test NetFlow packets:

```bash
# Install nfgen (or use softflowd)
apt install softflowd

# Configure softflowd to send flows
softflowd -i eth0 -n 127.0.0.1:2055 -v 5
```

### Testing with nfdump

Capture and replay NetFlow:

```bash
# Capture to file
nfcapd -p 2055 -l /tmp/flows

# Replay to bridge
nfreplay -H 127.0.0.1 -p 2055 /tmp/flows/nfcapd.*
```

## Router Configuration

### Cisco IOS

```
! Enable NetFlow v5
ip flow-export version 5
ip flow-export destination 10.0.0.100 2055
ip flow-export source Loopback0

interface GigabitEthernet0/0
 ip flow ingress
 ip flow egress
```

### Cisco IOS-XE (NetFlow v9)

```
flow exporter ZENSIGHT
 destination 10.0.0.100
 transport udp 2055
 export-protocol netflow-v9
 template data timeout 300

flow monitor MONITOR
 exporter ZENSIGHT
 record netflow ipv4 original-input

interface GigabitEthernet0/0/0
 ip flow monitor MONITOR input
 ip flow monitor MONITOR output
```

### Juniper JUNOS

```
forwarding-options {
    sampling {
        instance {
            sample1 {
                input {
                    rate 1000;
                }
                family inet {
                    output {
                        flow-server 10.0.0.100 {
                            port 2055;
                            version-ipfix;
                        }
                    }
                }
            }
        }
    }
}
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `zensight-common` | Shared data model |
| `zenoh` | Pub/sub messaging |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |
| `byteorder` | Binary parsing |

## License

MIT OR Apache-2.0
