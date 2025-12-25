# zenoh-bridge-syslog

Syslog bridge for the ZenSight observability platform. Receives syslog messages and publishes them to Zenoh.

## Features

- **RFC 3164** - BSD syslog format support
- **RFC 5424** - Modern syslog format with structured data
- **UDP/TCP** - Both transport protocols supported
- **Structured Data** - Parse SD-ELEMENT fields from RFC 5424
- **Auto-detection** - Automatically detect message format

## Installation

```bash
cargo build -p zenoh-bridge-syslog --release
```

## Usage

```bash
# Run with configuration file
zenoh-bridge-syslog --config configs/syslog.json5

# Run with custom config path
zenoh-bridge-syslog --config /etc/zensight/syslog.json5
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
zenoh-bridge-syslog/
├── src/
│   ├── main.rs      # Entry point, CLI, orchestration
│   ├── config.rs    # Configuration structs
│   ├── receiver.rs  # UDP/TCP listener tasks
│   └── parser.rs    # RFC 3164/5424 parsing
└── Cargo.toml
```

## Testing

```bash
# Run all tests (52 total)
cargo test -p zenoh-bridge-syslog

# Run parser tests only
cargo test -p zenoh-bridge-syslog parser

# Run with verbose output
cargo test -p zenoh-bridge-syslog -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Parser | 20 | RFC 3164/5424 parsing |
| Config | 6 | Configuration validation |
| Receiver | 10 | UDP/TCP handling |
| Integration | 16 | End-to-end message flow |

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

Configure rsyslog to forward to the bridge:

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

## License

MIT OR Apache-2.0
