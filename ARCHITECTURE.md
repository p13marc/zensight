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
│  │                              Protocol Bridges                                │   │
│  │                                                                              │   │
│  │   ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           │   │
│  │   │ zenoh-      │ │ zenoh-      │ │ zenoh-      │ │ zenoh-      │           │   │
│  │   │ bridge-snmp │ │ bridge-     │ │ bridge-     │ │ bridge-     │  ...      │   │
│  │   │             │ │ syslog      │ │ sysinfo     │ │ netflow     │           │   │
│  │   └──────┬──────┘ └──────┬──────┘ └──────┬──────┘ └──────┬──────┘           │   │
│  │          │               │               │               │                  │   │
│  │          │  Uses zensight-bridge-framework (BridgeRunner, Publisher)        │   │
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
│  │   ├── zensight/<protocol>/@/health              (bridge health)             │   │
│  │   ├── zensight/<protocol>/@/devices/*/liveness  (device liveness)           │   │
│  │   ├── zensight/<protocol>/@/errors              (error reports)             │   │
│  │   ├── zensight/_meta/bridges/*                  (bridge registration)       │   │
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
│   │   │      zensight-common       │  │    zensight-bridge-framework       │    │  │
│   │   │                            │  │                                    │    │  │
│   │   │  • TelemetryPoint          │  │  • BridgeRunner                    │    │  │
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
│   │  │   zensight    │  │ zenoh-bridge- │  │ zensight-     │  │ zensight-     │  │ │
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
│     External Device          Bridge                      Zenoh                   │
│     ───────────────          ──────                      ─────                   │
│                                                                                  │
│     ┌───────────┐     poll   ┌───────────────┐  publish  ┌──────────────────┐   │
│     │   SNMP    │──────────▶│zenoh-bridge-  │──────────▶│ zensight/snmp/   │   │
│     │   Agent   │    GET     │snmp           │           │ router01/        │   │
│     └───────────┘            └───────────────┘           │ system/sysUpTime │   │
│                                                          └──────────────────┘   │
│                                                                                  │
│     ┌───────────┐   UDP/TCP  ┌───────────────┐  publish  ┌──────────────────┐   │
│     │  Syslog   │──────────▶│zenoh-bridge-  │──────────▶│ zensight/syslog/ │   │
│     │  Source   │   514      │syslog         │           │ server01/...     │   │
│     └───────────┘            └───────────────┘           └──────────────────┘   │
│                                                                                  │
│  2. COMMON DATA MODEL                                                            │
│  ════════════════════                                                            │
│                                                                                  │
│     All bridges normalize data into TelemetryPoint:                              │
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

```
zensight/
├── <protocol>/                          # snmp, syslog, sysinfo, netflow, modbus, gnmi
│   ├── <source>/                        # Device/host identifier
│   │   └── <metric_path>                # Hierarchical metric name
│   │       Example: zensight/snmp/router01/interfaces/eth0/ifInOctets
│   │
│   └── @/                               # Metadata namespace
│       ├── health                       # Bridge HealthSnapshot (periodic)
│       ├── errors                       # ErrorReport publications
│       ├── alive                        # Bridge liveliness token
│       ├── commands/                    # Runtime commands (e.g., syslog filters)
│       │   └── filter
│       └── devices/
│           └── <device_id>/
│               ├── liveness             # DeviceLiveness status
│               └── alive                # Device liveliness token
│
└── _meta/
    ├── bridges/
    │   └── <bridge_name>                # Bridge registration info
    └── correlation/
        └── <ip_address>                 # Cross-bridge device correlation
```

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
│   │   │  • Bridge presence: zensight/<protocol>/@/alive                  │  │    │
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
│   │   │ • bridge_health│ │ • metrics      │ │ • edges        │             │    │
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

## Bridge Lifecycle

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                   Bridge Lifecycle (via BridgeRunner)                            │
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
│   │    Bridge Reports                 Frontend Combines                      │   │
│   │    ───────────────                ─────────────────                      │   │
│   │                                                                          │   │
│   │    DeviceLiveness {               Effective Status =                     │   │
│   │        status: Online,              max_severity(                        │   │
│   │        last_seen: ...,                bridge_reported_status,            │   │
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
│     → Combines with bridge status for final determination                        │
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

## Directory Structure

```
zensight/                            # Workspace root
├── Cargo.toml                       # Workspace manifest
├── ARCHITECTURE.md                  # This file
├── CLAUDE.md                        # AI assistant guidance
├── README.md                        # Project overview
│
├── zensight/                        # Frontend application
│   ├── src/
│   │   ├── main.rs                  # Binary entry
│   │   ├── lib.rs                   # Library (for testing)
│   │   ├── app.rs                   # Iced Application
│   │   ├── message.rs               # Message enum
│   │   ├── subscription.rs          # Zenoh subscription bridge
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
├── zensight-bridge-framework/       # Bridge abstraction
│   └── src/
│       ├── lib.rs
│       ├── runner.rs                # BridgeRunner
│       ├── publisher.rs             # Zenoh publisher
│       └── liveliness.rs            # Presence management
│
├── zenoh-bridge-snmp/               # SNMP bridge
├── zenoh-bridge-syslog/             # Syslog bridge
├── zenoh-bridge-sysinfo/            # System metrics bridge
├── zenoh-bridge-netflow/            # NetFlow bridge
├── zenoh-bridge-modbus/             # Modbus bridge
├── zenoh-bridge-gnmi/               # gNMI bridge
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
    ├── syslog.json5
    ├── prometheus.json5
    └── otel.json5
```
