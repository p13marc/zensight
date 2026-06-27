# zensight-sensor-logs

Unified logs sensor for the ZenSight observability platform. Ingests network
syslog **and/or** the local systemd journal, emits one telemetry event per log
line, and publishes derived rollups and per-unit SLO alerts to Zenoh.

## Features

- **RFC 3164 / RFC 5424** - BSD and modern syslog, structured data, RFC 6587 framing
- **UDP/TCP/Unix** - Multiple transports (`/dev/log`, `/var/run/syslog.sock`)
- **journald** - read the local journal via libsystemd: scope/namespace,
  server-side matching, cursor-based gap-free resume, known-event alerts
  (coredump / unit-failed / OOM), backpressure + drop accounting
- **Per-line events** - one `events/<uid>` per line with the OpenTelemetry logs
  data model in labels (no last-writer-wins loss)
- **Multiline joining** - fold stack traces into a single event
- **Template mining** - Drain3-style streaming miner + novelty / rate-spike detection
- **Derived rollups & SLOs** - per-unit log-rate / error rollups, error budgets, burn-rate alerts
- **Message Filtering** - by severity/facility/app/hostname/message (glob+regex),
  updatable at runtime via Zenoh commands

> See [`docs/SENSORS.md#syslog--logs`](../docs/SENSORS.md#syslog--logs) for the
> authoritative reference.

## Installation

```bash
cargo build -p zensight-sensor-logs --release
```

## Usage

```bash
# Network syslog listeners (journald block commented out)
zensight-sensor-logs --config configs/syslog.json5

# journald-only (used by `just run`)
zensight-sensor-logs --config configs/logs.json5
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

  // Syslog settings
  syslog: {
    key_prefix: "zensight/syslog",

    // Listeners
    listeners: [
      { protocol: "udp", bind: "0.0.0.0:514" },
      { protocol: "tcp", bind: "0.0.0.0:514" },
    ],

    // Optional: Override hostname detection
    default_hostname: null,  // Use sender IP if hostname missing
  },

  // Logging
  logging: {
    level: "info",
  },
}
```

### Multiple Listeners

```json5
{
  syslog: {
    listeners: [
      { protocol: "udp", bind: "0.0.0.0:514" },   // Standard syslog
      { protocol: "tcp", bind: "0.0.0.0:514" },   // Reliable syslog
      { protocol: "udp", bind: "0.0.0.0:1514" },  // Non-privileged port
    ],
  },
}
```

### Unix Socket Listener

For receiving local syslog messages (e.g., from systemd-journald):

```json5
{
  syslog: {
    listeners: [
      { protocol: "udp", bind: "0.0.0.0:514" },
      { 
        protocol: "unix", 
        bind: "/var/run/zensight-syslog.sock",
        socket_mode: 438,  // 0o666 in decimal
        remove_existing_socket: true,
      },
    ],
  },
}
```

### Message Filtering

Filter messages before publishing to reduce noise:

```json5
{
  syslog: {
    // Static filter configuration
    filter: {
      // Only forward warning and above (0=emergency, 7=debug)
      min_severity: 4,
      
      // Include only specific facilities
      include_facilities: ["auth", "daemon", "kern"],
      
      // Exclude specific facilities
      exclude_facilities: ["local7"],
      
      // Exclude app name patterns (glob or regex)
      exclude_app_patterns: [
        { pattern: "systemd-*", pattern_type: "glob" },
        { pattern: "^cron$", pattern_type: "regex" },
      ],
      
      // Include only specific hostnames
      include_hostname_patterns: [
        { pattern: "prod-*", pattern_type: "glob" },
      ],
      
      // Exclude message content patterns
      exclude_message_patterns: [
        { pattern: "*HEALTHCHECK*", pattern_type: "glob" },
      ],
    },
    
    // Enable runtime filter updates via Zenoh
    enable_dynamic_filters: true,
  },
}
```

### Dynamic Filter Commands

When `enable_dynamic_filters` is enabled, filters can be updated at runtime via Zenoh:

**Command Key:** `zensight/syslog/@/commands/filter`

```json
// Add a filter
{
  "type": "add_filter",
  "id": "my-filter",
  "filter": {
    "min_severity": 3,
    "exclude_app_patterns": [
      { "pattern": "noisy-app", "pattern_type": "glob" }
    ]
  }
}

// Remove a filter
{ "type": "remove_filter", "id": "my-filter" }

// Clear all dynamic filters
{ "type": "clear_filters" }
```

**Status Query:** `zensight/syslog/@/status`

Query the current filter status including base filter, dynamic filters, and statistics.

## Key Expressions

Published telemetry uses the format:

```
zensight/syslog/<hostname>/<facility>/<severity>
```

Examples:
- `zensight/syslog/server01/daemon/warning`
- `zensight/syslog/server01/auth/info`
- `zensight/syslog/webserver/local0/error`
- `zensight/syslog/router01/kern/critical`

## Syslog Facilities

| Code | Facility | Description |
|------|----------|-------------|
| 0 | kern | Kernel messages |
| 1 | user | User-level messages |
| 2 | mail | Mail system |
| 3 | daemon | System daemons |
| 4 | auth | Security/authorization |
| 5 | syslog | Syslogd internal |
| 6 | lpr | Printer subsystem |
| 7 | news | Network news |
| 8 | uucp | UUCP subsystem |
| 9 | cron | Clock daemon |
| 10 | authpriv | Security/authorization (private) |
| 11 | ftp | FTP daemon |
| 16-23 | local0-7 | Local use |

## Syslog Severities

| Code | Severity | Description |
|------|----------|-------------|
| 0 | emerg | System is unusable |
| 1 | alert | Action must be taken immediately |
| 2 | crit | Critical conditions |
| 3 | err | Error conditions |
| 4 | warning | Warning conditions |
| 5 | notice | Normal but significant |
| 6 | info | Informational messages |
| 7 | debug | Debug-level messages |

## Message Formats

### RFC 3164 (BSD Syslog)

```
<PRI>TIMESTAMP HOSTNAME TAG: MESSAGE
```

Example:
```
<34>Oct 11 22:14:15 server01 sshd[1234]: Accepted password for user
```

### RFC 5424 (Modern Syslog)

```
<PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID [STRUCTURED-DATA] MESSAGE
```

Example:
```
<165>1 2023-12-25T12:00:00.000Z server01 myapp 1234 ID47 [exampleSDID@32473 iut="3" eventSource="Application"] Application started
```

## Telemetry Format

```json
{
  "timestamp": 1703500800000,
  "source": "server01",
  "protocol": "syslog",
  "metric": "daemon/warning",
  "value": "Connection timed out",
  "labels": {
    "facility": "daemon",
    "severity": "warning",
    "app_name": "nginx",
    "proc_id": "1234",
    "msg_id": "CONN_TIMEOUT",
    "structured_data": "{\"exampleSDID@32473\":{\"iut\":\"3\"}}"
  }
}
```

## Structured Data

RFC 5424 structured data is parsed and included in labels:

```
[exampleSDID@32473 iut="3" eventSource="Application"]
```

Becomes:
```json
{
  "labels": {
    "structured_data": "{\"exampleSDID@32473\":{\"iut\":\"3\",\"eventSource\":\"Application\"}}"
  }
}
```

## Architecture

```
zensight-sensor-logs/
├── src/
│   ├── main.rs      # Entry point, CLI, orchestration
│   ├── config.rs    # Configuration structs
│   ├── receiver.rs  # UDP/TCP/Unix listener tasks
│   ├── parser.rs    # RFC 3164/5424 parsing
│   ├── filter.rs    # Message filtering (severity, facility, patterns)
│   └── commands.rs  # Dynamic filter command protocol
└── Cargo.toml
```

## Testing

```bash
# Run all tests (52 total)
cargo test -p zensight-sensor-logs

# Run parser tests only
cargo test -p zensight-sensor-logs parser

# Run with verbose output
cargo test -p zensight-sensor-logs -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Parser | 20 | RFC 3164/5424 parsing |
| Config | 6 | Configuration validation |
| Receiver | 10 | UDP/TCP/Unix handling |
| Filter | 10 | Severity, facility, pattern matching |
| Integration | 6 | End-to-end message flow |

### Sending Test Messages

```bash
# Send UDP syslog message
echo "<34>Oct 11 22:14:15 testhost myapp: Test message" | nc -u localhost 514

# Send TCP syslog message
echo "<34>Oct 11 22:14:15 testhost myapp: Test message" | nc localhost 514

# Send RFC 5424 message
echo '<165>1 2023-12-25T12:00:00Z testhost myapp - - - Test message' | nc -u localhost 514
```

## rsyslog Integration

Configure rsyslog to forward to the sensor:

```
# /etc/rsyslog.d/50-zensight.conf

# Forward all messages via UDP
*.* @127.0.0.1:514

# Or via TCP (more reliable)
*.* @@127.0.0.1:514

# Forward specific facilities
daemon.* @127.0.0.1:514
auth,authpriv.* @127.0.0.1:514
```

## systemd-journald Integration

Forward journald to syslog:

```bash
# /etc/systemd/journald.conf
[Journal]
ForwardToSyslog=yes
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `zensight-common` | Shared data model |
| `zenoh` | Pub/sub messaging |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |
| `chrono` | Timestamp parsing |
| `regex` | Pattern matching |
| `uuid` | Dynamic filter IDs |

## License

MIT OR Apache-2.0
