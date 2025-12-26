# ZenSight Bridge Review

Comprehensive analysis of all protocol bridges in the ZenSight codebase.

> **Note:** All bridges are read-only by design - they are for monitoring purposes only.

---

## Executive Summary

| Bridge | Protocols | Compliance | Key Strength | Main Gap |
|--------|-----------|------------|--------------|----------|
| **SNMP** | v1, v2c, v3 (full USM) | 85% | Strong SNMPv3 auth/encryption | No GetBulk |
| **Syslog** | RFC 3164, RFC 5424 | 90% | Full structured data parsing | No TLS |
| **NetFlow** | v5, v7, v9, IPFIX | 75% | Multi-version support | No aggregation |
| **Modbus** | TCP, RTU | 85% | Endianness handling, scaling | - |
| **Sysinfo** | N/A | N/A | Extensive filtering options | No container metrics |
| **gNMI** | gNMI streaming | 85% | Full TLS, streaming telemetry | - |

---

## Priority Focus Areas

### 1. Sysinfo Bridge Improvements
### 2. Cross-Bridge Improvements

---

## 1. Sysinfo Bridge (`zenoh-bridge-sysinfo`) - PRIORITY

### Current Implementation

**Metrics Collected:**
- **CPU:** Per-core usage, frequency
- **Memory:** Used, available, swap
- **Disk:** Usage per mount point with filtering
- **Network:** Bytes/packets in/out per interface
- **System:** Uptime, load averages
- **Processes:** Top N by CPU/memory (optional, disabled by default)

**Filtering Features:**
- Network: Include/exclude lists, loopback filtering, virtual interface exclusion
- Disk: Include/exclude lists, pseudo-filesystem exclusion
- Processes: Top N configuration (default 10)

**Features:**
- Auto hostname detection
- Selective metric collection
- Extensive pseudo-filesystem filtering (tmpfs, sysfs, proc, cgroup, overlay, etc.)
- Virtual interface detection (docker*, veth*, br-*, virbr*, vnet*)
- Rate calculation for network stats

### Limitations & Improvements

| Priority | Feature | Description |
|----------|---------|-------------|
| **HIGH** | I/O wait metrics | Critical for identifying disk bottlenecks |
| **HIGH** | CPU breakdown | User/system/iowait/steal/nice time breakdown |
| **HIGH** | Container metrics | Docker container stats (CPU, memory, network per container) |
| **MEDIUM** | Temperature sensors | Thermal monitoring for hardware health |
| **MEDIUM** | Disk I/O stats | Read/write bytes, IOPS per disk |
| **MEDIUM** | File descriptor usage | Open FDs vs limits |
| **MEDIUM** | TCP connection states | ESTABLISHED, TIME_WAIT, etc. counts |
| **LOW** | GPU metrics | NVIDIA/AMD GPU utilization (requires vendor libs) |
| **LOW** | Kubernetes metrics | Pod-level resource usage |

### Detailed Feature Specs

#### I/O Wait & CPU Breakdown
```
cpu/user          - User space time %
cpu/system        - Kernel time %
cpu/iowait        - Waiting for I/O %
cpu/steal         - Stolen by hypervisor % (VMs)
cpu/nice          - Nice'd processes %
cpu/idle          - Idle time %
```

#### Container Metrics (Docker)
```
container/<name>/cpu/usage
container/<name>/memory/used
container/<name>/memory/limit
container/<name>/network/rx_bytes
container/<name>/network/tx_bytes
container/<name>/status          (running, paused, stopped)
```

#### Disk I/O Stats
```
disk/<device>/read_bytes
disk/<device>/write_bytes
disk/<device>/read_iops
disk/<device>/write_iops
disk/<device>/io_time_ms
```

#### Temperature Sensors
```
sensors/<chip>/<sensor>/temp_c
sensors/cpu/temp_c
sensors/gpu/temp_c
```

---

## 2. Cross-Bridge Improvements - PRIORITY

### Current State
- Each bridge operates independently
- No shared state or coordination
- No unified health monitoring
- No correlation between data sources

### Improvements

| Priority | Feature | Description |
|----------|---------|-------------|
| **HIGH** | Bridge health metrics | Each bridge publishes its own health to Zenoh |
| **HIGH** | Device liveness | Detect when devices stop responding |
| **HIGH** | Unified error reporting | Consistent error telemetry format |
| **MEDIUM** | Cross-bridge correlation | Link SNMP interface → NetFlow → Syslog by IP/hostname |
| **MEDIUM** | Data freshness tracking | Timestamp + staleness detection |
| **MEDIUM** | Backpressure handling | Handle slow consumers gracefully |
| **LOW** | Bridge discovery | Auto-discover running bridges via Zenoh |

### Detailed Feature Specs

#### Bridge Health Metrics
Each bridge publishes to `zensight/bridge/<bridge-name>/health`:
```
{
  "bridge": "snmp",
  "status": "healthy",           // healthy, degraded, error
  "uptime_seconds": 3600,
  "devices_total": 10,
  "devices_responding": 9,
  "devices_failed": 1,
  "last_poll_duration_ms": 150,
  "errors_last_hour": 3,
  "metrics_published": 15420
}
```

#### Device Liveness
Track per-device state:
```
zensight/<protocol>/<device>/_meta/status
{
  "status": "online",            // online, offline, degraded
  "last_seen": 1703520000000,
  "consecutive_failures": 0,
  "last_error": null
}
```

#### Unified Error Format
All bridges publish errors to `zensight/bridge/<name>/errors`:
```
{
  "timestamp": 1703520000000,
  "device": "router01",
  "error_type": "timeout",       // timeout, auth_failed, connection_refused, parse_error
  "message": "SNMP request timed out after 5000ms",
  "retryable": true
}
```

#### Cross-Bridge Correlation
Enable linking data across bridges by consistent identifiers:
- Use IP address as primary key
- Support hostname aliases
- Publish correlation hints:
```
zensight/_meta/correlation/<ip>
{
  "ip": "10.0.0.1",
  "hostnames": ["router01", "router01.local"],
  "bridges": ["snmp", "netflow", "syslog"],
  "snmp_source": "router01",
  "netflow_exporter": "10.0.0.1",
  "syslog_host": "router01"
}
```

---

## Other Bridges (Lower Priority)

### SNMP Bridge
| Priority | Feature | Description |
|----------|---------|-------------|
| MEDIUM | GetBulk support | Efficient table retrieval, 10x faster |
| LOW | Rate limiting | Prevent overloading devices |
| LOW | ASN.1 MIB compiler | Auto-convert vendor MIBs |

### Syslog Bridge
| Priority | Feature | Description |
|----------|---------|-------------|
| MEDIUM | TLS support (RFC 5425) | Encrypted syslog |
| MEDIUM | Rate limiting | Prevent log floods |
| LOW | Message filtering | Drop/sample at source |

### NetFlow Bridge
| Priority | Feature | Description |
|----------|---------|-------------|
| MEDIUM | Flow aggregation | Reduce data volume |
| LOW | Sequence validation | Detect packet loss |

### Modbus Bridge
| Priority | Feature | Description |
|----------|---------|-------------|
| MEDIUM | Reconnection logic | Exponential backoff |
| LOW | Connection keep-alive | Maintain TCP connections |

### gNMI Bridge
| Priority | Feature | Description |
|----------|---------|-------------|
| MEDIUM | Sync handling | Proper initial sync detection |
| LOW | Certificate rotation | Handle cert renewal |

---

## Implementation Roadmap

### Phase 1: Sysinfo Enhancements
1. CPU breakdown (user/system/iowait/steal)
2. Disk I/O stats (read/write bytes, IOPS)
3. I/O wait metrics
4. Temperature sensors (Linux hwmon)
5. Docker container metrics

### Phase 2: Cross-Bridge Infrastructure
1. Bridge health metrics publishing
2. Device liveness tracking
3. Unified error reporting
4. Data freshness/staleness detection

### Phase 3: Cross-Bridge Correlation
1. IP/hostname correlation registry
2. Link SNMP ↔ NetFlow ↔ Syslog data
3. Bridge discovery via Zenoh

---

## Notes

- All bridges are **read-only by design** for monitoring purposes
- Consistent use of `TelemetryPoint` model across all bridges
- Good error resilience with retry logic and reconnection handling
- JSON5 configuration format provides flexibility with comments
