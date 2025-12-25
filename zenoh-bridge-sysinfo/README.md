# zenoh-bridge-sysinfo

System monitoring bridge for the ZenSight observability platform. Collects local host metrics and publishes them to Zenoh.

## Features

- **CPU Metrics** - Usage per core, frequency, load averages
- **Memory Metrics** - RAM usage, swap, available/used
- **Disk Metrics** - Usage per mount, I/O statistics
- **Network Metrics** - Bandwidth per interface, packets, errors
- **System Info** - Uptime, hostname, OS information
- **Process Metrics** - Optional top N by CPU/memory

## Installation

```bash
cargo build -p zenoh-bridge-sysinfo --release
```

## Usage

```bash
# Run with configuration file
zenoh-bridge-sysinfo --config configs/sysinfo.json5

# Run with custom config path
zenoh-bridge-sysinfo --config /etc/zensight/sysinfo.json5
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

  // Sysinfo settings
  sysinfo: {
    key_prefix: "zensight/sysinfo",

    // Hostname (use "auto" to detect)
    hostname: "auto",

    // Poll interval
    poll_interval_secs: 5,

    // What to collect
    collect: {
      cpu: true,
      memory: true,
      disk: true,
      network: true,
      system: true,
      processes: false,  // Can be heavy
    },

    // Optional filters
    filters: {
      // Only include specific disk mounts
      disk_mounts: ["/", "/home", "/var"],
      
      // Only include specific network interfaces
      network_interfaces: ["eth0", "wlan0"],
      
      // Exclude virtual interfaces
      exclude_virtual_interfaces: true,
    },
  },

  // Logging
  logging: {
    level: "info",
  },
}
```

### Minimal Configuration

```json5
{
  zenoh: { mode: "peer" },
  sysinfo: {
    poll_interval_secs: 10,
  },
}
```

### Full System Monitoring

```json5
{
  zenoh: { mode: "peer" },
  sysinfo: {
    poll_interval_secs: 5,
    collect: {
      cpu: true,
      memory: true,
      disk: true,
      network: true,
      system: true,
      processes: true,
    },
    filters: {
      process_limit: 10,  // Top 10 processes
    },
  },
}
```

## Key Expressions

Published telemetry uses the format:

```
zensight/sysinfo/<hostname>/<category>/<metric>
```

Examples:
- `zensight/sysinfo/server01/cpu/usage`
- `zensight/sysinfo/server01/cpu/0/usage`
- `zensight/sysinfo/server01/memory/used`
- `zensight/sysinfo/server01/memory/available`
- `zensight/sysinfo/server01/disk/root/usage`
- `zensight/sysinfo/server01/network/eth0/rx_bytes`
- `zensight/sysinfo/server01/system/uptime`

## Metrics Reference

### CPU Metrics

| Key | Type | Description |
|-----|------|-------------|
| `cpu/usage` | Gauge | Overall CPU usage (0-100%) |
| `cpu/<n>/usage` | Gauge | Per-core usage |
| `cpu/frequency` | Gauge | Current frequency (MHz) |
| `cpu/load_1` | Gauge | 1-minute load average |
| `cpu/load_5` | Gauge | 5-minute load average |
| `cpu/load_15` | Gauge | 15-minute load average |

### Memory Metrics

| Key | Type | Description |
|-----|------|-------------|
| `memory/total` | Counter | Total RAM (bytes) |
| `memory/used` | Gauge | Used RAM (bytes) |
| `memory/available` | Gauge | Available RAM (bytes) |
| `memory/usage` | Gauge | Memory usage (0-100%) |
| `memory/swap_total` | Counter | Total swap (bytes) |
| `memory/swap_used` | Gauge | Used swap (bytes) |

### Disk Metrics

| Key | Type | Description |
|-----|------|-------------|
| `disk/<mount>/total` | Counter | Total space (bytes) |
| `disk/<mount>/used` | Gauge | Used space (bytes) |
| `disk/<mount>/available` | Gauge | Available space (bytes) |
| `disk/<mount>/usage` | Gauge | Disk usage (0-100%) |

### Network Metrics

| Key | Type | Description |
|-----|------|-------------|
| `network/<iface>/rx_bytes` | Counter | Received bytes |
| `network/<iface>/tx_bytes` | Counter | Transmitted bytes |
| `network/<iface>/rx_packets` | Counter | Received packets |
| `network/<iface>/tx_packets` | Counter | Transmitted packets |
| `network/<iface>/rx_errors` | Counter | Receive errors |
| `network/<iface>/tx_errors` | Counter | Transmit errors |

### System Metrics

| Key | Type | Description |
|-----|------|-------------|
| `system/uptime` | Counter | System uptime (seconds) |
| `system/boot_time` | Counter | Boot timestamp (epoch) |
| `system/os_name` | Text | Operating system name |
| `system/os_version` | Text | OS version |
| `system/kernel_version` | Text | Kernel version |
| `system/hostname` | Text | System hostname |

### Process Metrics (Optional)

| Key | Type | Description |
|-----|------|-------------|
| `process/<pid>/name` | Text | Process name |
| `process/<pid>/cpu` | Gauge | CPU usage (%) |
| `process/<pid>/memory` | Gauge | Memory usage (bytes) |
| `process/<pid>/status` | Text | Process status |

## Telemetry Format

```json
{
  "timestamp": 1703500800000,
  "source": "server01",
  "protocol": "sysinfo",
  "metric": "cpu/usage",
  "value": 45.2,
  "labels": {}
}
```

For disk/network with mount/interface context:

```json
{
  "timestamp": 1703500800000,
  "source": "server01",
  "protocol": "sysinfo",
  "metric": "disk/root/usage",
  "value": 67.5,
  "labels": {
    "mount_point": "/",
    "filesystem": "ext4"
  }
}
```

## Architecture

```
zenoh-bridge-sysinfo/
├── src/
│   ├── main.rs      # Entry point, CLI, orchestration
│   ├── config.rs    # Configuration structs
│   ├── collector.rs # Metric collection task
│   ├── cpu.rs       # CPU metrics
│   ├── memory.rs    # Memory metrics
│   ├── disk.rs      # Disk metrics
│   ├── network.rs   # Network metrics
│   └── system.rs    # System info
└── Cargo.toml
```

## Testing

```bash
# Run all tests (10 total)
cargo test -p zenoh-bridge-sysinfo

# Run config tests only
cargo test -p zenoh-bridge-sysinfo config

# Run with verbose output
cargo test -p zenoh-bridge-sysinfo -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Config | 4 | Configuration validation |
| Filters | 3 | Mount/interface filtering |
| Collection | 3 | Metric gathering |

## Platform Support

The bridge uses the `sysinfo` crate which supports:

| Platform | CPU | Memory | Disk | Network | Processes |
|----------|-----|--------|------|---------|-----------|
| Linux | Full | Full | Full | Full | Full |
| macOS | Full | Full | Full | Full | Full |
| Windows | Full | Full | Full | Full | Partial |
| FreeBSD | Full | Full | Full | Full | Full |

## Deployment

### Systemd Service

```ini
# /etc/systemd/system/zenoh-bridge-sysinfo.service
[Unit]
Description=ZenSight Sysinfo Bridge
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/zenoh-bridge-sysinfo --config /etc/zensight/sysinfo.json5
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable zenoh-bridge-sysinfo
sudo systemctl start zenoh-bridge-sysinfo
```

### Docker

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build -p zenoh-bridge-sysinfo --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/zenoh-bridge-sysinfo /usr/local/bin/
COPY configs/sysinfo.json5 /etc/zensight/
CMD ["zenoh-bridge-sysinfo", "--config", "/etc/zensight/sysinfo.json5"]
```

## Performance Considerations

- **Poll Interval**: Lower intervals (< 5s) increase CPU usage
- **Process Monitoring**: Enabling processes adds significant overhead
- **Disk I/O**: Frequent disk stats can impact I/O-heavy systems
- **Network Counters**: Minimal overhead, safe to poll frequently

Recommended intervals:
| Use Case | Interval |
|----------|----------|
| Dashboard display | 5-10 seconds |
| Alerting | 10-30 seconds |
| Long-term trending | 60 seconds |

## Dependencies

| Crate | Purpose |
|-------|---------|
| `sysinfo` | Cross-platform system info |
| `zensight-common` | Shared data model |
| `zenoh` | Pub/sub messaging |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |

## License

MIT OR Apache-2.0
