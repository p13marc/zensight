# ZenSight Sensors Reference

ZenSight sensors translate a legacy or host-level monitoring source into the
unified [`TelemetryPoint`] model and publish it to Zenoh. Every sensor inherits
the same control-plane (`@/health`, `@/errors`, `@/alive`, `@/status`, and the
opt-in `@/report`) and key conventions from `zensight-sensor-core` — see the
[Keyspace Reference](KEYSPACE.md) for the full key tree and
[Architecture](ARCHITECTURE.md) for the runtime model.

This document is the per-sensor reference: what each sensor ingests, how to
configure it, and the exact Zenoh keys it publishes/serves.

**Conventions shared by all sensors**

- Telemetry: `zensight/<protocol>/<source>/<metric>` (payload: `TelemetryPoint`).
- Control-plane: `zensight/<protocol>/@/{health,errors,status,alive}` (+
  `devices/<device>/{liveness,alive}` where per-device tracking applies).
- Debug reports: every sensor can serve an on-demand redacted `tar.zst` bundle
  (config + health + counters) over `@/report/*` — **opt-in** per sensor via
  `report.enabled` in its config (disabled by default). See KEYSPACE.md §3.1a.
- Config: a JSON5 file under [`configs/`](../configs/); pass with `--config`.
  Every config has a `zenoh` block (`mode`, `connect`, `listen`) and a
  `logging` block. The `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` env vars override
  the `zenoh` block (used by `just run` to pin a loopback rendezvous).
- Telemetry is published with zenoh-ext **advanced publishers** (so it pairs
  with the GUI's advanced subscriber); control-plane (`@/…`) uses plain puts.
  See [Architecture → Zenoh Transport & Pub/Sub Model](ARCHITECTURE.md#zenoh-transport--pubsub-model).

| Sensor | Protocol | Source of truth | Default config | Privileges |
|--------|----------|-----------------|----------------|------------|
| [snmp](#snmp) | `snmp` | SNMP v1/v2c/v3 polling + traps | `configs/snmp.json5` | none |
| [syslog / logs](#syslog--logs) | `logs` | RFC 3164/5424 + systemd journald | `configs/syslog.json5`, `configs/logs.json5` | journal-read for system journal |
| [netflow](#netflow) | `netflow` | NetFlow v5/v9 / IPFIX | `configs/netflow.json5` | none |
| [modbus](#modbus) | `modbus` | Modbus TCP/RTU | `configs/modbus.json5` | none |
| [sysinfo](#sysinfo) | `sysinfo` | local host metrics | `configs/sysinfo.json5` | none |
| [gnmi](#gnmi) | `gnmi` | gNMI streaming telemetry | `configs/gnmi.json5` | none |
| [netlink](#netlink) | `netlink` | Linux kernel networking (RTNETLINK/sock_diag) | `configs/netlink.json5` | none (unprivileged reads) |
| [netring](#netring) | `netring` | wire-level capture (AF_PACKET/AF_XDP) or pcap | `configs/netring.json5` | `CAP_NET_RAW` for live capture |

---

## snmp

Polls SNMP agents and receives traps. v3 auth/priv supported.

- **Telemetry:** `zensight/snmp/<device>/<metric>` where `<metric>` is the
  MIB-resolved name (e.g. `system/sysUpTime`, `interfaces/ifInOctets`).
- **Traps:** `zensight/snmp/<source_ip>/trap/<trap_id>` and per-varbind
  `…/trap/<trap_id>/<varbind>`.
- **Alerts:** `@/alerts/<alert_key>` (when alerting is enabled).
- **Build note:** needs `openssl`/`net-snmp` headers at build time.

## syslog / logs

Receives syslog over the network **and/or** ingests the local systemd journal.
Both feed the same model and keyspace.

- **Telemetry:** `zensight/logs/<hostname>/events/<uid>` — one **per-line event**
  per log line (#104). `<uid>` is `<timestamp_ms><seq>` (zero-padded, time-
  sortable) so every line survives instead of being overwritten last-writer-wins.
  Payload value = the log message; facility/severity and the OpenTelemetry logs
  data model (`severity_number` 1–24, `severity_text`, `log.record.uid`, and
  `log.record.original` when raw is kept) land in **labels**, alongside structured
  fields — journald fields under `sd.journald.*`, `source_type` =
  `udp`/`tcp`/`unix`/`journald`.
- **Derived rollups** (`derived`, default on): cheap aggregates emitted every
  `derived_interval_secs` under `zensight/logs/<host>/logs/*` — per-severity
  counters (`logs/by_severity/<level>_total`), error/warning totals
  (`logs/errors_total`, `logs/warnings_total`), top-N per-unit message/error
  counters (`logs/by_unit/<unit>/...`, capped to `top_units` + an `other`
  bucket), a `logs/units_in_failure` gauge, and journald throughput
  (`logs/journald/{read,published,dropped,sampled_out}_total`).
- **Multiline joining** (`multiline`, default on, #107): on the TCP/Unix stream
  paths, continuation lines (indented stack frames, `Caused by:`, `...`,
  `Traceback …`) are folded back into the preceding record so a Java/Python/Go
  traceback stays one event (one uid) instead of one record per line. Bounded by
  `max_lines`/`max_bytes`; the last line of a burst is emitted after
  `flush_timeout_ms` (default 200ms). journald is unaffected (one record/entry).
- **Sources:**
  - Network: UDP/TCP/Unix listeners (RFC 3164 + RFC 5424).
  - **journald** (`journald.enabled`): reads the local journal via libsystemd
    (no `journalctl` subprocess). Supports scope (system/user), server-side
    matching (`units`, `min_priority`, `transports`), cursor-based no-loss
    resume (`start_from`), and **known-event alerts** — coredump / unit-failed /
    OOM are matched by `MESSAGE_ID` and raised on `@/alerts`. Coredump entries
    capture `COREDUMP_*` (exe/signal/pid) onto the record + alert; audit /
    SELinux records (`_AUDIT_TYPE_NAME`, `_SELINUX_CONTEXT`) are tagged
    `category=security` for the Security view (#107).
- **Control:** `@/commands/filter` + `@/status/filter` — add/remove/clear
  dynamic message filters at runtime.
- **Configs:** `configs/syslog.json5` (network listeners; journald block
  commented), `configs/logs.json5` (journald-only, used by `just run`).
- See [the journald feature notes](#journald-notes) below.

## netflow

Collects NetFlow v5/v9 and IPFIX flow records from exporters.

- **Telemetry:** `zensight/netflow/<exporter>/<metric>` (flow aggregates /
  per-conversation metrics, per the config).

## modbus

Polls Modbus TCP/RTU registers (coils, discrete inputs, holding/input registers).

- **Telemetry:** `zensight/modbus/<device>/<register>` (e.g.
  `holding/temperature`), with scaling/typing from the register map in config.

## sysinfo

Local host metrics (CPU, memory, disk, network, load) plus a Linux saturation /
error surface (PSI, vmstat, cgroup-v2, thermal/power). All families are gated by
`collect.*` flags; the families marked **default off** below are opt-in.

- **Telemetry:** `zensight/sysinfo/<hostname>/<metric>`. The metric families:

  | Family | `collect` flag | Example keys |
  |--------|----------------|--------------|
  | system | `system` | `system/uptime`, `system/load` (label `period`), `system/boot_time` |
  | cpu | `cpu` | `cpu/usage`, `cpu/<n>/usage`, `cpu/<n>/frequency` |
  | cpu times (Linux) | `cpu_times` | `cpu/times/{user,nice,system,idle,iowait,irq,softirq,steal}`, `cpu<n>/times/*` |
  | memory | `memory` | `memory/{total,used,available,usage_percent,swap_total,swap_used,swap_percent}` |
  | memory composition (Linux) | `memory` | `memory/{cached,buffers,slab,dirty,writeback}` |
  | disk | `disk` | `disk/<mount>/{total,used,available,usage_percent}` |
  | disk I/O (Linux) | `disk_io` | `disk/<dev>/io/{read_bytes,write_bytes,read_ops,write_ops,time_ms,read_rate,write_rate,read_iops,write_iops}`, plus saturation `disk/<dev>/io/{util_percent,queue_depth}` |
  | network | `network` | `network/<iface>/{rx_bytes,tx_bytes,rx_packets,tx_packets,rx_errors,tx_errors,rx_rate,tx_rate}` |
  | network extended (Linux) | `net_dev_extended` | `network/<iface>/{rx_dropped,rx_fifo,rx_frame,multicast,tx_dropped,tx_fifo,tx_colls,tx_carrier}` |
  | pressure / PSI (Linux) | `pressure` | `pressure/<cpu\|memory\|io>/<some\|full>_{avg10,avg60,avg300,total_us}` |
  | vmstat (Linux) | `vmstat` | `memory/{oom_kills_total,page_faults_major_total,page_faults_total,paging_in_total,paging_out_total,pgpgin_total,pgpgout_total}` |
  | kernel derivatives (Linux) | `vmstat` | `system/{context_switches_total,forks_total,procs_running,procs_blocked}` |
  | fd / inode ceilings (Linux) | `fd_inode` | `system/file_descriptors_{used,max,used_percent}`, `disk/<mount>/{inodes_total,inodes_used,inodes_free,inode_used_percent}` |
  | processes | `processes` **(default off)** | `system/{processes_total,processes_zombie}`, `process/<rank>/{cpu,memory}` |
  | temperatures (Linux) | `temperatures` **(default off)** | `sensors/<chip>/<label>/{temp,critical,max}` |
  | tcp states (Linux) | `tcp_states` **(default off)** | `tcp/<state>`, `tcp/total` |
  | cgroup-v2 (Linux) | `cgroups` **(default off)** | `cgroup/cpu/{nr_throttled,throttled_usec}`, `cgroup/memory/{current,max,used_percent,oom_kills_total,oom_total}`, `cgroup/<res>/pressure/<scope>_{avg10,total_us}` |
  | thermal / power (Linux) | `power` **(default off)** | `power/rapl/<zone>/watts`, `sensors/<chip>/<fan>/rpm`, `battery/<name>/{capacity,status}`, `system/entropy_avail` |

  Linux-only families degrade gracefully (an absent `/proc`/`/sys` file is
  skipped, never emitted as a zero). Per-mount/per-interface/per-device keys are
  sanitized for the key expression (e.g. `/` → `_`, the root mount → `root`) and
  carry the original name back in a label.
- **On-demand detail** (`@/query/<topic>`): `processes?sort=cpu|mem|io&top=N`
  (`collect.process_query`, default on) — the per-pid firehose, served on
  request rather than streamed.
- **eBPF saturation histograms** (`collect.ebpf`, **default off**, opt-in build
  — issue #99): scheduler run-queue latency (`runqlat`) and block-I/O latency
  (`biolatency`) as log2 histograms with derived p50/p95/p99 + max, served only
  on `@/query/latency` (never streamed). These are the saturation *tails* that
  `/proc` 5s averages cannot see. The reply is a `LatencyReport` JSON:
  `{ available, window_secs, runqlat: {unit, buckets:[{le_us,count}], total,
  p50_us, p95_us, p99_us, max_us}, biolatency: {...} }`.
  - **Build:** needs a binary built with `--features ebpf`, which requires a
    nightly toolchain + `rust-src` + `bpf-linker` (`rustup toolchain install
    nightly && rustup component add rust-src --toolchain nightly && cargo install
    bpf-linker`), then `cargo build -p zensight-sensor-sysinfo --release
    --features ebpf`. The feature is intentionally **out of** the default
    `cargo build --workspace` / stable CI (the eBPF program crate is a member
    that compiles to an empty host stub off the `bpf` target).
  - **Runtime:** needs `CAP_BPF` + `CAP_PERFMON` (kernel ≥ 5.8). Off / missing
    caps / unsupported kernel → one warning, `available:false`, and the
    unprivileged baseline is unchanged. See the commented `AmbientCapabilities`
    block in `packaging/systemd/zensight-sensor-sysinfo.service`.

## gnmi

Subscribes to gNMI streaming telemetry from network devices.

- **Telemetry:** `zensight/gnmi/<device>/<path>` where `<path>` mirrors the gNMI
  path (e.g. `interfaces/interface[name=eth0]/state/counters/in-octets`).
- **Build note:** needs `protoc` at build time.

## netlink

Linux kernel networking telemetry via RTNETLINK + `sock_diag`, read
**unprivileged**. Includes an embedded **sentinel** that asserts declared
expectations and alerts on deviation.

- **Telemetry:** `zensight/netlink/<host>/<metric>` — interfaces, addresses,
  routes, neighbors, sockets, plus ethtool/TC/xfrm depth metrics (gated by the
  `collect` config).
- **On-demand detail** (`@/query/<topic>`): `routes`, `neighbors`,
  `sockets?state=&port=`, `addresses`, `events`, `route_changes`, `tc`, `xfrm`, `nft`.
- **eBPF module** (`collect.ebpf`, **default off**, opt-in build — issue #114):
  what `sock_diag` snapshots cannot see — connection *lifecycle* and *attribution*.
  Streams connect-latency gauges `sockets/tcp/connlat_us_{p50,p95}` (through the
  normal publish path, so sentinel `metric-threshold` expectations can watch them)
  and serves two queryables: `@/query/retransmits` (top-K per-peer retransmit
  counts) and `@/query/connections` (recent tcplife records: pid/comm/peer/
  duration). **Build:** `--features ebpf` (nightly + `rust-src` + `bpf-linker`),
  then `cargo build -p zensight-sensor-netlink --release --features ebpf`. The
  feature is out of the default `cargo build --workspace` / stable CI (the eBPF
  program crate is a member that compiles to an empty host stub off the `bpf`
  target). **Runtime:** needs `CAP_BPF` + `CAP_NET_ADMIN`; off / missing caps /
  unsupported kernel → one warning and the unprivileged baseline is unchanged.
  The shipped systemd unit grants `CAP_BPF`/`CAP_PERFMON` (alongside
  `CAP_NET_ADMIN`) via `AmbientCapabilities` for a "just run" demo; the stock
  binary ignores them unless built `--features ebpf` with `collect.ebpf = true`.
- **Default-route flaps:** a streamed `routes/default_v4_flaps_total` counter plus
  a per-transition history ring served on `@/query/route_changes` (gateway change /
  withdrawal / re-appearance with timestamps) — the #1 connectivity incident.
- **Control-plane timeline + IPsec events (nlink 0.23):** real-time RTNETLINK
  changes fold into counters `events/{link,addr,route,neighbor}/{added,removed,
  changed}_total` and a recent-events ring (`@/query/events`). The XFRM **monitor**
  stream adds a fifth `ipsec` family — SA/policy lifecycle (`NewSa`/`DelSa`,
  soft/hard `ExpireSa`, `Acquire`, …) the periodic SA snapshot misses between
  ticks — as `events/ipsec/{added,changed,removed}_total` + timeline rows. Gated on
  `collect.events && collect.xfrm`; degrades cleanly where no IPsec is configured.
- **ethtool link health (nlink 0.23):** beyond speed/duplex/autoneg/rings/pause,
  per-interface **FEC** (`ethtool/<iface>/fec/{modes,auto}` — silent corruption on
  marginal optics) and **EEE** (`ethtool/<iface>/eee/{enabled,active}` — power-save
  that can add latency). Best-effort per family; drivers lacking one still yield the
  rest.
- **nftables firewall hit-rate (#115):** the per-rule `counter` expression is
  decoded from the raw ruleset, so beyond ruleset shape (`nft/{tables,chains,rules}
  _total`) the sensor streams monotonic `nft/{packets,bytes}_total` and per-table
  `nft/<family>/<table>/{packets,bytes}` counters; `@/query/nft` carries per-rule
  `packets`/`bytes`.
- **Alerts:** `@/alerts/<alert_key>` from sentinel expectation violations
  (sockets listen/established/forbid, links up, …).
- **Control:** `@/commands/expectations` (+ `@/status/expectations`) to
  hot-swap expectations; `@/commands/collection` (+ status) to toggle collectors.
- **Config:** `configs/netlink.json5` (`collect.*` flags, `expectations` block).
- **GUI (#270):** the netlink device screen is a tabbed, chart-driven view —
  **Overview** (bottleneck gauge + issue badges + interface status strip +
  TCP-health tiles w/ sparklines + route/neighbor chips) · **Interfaces**
  (per-iface throughput trends + ethtool link health + iface→sockets pivot) ·
  **Sockets** (first-class explorer: RTT histogram + congestion donut + paginated
  table, no silent cutoff) · **Routing & Neighbors** (route/neighbor/address
  DataTables + neighbor-state donut + default-route flap section) · **QoS/Queues**
  (per-qdisc health chips + AQM + backlog trends + qdisc tree) · **Firewall &
  IPsec** (conntrack gauge + per-proto donut + nft/xfrm DataTables) · **Events**
  (structured control-plane timeline + per-family context chart) · **WireGuard**
  (peer cards w/ handshake-age chips + rx/tx trends). Capability-gated tabs appear
  only when their data is present.

## netring

Wire-level flow / L7 / network-detection telemetry built on the `netring`
capture engine (`flowscope` parsers). Live capture needs `CAP_NET_RAW`
(`+CAP_IPC_LOCK` for AF_XDP); offline **pcap replay** needs no privileges.

- **Telemetry:** `zensight/netring/<sensor>/<metric>` — flow RED (started/ended/
  bytes/packets/retransmits/duration percentiles), per-L4 + connection-state
  composition, TCP resets, DNS RED, HTTP RED, TLS fingerprint counts, ICMP errors,
  capture health with the honest drop breakdown
  (`capture/<src>/drops` + `freezes` / `xdp/<cause>`), and the passive asset
  count (`assets/discovered`).
- **Capture overload (netring 0.27):** the windowed drop-rate feeds a hysteresis
  detector (enter 5%, recover 1% × 3 windows) that raises/clears a
  `capture-overload` SensorHealth alert — the honest "the sensor is silently
  losing your packets" signal. Tunable under `overload` in the config.
- **On-demand detail** (`@/query/<topic>`): `flows`, `tls`, `talkers?top=N`,
  `matrix?top=N`, `elephant_flows`, `dns?top=N`, `http?top=N`, `quic`, `ssh`,
  `ja4h?top=N`, `assets`.
- **Traffic matrix / service map (#122):** alongside the per-destination talker
  histogram, an `(src,dst)`-keyed byte/packet/flow matrix served on
  `@/query/matrix?top=N` — "who talks to whom" for the service-map view.
- **L7 protocol inventories (netring 0.27, opt-in):** QUIC Initial SNI/ALPN/
  version (`collect.quic`, UDP/443 — passive, no decryption) and SSH banner +
  KEXINIT HASSH fingerprints (`collect.ssh`, TCP/22), each served on its
  `@/query/*` channel with a streamed distinct-count. Cleartext SNMP v1/v2c
  community strings can be flagged as `cleartext-snmp` anomalies with
  `collect.snmp_cleartext` (build with `--features snmp`).
- **JA4H HTTP fingerprints (#124, opt-in, license-gated):** with `collect.http_fp`
  on a build that enables `--features ja4plus`, cleartext HTTP requests are
  fingerprinted with JA4H (FoxIO `a_b_c_d` form) into a per-fingerprint inventory
  served on `@/query/ja4h?top=N` — surfaced in the GUI fingerprint explorer
  alongside JA4/JA3/QUIC-SNI/HASSH. The `ja4plus` feature pulls FoxIO-License-1.1
  code (NOT OSI); the default build stays OSI-clean and the channel is absent.
  **JA4SSH is not yet available upstream** (flowscope 0.19 / netring 0.27
  fingerprint SSH via HASSH only), so the SSH side of #124 is deferred.
- **Passive asset inventory (netring 0.27):** with `collect.assets`, discovers
  hosts on the wire from ARP / NDP / LLDP (+ CDP via `collect.asset_cdp`) into a
  MAC-keyed inventory (MAC / IP / hostname / platform / capabilities / seen-via),
  served on `@/query/assets`. Covers hosts that emit no telemetry of their own.
- **Alerts:** `@/alerts/<alert_key>` from detectors and threat-intel —
  - Detectors: TRW port-scan (`anomalies.port_scan`), CV + RITA beaconing
    (`anomalies.beaconing` / `anomalies.rita_beacon`, thresholds
    `beacon_threshold` / `rita_beacon_threshold`), connection-flood
    (`anomalies.connection_flood`), DGA (`anomalies.dga`), DNS-tunneling
    (`anomalies.dns_tunnel`, `dns_tunnel_distinct` / `dns_tunnel_qname_len`), and
    Newly-Observed-Domain / NOD (`anomalies.nod`). Each carries a MITRE ATT&CK
    `technique` label (T1046 / T1071 / T1071.004 / T1568 / …) and a Community ID.
    `anomalies.allowlist` (case-insensitive substring) suppresses noisy
    destinations/SLDs; all enables/thresholds hot-swap at runtime (see below).
  - **Per-detector metric surfacing (#254):** each detector also publishes a
    monotonic `anomaly/<kind>/total` counter (e.g. `anomaly/RitaBeacon/total`,
    `anomaly/DnsTunnel/total`) — re-emitted each aggregate tick — so the GUI
    Overview anomaly strip can roll up per-detector activity without a
    Security-view round-trip. The `<kind>` slug equals the alert `rule`.
  - **Lateral movement (#123, opt-in):** SMB admin-share / `IPC$` service-pipe
    access (T1021.002), RDP connection requests (T1021.001), and Kerberos
    kerberoast / weak-etype / brute-force signals (T1558). Build with
    `--features lateral` (pulls the SMB/RDP/Kerberos parsers) and set
    `anomalies.lateral_movement`.
  - **Data exfiltration (#123, opt-in):** a per-source EWMA baseline of outbound
    flow volume flags a flow exceeding it by `exfil_sigma` stddevs above the
    `exfil_min_bytes` floor (T1048). Set `anomalies.data_exfil`.
  - **Threat-intel (netring 0.27):** flow-risk scoring (obsolete TLS, cleartext
    HTTP credentials), IOC matching (bad IPs/domains/JA3/JA4, from config lists
    or indicator files), Sigma rules (build with `--features sigma`).
- **Runtime detection tuning (#121):** the `@/commands/detectors` channel (status
  on `@/status/detectors`) hot-swaps the allowlist and each detector's
  enable/threshold without a restart — surfaced in the GUI Security view's
  *Detection Tuning* panel. A detector that was off at startup isn't built into
  the pipeline, so enabling it still needs a restart; tuning and mute/unmute of
  built detectors are immediate.
- **Config:** `configs/netring.json5` (`collect.*`, `anomalies.*`, `threat.*`,
  `pcap` for replay).
- **GUI (#257):** the netring device screen is a tabbed, chart-driven view —
  **Overview** (RED hero + per-L4 donut + live anomaly strip) · **Flows** ·
  **Talkers & Matrix** · **DNS** (RED tiles + rcode bars + top-SLD table) ·
  **HTTP/TLS** (RED + TLS/QUIC/SSH inventories, JA3+JA4) · **Bandwidth** (ranked
  bars) · **Assets** (filterable inventory) · **Security** (in-view ATT&CK
  rollup, deep-links to the global Security view) · **Capture**. Endpoints are
  drill-down pivots to filtered flows. Tabs appear only when their data is
  present.

---

## journald notes

The logs sensor's journald source (`syslog.journald`):

| Field | Meaning |
|-------|---------|
| `enabled` | turn the journald reader on |
| `scope` | `system` \| `user` \| `local_only` \| `runtime_only` |
| `namespace` | a journald log namespace, or null |
| `start_from` | `cursor` (gap-free resume) \| `tail` \| `head` \| `boot` \| `since` |
| `since` / `cursor_file` / `on_missing_cursor` | resume tuning |
| `units` / `min_priority` / `transports` / `match` | **server-side** filters (applied in the journal) |
| `detect_events` / `event_dedup_secs` / `event_severity` | known-event → alert tuning |
| `overflow` | channel-full policy under storms: `drop_newest` (default, shed + count) \| `block` (backpressure) |
| `max_eps` / `sample_ratio` | optional rate limit; beyond it keep 1-in-`sample_ratio`, count the rest as sampled-out |
| `drop_alert_ratio` | raise an `ErrorReport` once windowed loss exceeds this fraction (default 0.01) |

Under a log storm the reader sheds (or backpressures) per `overflow` and keeps
honest accounting — entries read / published / dropped / sampled-out — so a
sustained drop surfaces as an `ErrorReport` rather than silent loss. Journal
rotation (`journalctl --rotate`) is followed transparently (the `wait()`
*invalidate* is handled, not treated as EOF).

Reading the **system** journal needs journal-read access — run as a system
service or add the user to the `systemd-journal` group. The `user` scope is
always readable. Building the sensor needs `libsystemd-dev` (the `journald`
cargo feature is on by default; build with `--no-default-features` to drop it).

[`TelemetryPoint`]: ../zensight-common/src/telemetry.rs
