# Changelog

All notable changes to ZenSight will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Bandwidth-by-service from systemd IPAccounting (#315, epic #320)**: the systemd
  sensor derives per-unit network rate `unit/<name>/{ip_ingress_bps,ip_egress_bps}`
  from successive `IPIngressBytes`/`IPEgressBytes` deltas (the cheapest bandwidth-by-*
  tier). Metrics are labelled `bw.source=systemd`/`bw.semantics=wire-l3` (cgroup_skb:
  L3+ bytes, no L2) so they're never blended with app-goodput or wire-L2 sources; a
  unit restart re-baselines the counter; an active unit with IPAccounting off emits an
  explicit `ip_accounting=false` state rather than a silent zero. New shared vocabulary
  in `zensight-common::bandwidth` (`BandwidthSource`/`ByteSemantics`/`ProtoScope`,
  `BandwidthRecord`, and the `bw.*` label keys) underpins all bandwidth tiers.

- **Host-identity envelope (#301)**: every sensor now publishes a registration
  record on `zensight/_meta/sensors/<name>/<source>` and a self-report
  `HostEvidence` claim on `zensight/_meta/evidence/host/<sensor>/<source>`
  (re-emitted every 60 s via cached publishers). The identity carries a
  **hashed** machine-id (`host_id` = sha256(machine-id + app salt); the raw id
  never leaves the host), boot id, hostname/fqdn, and non-loopback IPs/MACs.
  Health snapshots gain `host_id`; alerts gain a `host.id` annotation label.
- **sysinfo process enrichment + argv scrubber (#302)**: the on-demand
  `@/query/processes` `ProcessRecord` gains `cmdline`, `exe`, `ppid`, `cgroup`
  (v2 path — joins a process to its systemd unit), `start_time` (the
  `(pid, start_time)` identity pair), and `user`. Command lines are **scrubbed of
  secret-looking argv values** (Datadog-style key list; both `key=value` and
  `--key value` shapes) and byte-capped before publish — controlled by
  `processes.scrub_args` (default `true`), `custom_sensitive_words`, and
  `strip_proc_arguments`.
- **systemd/logs unit↔process↔log identity (#303)**: `UnitDetail` (`@/query/unit`)
  gains `main_pid` + `main_pid_start_time`, `invocation_id`, and `control_group`;
  the logs sensor captures `_SYSTEMD_INVOCATION_ID` as `sd.journald.invocation_id`.
  Together these join a systemd unit to its main process, its cgroup, and its exact
  log lines.
- **netlink socket→process attribution (#304)**: `@/query/sockets` `SocketRecord`
  gains `cookie`, `cgroup_id`/`cgroup`, and the owning `pid`/`process`/
  `proc_start_time`, resolved **unprivileged** via a per-request `/proc` fd-scan
  (`collect.socket_processes`, default on; ceiling `socket_process_max_procs`,
  default 4096). An optional eBPF tier attributes recently-closed / live-established
  sockets the fd-scan can't reach.
- **Passive DNS name resolution (#308)**: the netring sensor parses DNS answers
  (flowscope `NameMap` — CNAME-chain-following, glue-poisoning-safe, PTR-aware)
  into a client-scoped IP↔name cache. Flow and talker records gain
  provenance-ranked `dst_names`/`names`, and an FQDN-pivoted RITA beacon detector
  flags periodic beaconing keyed by destination name (ATT&CK T1071).
- **Identity evidence feeds (#307)**: netring publishes observed-asset evidence
  (ARP/LLDP/DHCP inventory → `HostEvidence` with `observer=netring`) and
  passive-DNS `NameObservation`s; netlink publishes observed-neighbor evidence
  (ARP/ND table → `HostEvidence`). All third-party claims are rate-limited
  (per-source min-interval + per-tick cap) and age out by TTL.
- **`zensight-correlator` — identity correlation service (#305)**: a new
  single-writer daemon that subscribes to the evidence keyspace and merges
  claims into `HostEntity` docs on `zensight/_meta/entity/host/<id>` via a
  deterministic union-find over ranked identity rules (host_id > MAC+IP > FQDN >
  hostname; IP/MAC-alone never join; a host_id-conflict guard blocks weak
  false-merges). Entities carry membership provenance (which rule + confidence
  bound each source), are re-emitted for liveness, tombstoned on retire, and
  seeded to late joiners via the `_meta/query/entities` queryable; arbitrary-IP
  names resolve on demand via `_meta/query/names?ip=`. A `--demo` mode feeds
  synthetic evidence through the real pipeline. A Zenoh liveliness token
  guarantees a single writer.
- **Host-entity frontend (#306)**: the GUI consumes the correlator's `HostEntity`
  docs (subscribe `_meta/entity/**` + connect-time seed) into an `EntityStore`.
  The dashboard groups a host's per-protocol devices under **one host card**
  (worst-of-members status, facet chips, alert rollup, a persisted "group by host"
  toggle); the host detail page gains an identity header and a "merged from N
  sources" resolution drill-down (each member's binding rule + confidence); the
  topology keys nodes by entity, bridges wire flows via identifying IPs, and shows
  wire-only hosts as passive nodes. With no correlator the store is empty and every
  view falls back to the per-source rendering (degraded path, pinned by test).
- **On-demand pcap capture (#333)**: the netring sensor serves a `Capture` artifact
  over the `@/artifact` channel — a dedicated build-time packet tap (idle cost: one
  `ArcSwap` load per matching frame) is narrowed per request via the monitor's
  reload handle, streamed through a bounded drop-on-overflow channel into a pcap
  writer, and zstd-compressed to a `capture-<source>-<ts>.pcap.zst` blob. Requests
  are clamped to configured `capture.on_demand` limits (duration, bytes, snaplen,
  cooldown, optional filter allowlist); off by default. The GUI's artifact card
  gains a capture request form (duration/filter/size/compress) with progress and
  download; the netring device screen's tab is renamed **Capture health** and points
  to the Sensors page for launching captures.

### Changed

- **BREAKING (identity, #301): host-id config unified to `source`.** The
  netlink `netlink.hostname`, netring `netring.sensor_id`, and sysinfo
  `sysinfo.hostname` config fields are renamed to `source` (same `"auto"` →
  local-hostname default). The remote-device sensors (snmp, gnmi, modbus,
  netflow, logs) gain an optional `source` override for the *agent host* id
  used in debug bundles and artifact routing. Update your JSON5 configs.
- **BREAKING (identity, #301): `SensorInfo` redesigned and re-keyed.** The
  (previously never-published) `zensight/_meta/sensors/<name>` record moves to
  `zensight/_meta/sensors/<name>/<source>` — the per-name key collides across
  hosts — and now carries identity fields instead of duplicated health data.
  The dead `zensight/_meta/correlation/<ip>` keyspace and `CorrelationEntry`
  wire type are deleted (replaced by `_meta/evidence/**` + the upcoming
  entity keyspace).
- **Alert keys ignore `host.`-prefixed labels** (the annotation namespace):
  identity metadata stamped onto alerts never changes alert identity, so
  firing/resolve pairs stay matched across identity refreshes. Alert keys for
  alerts without such labels are unchanged.

- **BREAKING (large-data transfer): unified the `@/report` and `@/snapshot`
  control-plane channels into one `@/artifact` channel.** Operator-facing
  migration note — anything that PUT report/snapshot requests or GET the bytes
  must move to the new keyspace and wire types:
  - **Keyspace**: `@/report/{request,status,blob/<id>/**,cancel}` and
    `@/snapshot/{request,status,cancel}` collapse to
    `@/artifact/{request,status,cancel}` + `@/artifact/blob/<id>/**`. The Tier-2
    `@/store/<algo>/<hash>` and `@/tree/<id>` queryables are unchanged but are now
    kind-agnostic delivery infra shared by any artifact whose producer emits a
    `Tree` delivery.
  - **Wire types** (`zensight-common`): `Report*`/`Snapshot*` → `Artifact*` —
    `ReportRequest`/`SnapshotRequest` → `ArtifactRequest` (+ tagged `ArtifactKind`:
    `Report`, `Snapshot { dir }`, `Capture` — the last shipped by netring in
    #333); `ReportState` → tagged
    `ArtifactState`; the new tagged `Delivery` (`Blob` | `Tree`) tells the client
    which tier to pull; `ReportStatus`/`SnapshotStatus` → `ArtifactStatus { kinds:
    Vec<KindStatus> }` (one entry per kind). The old `report_*`/`snapshot_*` key
    builders are replaced by `artifact_{request,status,cancel}_key` +
    `artifact_{blob,store,tree}_prefix`.
  - **Config**: the top-level `report:` / `snapshot:` sections (`ReportLimits` /
    `SnapshotLimits`) move under a single `artifacts: { report, snapshot }` section
    (`ArtifactLimits`); every kind stays **disabled by default**. `SnapshotDir {
    name, path }` allowlist entries now live under `artifacts.snapshot.dirs`.
  - **Sensor-core API**: `SensorRunner::with_report`/`with_snapshot` →
    `with_artifacts(source_id, vec![ReportProducer, SnapshotProducer, …])`;
    `SensorConfig::report_limits`/`snapshot_limits` → `artifact_limits()`. One
    `ArtifactChannel` owns request/status/cancel + reaper (per-kind busy +
    cooldown, lazy `BlobServer`/`TreeServer`); producers implement the
    `ArtifactProducer` trait. See `docs/KEYSPACE.md` §3.1a and
    `docs/LARGE-DATA-TRANSFER.md`.
  - **Frontend**: the `blob_fetch.rs` + `dir_fetch.rs` views merge into one
    `zensight/src/view/artifact_fetch.rs` whose `download_stream` matches on
    `Delivery`.
- **Dependencies**: bumped `nlink` 0.23 → 0.24, `netring` 0.28 → 0.29, and
  `flowscope` 0.20 → 0.22 (netlink and netring sensors). Migrated the breaking
  surface: `MonitorBuilder::flow_risk()` → `flow_analysis()`, and our local
  `DetectorScore` impls (`RitaBeaconHit`, `FloodScore`) to the typed
  `DetectorKind` (`DetectorKind::Other(...)`). Published anomaly kind slugs are
  byte-identical, so alert keys, the detection-tuning panel, and the Security
  view are unaffected.

### Fixed

These are upstream bug fixes inherited with the bump; they change values on
metrics ZenSight already publishes, so dashboards and alerts on these series
will see a step:

- **netlink `sockets/tcp/bytes_retrans_total` and `.../reordered_total` were
  always zero.** `nlink` < 0.24 stopped parsing `TcpInfo` at byte 168, so
  `bytes_retrans` and `reord_seen` silently read 0 on every kernel. They now
  carry real values.
- **netlink socket memory metrics never appeared.** An off-by-one in
  `nlink`'s `InetExtension::mask()` meant `with_mem_info()` actually requested a
  different extension, so `InetSocket.mem_info` was always `None` and the skmem
  branch was dead. Socket-memory metrics now flow for the first time.
- **netring IPv6 ICMP error counters were wrong.** `flowscope` misdetected
  ICMPv6 error types (Destination Unreachable, Time Exceeded) as ICMPv4, so the
  `on_icmp_error` counters (unreachable / time-exceeded / MTU) undercounted or
  mislabeled IPv6 errors. Counts are now correct.
- **netring AF_XDP could hang under sparse traffic.** `flowscope`'s async
  AF_XDP poller could miss a wakeup when packets arrived during an idle gap;
  fixed upstream.
- **netlink XFRM/IPsec polling no longer spams the kernel log.** `nlink`'s XFRM
  dumps appended a stray struct that made the kernel log a ratelimited
  `netlink: … bytes leftover` warning on every poll. Results were always
  correct; the log noise is gone.

## [0.6.2] - 2026-06-27

### Fixed

- **Packaging**: build the legacy sensor Docker images (syslog/sysinfo/snmp).
  `Dockerfile.sensor` gained `libsystemd` (build + runtime) for the logs sensor's
  journald support and `libssl3` at runtime for snmp; the Docker matrices no
  longer `fail-fast`. Completes the container-image set (deb/rpm/flatpak and the
  exporter images were already published for 0.6.1). No shipped binary changed.

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
