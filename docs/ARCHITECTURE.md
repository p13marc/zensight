# ZenSight Architecture

This document describes the high-level architecture and component relationships in ZenSight.

## System Overview

```
                                    ZenSight Platform
┌─────────────────────────────────────────────────────────────────────────────────────┐
│                                                                                     │
│  ┌─────────────────────────────────────────────────────────────────────────────┐   │
│  │                         Protocol Sources (External)                          │   │
│  │                                                                              │   │
│  │   ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐   │   │
│  │   │  SNMP   │ │ Syslog  │ │ Sysinfo │ │ NetFlow │ │ Modbus  │ │  gNMI   │   │   │
│  │   │ Devices │ │ Sources │ │  Hosts  │ │Exporters│ │   PLCs  │ │ Routers │   │   │
│  │   └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘   │   │
│  └────────│───────────│───────────│───────────│───────────│───────────│────────┘   │
│           │           │           │           │           │           │            │
│           ▼           ▼           ▼           ▼           ▼           ▼            │
│  ┌─────────────────────────────────────────────────────────────────────────────┐   │
│  │                              Protocol Sensors                                │   │
│  │                                                                              │   │
│  │   ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           │   │
│  │   │ zenoh-      │ │ zenoh-      │ │ zenoh-      │ │ zenoh-      │           │   │
│  │   │ sensor-snmp │ │ sensor-     │ │ sensor-     │ │ sensor-     │  ...      │   │
│  │   │             │ │ syslog      │ │ sysinfo     │ │ netflow     │           │   │
│  │   └──────┬──────┘ └──────┬──────┘ └──────┬──────┘ └──────┬──────┘           │   │
│  │          │               │               │               │                  │   │
│  │          │  Uses zensight-sensor-core (SensorRunner, Publisher)        │   │
│  │          │  Uses zensight-common (TelemetryPoint, config, serialization)    │   │
│  └──────────│───────────────│───────────────│───────────────│──────────────────┘   │
│             │               │               │               │                      │
│             ▼               ▼               ▼               ▼                      │
│  ┌─────────────────────────────────────────────────────────────────────────────┐   │
│  │                                                                              │   │
│  │                         Zenoh Pub/Sub Infrastructure                         │   │
│  │                                                                              │   │
│  │   Key Expressions:                                                           │   │
│  │   ├── zensight/<protocol>/<source>/<metric>     (telemetry data)            │   │
│  │   ├── zensight/<protocol>/@/health              (sensor health)             │   │
│  │   ├── zensight/<protocol>/@/devices/*/liveness  (device liveness)           │   │
│  │   ├── zensight/<protocol>/@/errors              (error reports)             │   │
│  │   ├── zensight/_meta/sensors/*                  (sensor registration)       │   │
│  │   └── zensight/_meta/correlation/*              (device correlation)        │   │
│  │                                                                              │   │
│  └───────────────────────────────┬──────────────────────────────────────────────┘   │
│                                  │                                                  │
│             ┌────────────────────┼────────────────────┐                            │
│             │                    │                    │                            │
│             ▼                    ▼                    ▼                            │
│  ┌───────────────────┐ ┌─────────────────┐ ┌─────────────────────┐                 │
│  │                   │ │                 │ │                     │                 │
│  │   ZenSight GUI    │ │   Prometheus    │ │   OpenTelemetry     │                 │
│  │   (Iced 0.14)     │ │   Exporter      │ │   Exporter          │                 │
│  │                   │ │                 │ │                     │                 │
│  │  ┌─────────────┐  │ │  /metrics       │ │  OTLP (gRPC/HTTP)   │                 │
│  │  │ Dashboard   │  │ │  endpoint       │ │  → metrics + logs   │                 │
│  │  │ Device View │  │ │                 │ │                     │                 │
│  │  │ Topology    │  │ └────────┬────────┘ └──────────┬──────────┘                 │
│  │  │ Alerts      │  │          │                     │                            │
│  │  │ Settings    │  │          ▼                     ▼                            │
│  │  └─────────────┘  │   ┌────────────┐        ┌────────────┐                      │
│  │                   │   │ Prometheus │        │ OTEL       │                      │
│  └───────────────────┘   │ Server     │        │ Backends   │                      │
│                          └────────────┘        └────────────┘                      │
│                                                                                     │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

## Crate Dependencies

```
┌─────────────────────────────────────────────────────────────────────────────────────┐
│                              Workspace Crates                                       │
├─────────────────────────────────────────────────────────────────────────────────────┤
│                                                                                     │
│   ┌─────────────────────────────────────────────────────────────────────────────┐  │
│   │                          Shared Libraries                                    │  │
│   │                                                                              │  │
│   │   ┌────────────────────────────┐  ┌────────────────────────────────────┐    │  │
│   │   │      zensight-common       │  │    zensight-sensor-core       │    │  │
│   │   │                            │  │                                    │    │  │
│   │   │  • TelemetryPoint          │  │  • SensorRunner                    │    │  │
│   │   │  • TelemetryValue          │◄─┤  • Publisher                       │    │  │
│   │   │  • Protocol enum           │  │  • LivelinessManager               │    │  │
│   │   │  • DeviceStatus            │  │  • HealthSnapshot publishing       │    │  │
│   │   │  • HealthSnapshot          │  │  • CorrelationRegistry             │    │  │
│   │   │  • KeyExprBuilder          │  │                                    │    │  │
│   │   │  • Config loading          │  └──────────────────────────────────────┘  │  │
│   │   │  • Serialization           │                                           │  │
│   │   └────────────────────────────┘                                           │  │
│   │              ▲                                                              │  │
│   └──────────────│──────────────────────────────────────────────────────────────┘  │
│                  │                                                                  │
│   ┌──────────────┴───────────────────────────────────────────────────────────────┐ │
│   │                              Applications                                     │ │
│   │                                                                               │ │
│   │  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐  │ │
│   │  │   zensight    │  │ zenoh-sensor- │  │ zensight-     │  │ zensight-     │  │ │
│   │  │   (frontend)  │  │ *             │  │ exporter-     │  │ exporter-     │  │ │
│   │  │               │  │               │  │ prometheus    │  │ otel          │  │ │
│   │  │  Iced 0.14    │  │  SNMP         │  │               │  │               │  │ │
│   │  │  GUI          │  │  Syslog       │  │  HTTP         │  │  OTLP         │  │ │
│   │  │               │  │  Sysinfo      │  │  /metrics     │  │  gRPC/HTTP    │  │ │
│   │  │               │  │  NetFlow      │  │               │  │               │  │ │
│   │  │               │  │  Modbus       │  │               │  │               │  │ │
│   │  │               │  │  gNMI         │  │               │  │               │  │ │
│   │  └───────────────┘  └───────────────┘  └───────────────┘  └───────────────┘  │ │
│   │                                                                               │ │
│   └───────────────────────────────────────────────────────────────────────────────┘ │
│                                                                                     │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

## Data Flow

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                                 Data Flow                                        │
├──────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│  1. COLLECTION                                                                   │
│  ═════════════                                                                   │
│                                                                                  │
│     External Device          Sensor                      Zenoh                   │
│     ───────────────          ──────                      ─────                   │
│                                                                                  │
│     ┌───────────┐     poll   ┌───────────────┐  publish  ┌──────────────────┐   │
│     │   SNMP    │──────────▶│zenoh-sensor-  │──────────▶│ zensight/snmp/   │   │
│     │   Agent   │    GET     │snmp           │           │ router01/        │   │
│     └───────────┘            └───────────────┘           │ system/sysUpTime │   │
│                                                          └──────────────────┘   │
│                                                                                  │
│     ┌───────────┐   UDP/TCP  ┌───────────────┐  publish  ┌──────────────────┐   │
│     │  Syslog   │──────────▶│zenoh-sensor-  │──────────▶│ zensight/syslog/ │   │
│     │  Source   │   514      │syslog         │           │ server01/...     │   │
│     └───────────┘            └───────────────┘           └──────────────────┘   │
│                                                                                  │
│  2. COMMON DATA MODEL                                                            │
│  ════════════════════                                                            │
│                                                                                  │
│     All sensors normalize data into TelemetryPoint:                              │
│                                                                                  │
│     ┌────────────────────────────────────────────────────────────────────────┐  │
│     │  TelemetryPoint {                                                       │  │
│     │      timestamp: 1704412800000,        // Unix epoch ms                  │  │
│     │      source: "router01",              // Device identifier              │  │
│     │      protocol: Protocol::Snmp,        // Origin protocol                │  │
│     │      metric: "system/sysUpTime",      // Metric path                    │  │
│     │      value: TelemetryValue::Counter(123456),                            │  │
│     │      labels: {"location": "dc1", "vendor": "cisco"},                    │  │
│     │  }                                                                      │  │
│     └────────────────────────────────────────────────────────────────────────┘  │
│                                                                                  │
│  3. CONSUMPTION                                                                  │
│  ══════════════                                                                  │
│                                                                                  │
│     Zenoh                           Consumer                                     │
│     ─────                           ────────                                     │
│                                                                                  │
│     zensight/**  ──subscribe──▶  ┌─────────────────────────────────────────┐    │
│                                  │  ZenSight Frontend                       │    │
│                                  │  • Displays in Dashboard/Device views    │    │
│                                  │  • Tracks device health & liveness       │    │
│                                  │  • Builds topology graph                 │    │
│                                  └─────────────────────────────────────────┘    │
│                                                                                  │
│     zensight/**  ──subscribe──▶  ┌─────────────────────────────────────────┐    │
│                                  │  Prometheus Exporter                     │    │
│                                  │  • Converts to Prometheus metrics        │    │
│                                  │  • Exposes /metrics HTTP endpoint        │    │
│                                  └─────────────────────────────────────────┘    │
│                                                                                  │
│     zensight/**  ──subscribe──▶  ┌─────────────────────────────────────────┐    │
│                                  │  OpenTelemetry Exporter                  │    │
│                                  │  • Exports metrics via OTLP              │    │
│                                  │  • Converts syslog to OTEL logs          │    │
│                                  └─────────────────────────────────────────┘    │
│                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

## Key Expression Hierarchy

Telemetry is `zensight/<protocol>/<source>/<metric>`; per-sensor control-plane
lives under `zensight/<protocol>/@/…`; cross-sensor metadata under
`zensight/_meta/…`.

```
zensight/
├── <protocol>/                          # snmp, syslog, netflow, modbus, sysinfo, gnmi, netlink, netring
│   ├── <source>/<metric_path>           # telemetry — TelemetryPoint
│   │       Example: zensight/snmp/router01/interfaces/eth0/ifInOctets
│   └── @/                               # control-plane (verbatim @ — wildcards don't cross it)
│       ├── health · errors · status · alive
│       ├── devices/<device>/{liveness,alive}
│       ├── alerts/<alert_key>           # Alert (firing/resolved)
│       ├── query/{alerts,<topic>}       # firing-set seed + on-demand detail
│       └── {commands,status}/<topic>    # runtime control
└── _meta/{sensors/<name>, correlation/<ip>}
```

**[KEYSPACE.md](KEYSPACE.md) is the canonical, exhaustive reference** — every key,
which sensors use it, the wildcards, and the key-building helpers. Keep that
document authoritative; this is only a sketch.

## Zenoh Transport & Pub/Sub Model

Every process (sensors, frontend, exporters) is an independent Zenoh app; how
they connect and how they publish/subscribe both matter for telemetry to flow.

### Connectivity — peers + an explicit local rendezvous

All processes run in `mode: "peer"`. Peers can discover each other two ways:

- **Multicast scouting** (Zenoh default) — works when every process shares a
  multicast-capable interface. It is **unreliable** on hosts with a VPN or
  several interfaces (tailscale, docker, …), where scouting may bind to the
  wrong interface and the GUI then forms no session and shows nothing.
- **Explicit endpoints** (`connect` / `listen`) — deterministic, no multicast.

`just run` therefore pins an explicit **loopback rendezvous**: the GUI
`listen`s on `tcp/127.0.0.1:7447` and every sensor `connect`s to it, so the
pieces always meet regardless of the network. This is driven by environment
overrides applied on top of the file/settings config:

| Env var | Effect |
|---------|--------|
| `ZENSIGHT_ZENOH_MODE` | override `mode` |
| `ZENSIGHT_ZENOH_CONNECT` | override `connect` endpoints (comma-separated) |
| `ZENSIGHT_ZENOH_LISTEN` | override `listen` endpoints (comma-separated) |

Implemented by `ZenohConfig::with_env_overrides()` (zensight-common), applied in
both the sensor session (`session::connect`) and the GUI.

### Publish/subscribe pairing — advanced telemetry, plain control-plane

The two key subtrees use **different** pub/sub machinery, and the publisher must
match the subscriber:

| Subtree | Publisher | Subscriber (frontend) |
|---------|-----------|-----------------------|
| **Telemetry** `zensight/**` | zenoh-ext **`AdvancedPublisher`** (per-key cache + miss/publisher detection) | zenoh-ext **`AdvancedSubscriber`** (`history` + `recovery` + late-publisher detection) |
| **Control-plane** `zensight/<proto>/@/…` | plain `put` / `delete` | plain subscriber on `zensight/*/@/**` |

- **Telemetry** flows through the base `Publisher`, which routes
  `publish`/`publish_to_key`/`publish_batch` through an
  `AdvancedPublisherRegistry` (one advanced publisher per key, created on first
  use, shared across `Publisher` clones). This matches the GUI's
  `AdvancedSubscriber` so delivery and late-joiner **history/recovery** work for
  **every** sensor — an advanced subscriber must be fed by an advanced publisher.
- **Control-plane** (`health`, `errors`, `alerts`, `liveness`, `status`,
  `commands`, `query`) is plain `put`/`delete`. The GUI reads it with a separate
  **plain** subscriber on `zensight/*/@/**` — necessary because the telemetry
  wildcard `zensight/**` does **not** match `@/` chunks (Zenoh treats a chunk
  starting with `@` verbatim; `*`/`**` never cross into it).

> **Symptom → cause:** "the GUI shows no metrics/logs" is almost always one of
> these two — discovery (no session formed) or a plain-`put` telemetry publisher
> that doesn't pair with the advanced subscriber. Both are addressed above.

## Frontend Architecture

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                            ZenSight Frontend (Iced 0.14)                         │
├──────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│   ┌────────────────────────────────────────────────────────────────────────┐    │
│   │                           Main Application                              │    │
│   │                                                                         │    │
│   │   ┌─────────────────┐      ┌─────────────────┐      ┌───────────────┐  │    │
│   │   │   ZenSight      │      │   Message       │      │   Views       │  │    │
│   │   │   (app.rs)      │◄────▶│   (message.rs)  │◄────▶│   (view/)     │  │    │
│   │   │                 │      │                 │      │               │  │    │
│   │   │  boot()         │      │  Telemetry      │      │  dashboard    │  │    │
│   │   │  update()       │      │  Health         │      │  device       │  │    │
│   │   │  view()         │      │  Liveness       │      │  alerts       │  │    │
│   │   │  subscription() │      │  UI events      │      │  settings     │  │    │
│   │   └────────┬────────┘      │  Keyboard       │      │  topology     │  │    │
│   │            │               │  Tick           │      └───────────────┘  │    │
│   │            │               └─────────────────┘                          │    │
│   └────────────│────────────────────────────────────────────────────────────┘    │
│                │                                                                  │
│                ▼                                                                  │
│   ┌────────────────────────────────────────────────────────────────────────┐    │
│   │                        Subscriptions (subscription.rs)                  │    │
│   │                                                                         │    │
│   │   ┌─────────────────────────────────────────────────────────────────┐  │    │
│   │   │  Zenoh Subscriber                                                │  │    │
│   │   │  • zensight/** (wildcard for all telemetry)                     │  │    │
│   │   │  • History recovery for late joiners                             │  │    │
│   │   │  • Late publisher detection                                      │  │    │
│   │   └─────────────────────────────────────────────────────────────────┘  │    │
│   │                                                                         │    │
│   │   ┌─────────────────────────────────────────────────────────────────┐  │    │
│   │   │  Liveliness Subscriber                                           │  │    │
│   │   │  • Sensor presence: zensight/<protocol>/@/alive                  │  │    │
│   │   │  • Device presence: zensight/<protocol>/@/devices/*/alive        │  │    │
│   │   └─────────────────────────────────────────────────────────────────┘  │    │
│   │                                                                         │    │
│   │   ┌───────────────────────┐  ┌───────────────────────┐                 │    │
│   │   │  Tick (1s interval)   │  │  Keyboard (Ctrl+F,    │                 │    │
│   │   │  • UI refresh         │  │  Escape, etc.)        │                 │    │
│   │   └───────────────────────┘  └───────────────────────┘                 │    │
│   │                                                                         │    │
│   └─────────────────────────────────────────────────────────────────────────┘    │
│                                                                                  │
│   ┌────────────────────────────────────────────────────────────────────────┐    │
│   │                              State Management                           │    │
│   │                                                                         │    │
│   │   ┌────────────────┐ ┌────────────────┐ ┌────────────────┐             │    │
│   │   │ DashboardState │ │DeviceDetail-   │ │ TopologyState  │             │    │
│   │   │                │ │State           │ │                │             │    │
│   │   │ • devices      │ │ • device_id    │ │ • nodes        │             │    │
│   │   │ • sensor_health│ │ • metrics      │ │ • edges        │             │    │
│   │   │ • connection   │ │ • history      │ │ • layout       │             │    │
│   │   └────────────────┘ └────────────────┘ └────────────────┘             │    │
│   │                                                                         │    │
│   │   ┌────────────────┐ ┌────────────────┐ ┌────────────────┐             │    │
│   │   │ AlertsState    │ │ SettingsState  │ │SyslogFilter-   │             │    │
│   │   │                │ │                │ │State           │             │    │
│   │   │ • rules        │ │ • zenoh config │ │ • severity     │             │    │
│   │   │ • triggered    │ │ • theme        │ │ • facilities   │             │    │
│   │   │ • acknowledged │ │ • groups       │ │ • patterns     │             │    │
│   │   └────────────────┘ └────────────────┘ └────────────────┘             │    │
│   │                                                                         │    │
│   └─────────────────────────────────────────────────────────────────────────┘    │
│                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

## Sensor Lifecycle

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                   Sensor Lifecycle (via SensorRunner)                            │
├──────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│   1. STARTUP                                                                     │
│   ──────────                                                                     │
│                                                                                  │
│   ┌────────────────┐    ┌────────────────┐    ┌────────────────┐                │
│   │  Parse CLI     │───▶│  Load Config   │───▶│  Init Logging  │                │
│   │  Arguments     │    │  (JSON5)       │    │  (tracing)     │                │
│   └────────────────┘    └────────────────┘    └────────────────┘                │
│           │                                                                      │
│           ▼                                                                      │
│   ┌────────────────┐    ┌────────────────┐    ┌────────────────┐                │
│   │  Connect to    │───▶│  Create        │───▶│  Declare       │                │
│   │  Zenoh         │    │  Publisher     │    │  Liveliness    │                │
│   └────────────────┘    └────────────────┘    └────────────────┘                │
│                                                                                  │
│   2. RUNNING                                                                     │
│   ──────────                                                                     │
│                                                                                  │
│   ┌─────────────────────────────────────────────────────────────────────────┐   │
│   │                                                                          │   │
│   │   ┌──────────────────┐     ┌──────────────────┐     ┌────────────────┐  │   │
│   │   │  Protocol Task   │     │  Health Task     │     │ Liveliness     │  │   │
│   │   │                  │     │                  │     │ Token          │  │   │
│   │   │  • Poll devices  │     │  • Periodic      │     │                │  │   │
│   │   │  • Receive data  │     │    snapshots     │     │  • Automatic   │  │   │
│   │   │  • Publish       │     │  • Update status │     │    keep-alive  │  │   │
│   │   │    telemetry     │     │  • Publish       │     │                │  │   │
│   │   │                  │     │    liveness      │     │                │  │   │
│   │   └────────┬─────────┘     └────────┬─────────┘     └────────────────┘  │   │
│   │            │                        │                                    │   │
│   │            ▼                        ▼                                    │   │
│   │   ┌──────────────────────────────────────────────────────────────────┐  │   │
│   │   │                      Zenoh Publisher                              │  │   │
│   │   │                                                                   │  │   │
│   │   │   zensight/<protocol>/<source>/<metric>  →  TelemetryPoint       │  │   │
│   │   │   zensight/<protocol>/@/health           →  HealthSnapshot       │  │   │
│   │   │   zensight/<protocol>/@/devices/*/...    →  DeviceLiveness       │  │   │
│   │   │                                                                   │  │   │
│   │   └──────────────────────────────────────────────────────────────────┘  │   │
│   │                                                                          │   │
│   └─────────────────────────────────────────────────────────────────────────┘   │
│                                                                                  │
│   3. SHUTDOWN                                                                    │
│   ────────────                                                                   │
│                                                                                  │
│   ┌────────────────┐    ┌────────────────┐    ┌────────────────┐                │
│   │  Receive       │───▶│  Cancel Tasks  │───▶│  Close Zenoh   │                │
│   │  SIGINT/SIGTERM│    │  Gracefully    │    │  Session       │                │
│   └────────────────┘    └────────────────┘    └────────────────┘                │
│                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

## Device Health Model

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                              Device Health Model                                 │
├──────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│   Status Determination                                                           │
│   ════════════════════                                                           │
│                                                                                  │
│   ┌─────────────────────────────────────────────────────────────────────────┐   │
│   │                                                                          │   │
│   │    Sensor Reports                 Frontend Combines                      │   │
│   │    ───────────────                ─────────────────                      │   │
│   │                                                                          │   │
│   │    DeviceLiveness {               Effective Status =                     │   │
│   │        status: Online,              max_severity(                        │   │
│   │        last_seen: ...,                sensor_reported_status,            │   │
│   │        latency_ms: 42,                local_staleness_status             │   │
│   │    }                                )                                    │   │
│   │              │                              │                            │   │
│   │              ▼                              ▼                            │   │
│   │    ┌────────────────────────────────────────────────────────────────┐   │   │
│   │    │                                                                 │   │   │
│   │    │   ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐      │   │   │
│   │    │   │  Online  │  │ Degraded │  │ Offline  │  │ Unknown  │      │   │   │
│   │    │   │  (Green) │  │ (Orange) │  │  (Red)   │  │  (Gray)  │      │   │   │
│   │    │   │          │  │          │  │          │  │          │      │   │   │
│   │    │   │ Device   │  │ Device   │  │ Device   │  │ No data  │      │   │   │
│   │    │   │ responds │  │ has      │  │ not      │  │ received │      │   │   │
│   │    │   │ normally │  │ issues   │  │ responding│  │ yet      │      │   │   │
│   │    │   └──────────┘  └──────────┘  └──────────┘  └──────────┘      │   │   │
│   │    │                                                                 │   │   │
│   │    └─────────────────────────────────────────────────────────────────┘   │   │
│   │                                                                          │   │
│   └─────────────────────────────────────────────────────────────────────────┘   │
│                                                                                  │
│   Staleness Detection                                                            │
│   ═══════════════════                                                            │
│                                                                                  │
│   Frontend tracks last_received timestamp per device.                            │
│   If no data for > staleness_threshold (default 30s):                           │
│     → Device marked as locally stale                                             │
│     → Combines with sensor status for final determination                        │
│                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

## Exporter Data Transformation

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                         Exporter Data Transformation                             │
├──────────────────────────────────────────────────────────────────────────────────┤
│                                                                                  │
│   Prometheus Exporter                                                            │
│   ═══════════════════                                                            │
│                                                                                  │
│   TelemetryPoint                           Prometheus Metric                     │
│   ──────────────                           ─────────────────                     │
│                                                                                  │
│   value: Counter(123)        ───▶          # TYPE metric_name counter            │
│                                            metric_name{labels...} 123            │
│                                                                                  │
│   value: Gauge(45.6)         ───▶          # TYPE metric_name gauge              │
│                                            metric_name{labels...} 45.6           │
│                                                                                  │
│   value: Text("running")     ───▶          # TYPE metric_name_info info          │
│                                            metric_name_info{value="running"} 1   │
│                                                                                  │
│   Metric naming: sanitize(protocol + "_" + metric)                               │
│   Valid chars: [a-zA-Z0-9_:]                                                     │
│                                                                                  │
│   OpenTelemetry Exporter                                                         │
│   ══════════════════════                                                         │
│                                                                                  │
│   TelemetryPoint                           OTEL Signal                           │
│   ──────────────                           ───────────                           │
│                                                                                  │
│   protocol: Syslog           ───▶          Log {                                 │
│   value: Text(message)                       severity: map_severity(level),      │
│                                              body: message,                       │
│                                              attributes: labels,                  │
│                                            }                                      │
│                                                                                  │
│   protocol: *                ───▶          Metric {                              │
│   value: Counter/Gauge                       type: Sum/Gauge,                     │
│                                              value: ...,                          │
│                                              attributes: labels,                  │
│                                            }                                      │
│                                                                                  │
│   Syslog Severity Mapping:                                                       │
│   0 (Emergency)  → FATAL                                                         │
│   1 (Alert)      → FATAL                                                         │
│   2 (Critical)   → FATAL                                                         │
│   3 (Error)      → ERROR                                                         │
│   4 (Warning)    → WARN                                                          │
│   5 (Notice)     → INFO                                                          │
│   6 (Info)       → INFO                                                          │
│   7 (Debug)      → DEBUG                                                         │
│                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

Both exporters subscribe to telemetry on `zensight/**` (which, by Zenoh's
verbatim-`@` rule, excludes the control plane). With `export_alerts` enabled (the
default) each exporter **also** declares a dedicated subscriber on
`zensight/*/@/alerts/*` and mirrors firing sensor alerts out: Prometheus renders a
`<prefix>_alert` gauge (`1` while firing, series absent once resolved —
Alertmanager-compatible), and the OTel exporter emits OTLP log records on the
`zensight.alerts` scope. Everything else under `@/…` and `zensight/_meta/…` is
skipped. The sysinfo host metrics are additionally mapped to OpenTelemetry
host-metrics semantic conventions via `zensight_common::semconv` (see
[Keyspace §6](KEYSPACE.md#6-exporter-semconv-mapping--zensight_commonsemconv-100)).

## Directory Structure

```
zensight/                            # Workspace root
├── Cargo.toml                       # Workspace manifest
├── CLAUDE.md                        # AI assistant guidance
├── README.md                        # Project overview
├── justfile                         # build / configure / run recipes
│
├── docs/                            # Documentation (this directory)
│   ├── README.md                    # Docs index
│   ├── ARCHITECTURE.md              # This file
│   ├── SENSORS.md                   # Per-sensor reference
│   ├── KEYSPACE.md                  # Canonical Zenoh keyspace reference
│   └── UI_TESTING.md                # Frontend testing guide
│
├── zensight/                        # Frontend application
│   ├── src/
│   │   ├── main.rs                  # Binary entry
│   │   ├── lib.rs                   # Library (for testing)
│   │   ├── app.rs                   # Iced Application
│   │   ├── message.rs               # Message enum
│   │   ├── subscription.rs          # Zenoh subscription sensor
│   │   ├── mock.rs                  # Mock data generators
│   │   └── view/                    # UI components
│   │       ├── dashboard.rs
│   │       ├── device.rs
│   │       ├── alerts.rs
│   │       ├── settings.rs
│   │       ├── topology/
│   │       └── icons/
│   └── tests/
│       └── ui_tests.rs
│
├── zensight-common/                 # Shared library
│   └── src/
│       ├── lib.rs
│       ├── telemetry.rs             # TelemetryPoint, Protocol
│       ├── health.rs                # DeviceStatus, HealthSnapshot
│       ├── config.rs                # Configuration loading
│       ├── session.rs               # Zenoh session helpers
│       ├── keyexpr.rs               # Key expression builders
│       └── serialization.rs         # JSON/CBOR encoding
│
├── zensight-sensor-core/       # Sensor abstraction
│   └── src/
│       ├── lib.rs
│       ├── runner.rs                # SensorRunner
│       ├── publisher.rs             # Zenoh publisher
│       └── liveliness.rs            # Presence management
│
├── zensight-sensor-snmp/               # SNMP sensor
├── zensight-sensor-logs/             # Syslog + journald (logs) sensor
├── zensight-sensor-sysinfo/            # System metrics sensor
├── zensight-sensor-netflow/            # NetFlow sensor
├── zensight-sensor-modbus/             # Modbus sensor
├── zensight-sensor-gnmi/               # gNMI sensor
├── zensight-sensor-netlink/            # Linux kernel networking sensor
├── zensight-sensor-netring/            # Wire-level flow/L7/NDR sensor
│
├── zensight-exporter-prometheus/    # Prometheus exporter
│   └── src/
│       ├── config.rs
│       ├── mapping.rs               # Type conversion
│       ├── collector.rs             # Metric storage
│       └── http.rs                  # /metrics endpoint
│
├── zensight-exporter-otel/          # OpenTelemetry exporter
│   └── src/
│       ├── config.rs
│       ├── metrics.rs
│       ├── logs.rs                  # Syslog → OTEL logs
│       └── exporter.rs
│
└── configs/                         # Example configurations
    ├── snmp.json5
    ├── syslog.json5                 # network syslog listeners
    ├── logs.json5                   # journald (used by `just run`)
    ├── netlink.json5
    ├── netring.json5
    ├── sysinfo.json5
    ├── prometheus.json5
    └── otel.json5
```

> For the full key tree see [KEYSPACE.md](KEYSPACE.md); for per-sensor details
> see [SENSORS.md](SENSORS.md).
