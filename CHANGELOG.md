# Changelog

All notable changes to ZenSight will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.1] - 2026-06-27

### Fixed

- **Packaging**: restore the RPM and Docker artifacts in the release workflow.
  The Fedora RPM build now installs `protobuf-devel` (the well-known-type
  includes gNMI's build needs), and both Docker build contexts
  (`Dockerfile.sensor` / `Dockerfile.exporter`) now copy the
  `zensight-sensor-netlink` / `zensight-sensor-netring` crates needed for
  workspace resolution. No shipped binary changed from 0.6.0; this only completes
  the artifact set (deb + flatpak were already published for 0.6.0).

## [0.6.0] - 2026-06-27

A large release: two new kernel/wire-level sensors, a unified logs sensor with
journald, a full host/incident-centric frontend redesign with NDR, alert export
to Prometheus/OTel, and OS packaging. See `docs/SENSORS.md`, `docs/KEYSPACE.md`,
and `docs/ARCHITECTURE.md` for the authoritative references.

### Added

#### New sensors

- **`zensight-sensor-netlink`** — Linux kernel networking telemetry over
  RTNETLINK + `sock_diag`, read **unprivileged**: interface/address/route/
  neighbor state, enriched `tcp_info` (delivery/pacing/retrans/reordering),
  qdisc/bufferbloat health score with AQM classification, conntrack and
  WireGuard (root-gated), nftables per-rule hit-rate, a default-route flap
  history, and a control-plane change timeline. Embeds a **sentinel** that
  asserts declared expectations (sockets/links/routes, rate-of-change, delivery
  floors) and raises alerts on deviation, hot-swappable at runtime.
- **`zensight-sensor-netring`** — wire-level flow / L7 / NDR telemetry via
  AF_PACKET/AF_XDP (needs `CAP_NET_RAW`) or offline pcap replay: flow RED,
  bandwidth, TCP resets, DNS/HTTP RED, TLS fingerprints, ICMP errors, a
  `(src,dst)` traffic matrix, and capture self-health with honest drop
  accounting + overload detection. Detectors: TRW port-scan, RITA beaconing,
  DNS-tunnel / Newly-Observed-Domain, connection-flood, Community ID v1, and
  MITRE ATT&CK technique tags. Opt-in: lateral-movement (SMB/RDP/Kerberos) and
  data-exfil heuristics, threat-intel (flow-risk/IOC/Sigma), passive asset
  inventory (ARP/NDP/LLDP/CDP), QUIC/SSH inventories, and JA4H fingerprints.

#### Logs sensor (formerly `syslog`)

- **journald ingestion** via libsystemd — scope/namespace, server-side matching,
  cursor-based gap-free resume, and known-event alerts (coredump / unit-failed /
  OOM by `MESSAGE_ID`); audit/SELinux records tagged `category=security`.
- **Per-line log events** (`events/<uid>`) with the OpenTelemetry logs data
  model in labels, replacing the last-writer-wins `<facility>/<severity>` key.
- Multiline stack-trace joining, a Drain3-style streaming **template miner** with
  novelty / rate-spike detection, derived per-unit log-rate and error rollups,
  per-unit **error budgets / SLOs with burn-rate alerts**, journald backpressure
  / rate-limit / drop accounting, and RFC 6587 framing on the network path.

#### Alerting & detection

- Common **alert model** (`Alert{Kind,Severity,State}`, stable `alert_key`) and
  an `AlertReporter` (debounce, reconcile) in `zensight-sensor-core`. Alerts flow
  on `@/alerts/<key>` as a firing → resolved → tombstone lifecycle, with a
  `@/query/alerts` firing-set queryable for late-joiner recovery.

#### Frontend

- **Redesign**: persistent app shell (left nav rail + top bar), host/
  incident-centric information architecture with facet tabs, a unified
  **Incident** object (grouped alerts + timeline + evidence pivots), and a
  composite host-health / worst-first fleet overview.
- New views: **Security** (NDR anomaly + ATT&CK by-tactic lens, detection
  tuning), **Expectations** (sentinel authoring), **Sensors** (health/failure
  tracking), top-level **Logs** (structured drill-down, MESSAGE_ID catalog,
  follow/pause, boot lens), **Inventory** + unified **fingerprint explorer**, and
  specialized netlink/netring device views with on-demand detail drill-downs.
- Productivity: **command palette** (Ctrl+P), **fuzzy** global metric search,
  **keyboard-shortcuts help overlay**, saved **alert-filter presets**, alert
  severity/source filter pills, per-device **metric favorites**, "alert on this
  metric" promotion, desktop notifications for CRITICAL alerts, native save
  dialog for export, and an absolute from/to chart time-range picker.
- Topology enrichment (netlink host nodes + neighbor-adjacency edges, alert
  overlay, router classification), a universal trend layer (booleans as 0/1 step
  series, log-rate series), and a **local store** (redb hot ring + tiered
  retention/eviction, template-aware log sampling) so history survives restart.

#### Exporters

- **Export sensor alerts** to Prometheus (a `<prefix>_alert` gauge, Alertmanager-
  compatible) and OTel (OTLP log records on the `zensight.alerts` scope).
- OpenTelemetry **host-metrics semantic-convention** mapping for sysinfo via a
  shared `zensight_common::semconv` table, so exported metrics are
  dashboard-portable.

#### Packaging & operations

- **systemd units** for every sensor and exporter (hardened: `DynamicUser`,
  `ProtectSystem=strict`, minimal ambient caps) plus **deb/rpm packaging parity**
  for all sensors and exporters, installing a unit and an example config.
- **SIGTERM** is handled for graceful shutdown (publish offline status, tombstone
  firing alerts) under systemd/Docker stop, not just Ctrl-C.

#### Project

- `justfile` to build / grant caps / configure / run the GUI with local sensors,
  pinning an explicit loopback rendezvous so discovery works without multicast.
- CI **clippy (`-D warnings`) + rustfmt gate** and a design-system color guard.

### Changed

- **BREAKING**: Renamed the "bridge" crate family to "sensor". `zenoh-bridge-*`
  crates/binaries are now `zensight-sensor-*`; `zensight-bridge-framework` is now
  `zensight-sensor-core`. Framework types renamed (`BridgeRunner`→`SensorRunner`,
  `BridgeConfig`→`SensorConfig`, `BridgeArgs`→`SensorArgs`, `BridgeHealth`→
  `SensorHealth`, `BridgeError`→`SensorError`, `BridgeInfo`→`SensorInfo`,
  `BridgeStatus`→`SensorStatus`).
- **BREAKING (wire)**: Renamed the `_meta/bridges/*` discovery key to
  `_meta/sensors/*`, and the `bridge`/`bridges` JSON fields in `HealthSnapshot`,
  `SensorInfo`, and `CorrelationEntry` to `sensor`/`sensors`. All sensors and the
  frontend cut over together; the `zensight/<protocol>/<source>/<metric>`
  telemetry prefix is unchanged.
- **Keyspace v2**: a formalized control-plane under `zensight/<protocol>/@/…`
  (`health`, `errors`, `status`, `alive`, `alerts`, `commands`, `query`) that
  telemetry wildcards deliberately don't match, plus on-demand `@/query/<topic>`
  detail channels (high-cardinality data served on request, never streamed). The
  `syslog` protocol is now `logs`. Documented in `docs/KEYSPACE.md`.
- Telemetry is published with zenoh-ext **AdvancedPublisher** (per-key cache +
  late-joiner recovery), paired with the GUI's AdvancedSubscriber.
- Frontend **design system**: type/spacing tokens, a theme-aware color layer, and
  a shared component kit; all ad-hoc colors centralized (CI-guarded).

### Fixed

- **Discovery**: the GUI and sensors form a session via an explicit loopback
  rendezvous instead of relying on multicast (broke under VPN/extra interfaces).
- Harden the SNMP authPriv path so a malformed v3 config returns an error instead
  of panicking; correct gNMI path-segment handling.
- Device-liveness regression and several dead/un-wired query channels in the GUI.

## [0.5.0] - 2026-02-21

### Fixed

- **Critical**: Remove unsafe `transmute` in AdvancedPublisher registry, replaced with safe `Arc` cloning
- **Critical**: Fix TOCTOU race condition in publisher cache with atomic check-and-insert
- **Critical**: Add missing `Sysinfo` protocol variant to `parse_key_expr()`
- **Data Integrity**: Fix `i64` to `f64` precision loss in `TelemetryValue::From<i64>` conversion
- **Data Integrity**: Tag `TelemetryValue` enum with `#[serde(tag)]` for unambiguous serialization
- **Exporters**: Fix silent metric rendering failures in Prometheus collector
- **Exporters**: Fix silent export failures in OTEL exporter
- **Exporters**: Fix gauge key collision with sorted attributes in OTEL exporter
- **Bridges**: Fix gNMI nanosecond timestamp conversion overflow
- **Bridges**: Fix Modbus address overflow with checked arithmetic
- **Bridges**: Fix incomplete regex escaping in syslog `glob_to_regex()`

### Changed

- `parse_key_expr()` now returns `Result` with descriptive errors instead of `Option`
- `KeyExprBuilder::build()` validates inputs (no empty strings, no invalid chars)
- Replace string-typed status fields with `HealthStatus` enum in bridge health
- `errors_last_hour` now uses a rolling window instead of monotonic counter
- Handle lock poisoning gracefully in `CorrelationRegistry`
- Improved error categorization for Zenoh errors (`BridgeError` variants)
- gNMI reconnection uses exponential backoff (5s to 5min) instead of fixed 5s
- Reduced NetFlow mutex contention by narrowing lock scope
- Dashboard uses cached filtered results for better performance
- Metric history uses `VecDeque` instead of `Vec` for efficient bounded storage
- Reduced string allocations in subscription key expression parsing

### Added

- **Toast Notifications**: Non-intrusive notification system for user feedback
- **Loading Indicator**: Visual feedback during Zenoh connection establishment
- **Stale Metric Indicators**: Visual cue for metrics that haven't updated recently
- **Decode Failure Metrics**: Both exporters now track deserialization error counts
- **OTEL Staleness Cleanup**: Automatic expiry of stale gauge entries in OTEL exporter
- **OTEL Instrument Caching**: Cache `Meter` and `Logger` instances to avoid recreation

## [0.4.0] - 2025-12-29

### Added

- **Device Metrics Table**: Replace metrics list with Iced 0.14 table widget for better data presentation
- **Page Transition Infrastructure**: Add animated page transitions between views
- **Dashboard Table View Toggle**: Switch between card and table views on dashboard
- **Syslog Table Widget**: Replace log stream with Iced 0.14 table widget
- **Responsive Grid Layout**: Dashboard device cards now use responsive grid
- **Double-Click Support**: Navigate to device details with double-click on cards
- **Animated Status Indicators**: Status dots use iced_anim for smooth animations

## [0.3.0] - 2025-12-29

### Added

- **Prometheus Exporter** (`zensight-exporter-prometheus`): Export ZenSight telemetry to Prometheus
  - HTTP `/metrics` endpoint for Prometheus scraping
  - Automatic metric type conversion (Counter, Gauge, Text to Prometheus types)
  - Metric name sanitization for Prometheus compatibility
  - Staleness-based expiry to prevent unbounded memory growth
  - Configurable filtering by protocol, source, and metric patterns

- **OpenTelemetry Exporter** (`zensight-exporter-otel`): Export ZenSight telemetry via OTLP
  - Support for both gRPC and HTTP OTLP protocols
  - Exports metrics and logs signals
  - Syslog messages converted to OTEL logs with severity mapping
  - Resource attributes for service identification

- **CI/CD**: Added deb, rpm, and Docker builds for exporters

### Changed

- Unified workspace versioning for all crates

## [0.2.0] - 2025-12-28

### Added

- **Network Topology View**: Interactive force-directed graph visualization
  - Canvas-based rendering with zoom and pan
  - Node search and click-to-select
  - Edge thickness based on bandwidth
  - Info panel with device details

- **UI Animations**: Smooth transitions using iced_anim
  - Animated buttons with hover effects
  - Animated SVG icons

- **Syslog Filtering**: Advanced message filtering capabilities
  - Static filters (severity, facility, patterns) in config
  - Dynamic runtime filters via Zenoh commands
  - Frontend filter panel

- **Advanced Zenoh Features**
  - Liveliness tokens for bridge/device presence detection
  - AdvancedPublisher/Subscriber from zenoh-ext

- **Cross-Bridge Infrastructure**
  - Bridge health monitoring (`BridgeHealth`)
  - Device liveness tracking (`DeviceLiveness`, `DeviceStatus`)
  - Unified error reporting (`ErrorReport`, `ErrorType`)
  - Cross-bridge correlation registry

- **Enhanced Sysinfo Bridge**
  - CPU breakdown (user/system/iowait/steal/nice/idle/irq/softirq)
  - Disk I/O stats (read/write bytes, IOPS)
  - Temperature sensors (Linux hwmon)
  - TCP connection state counts

- **Demo Mode Enhancements**
  - Realistic telemetry simulation
  - Health and liveness simulation
  - Periodic anomaly injection

- **Persistence**: Save/restore alert rules, theme, and current view

- **Chart Improvements**
  - Multi-metric comparison mode
  - Threshold/baseline lines
  - Larger time windows (6h, 24h, 7d)
  - Zoom with keyboard and Ctrl+scroll
  - Pan controls for time navigation

- **Alerts**: Test Rule button for previewing matches

- **UI Polish**
  - Tooltips for truncated values
  - Alert count badge on dashboard
  - Light/dark theme toggle
  - Keyboard shortcuts (Ctrl+F search, Esc back/close)
  - Search debouncing (300ms)

### Fixed

- Node click detection in topology view
- Theme-aware colors (replaced hardcoded values)
- Layout convergence stability
- Clippy warnings across workspace

## [0.1.0] - 2025-12-15

### Added

- **Core Platform**
  - `zensight`: Iced 0.14 desktop frontend
  - `zensight-common`: Shared telemetry model and Zenoh helpers
  - `zensight-sensor-core`: Common bridge infrastructure

- **Protocol Bridges**
  - `zensight-sensor-snmp`: SNMP v1/v2c/v3 with full USM support, MIB loading
  - `zensight-sensor-syslog`: RFC 3164/5424, UDP/TCP/Unix socket
  - `zensight-sensor-netflow`: NetFlow v5/v7/v9 and IPFIX
  - `zensight-sensor-modbus`: Modbus TCP/RTU
  - `zensight-sensor-sysinfo`: System metrics (CPU, memory, disk, network)
  - `zensight-sensor-gnmi`: gNMI streaming telemetry with TLS

- **Frontend Features**
  - Dashboard with device overview
  - Device detail view with metrics
  - Time-series charts
  - Alerts and notifications
  - Settings page
  - Data export (CSV/JSON)
  - SVG icons

- **Testing**
  - Simulator-based UI tests
  - Mock telemetry generators
