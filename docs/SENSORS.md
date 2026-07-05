# ZenSight Sensors Reference

ZenSight sensors translate a legacy or host-level monitoring source into the
unified [`TelemetryPoint`] model and publish it to Zenoh. Every sensor inherits
the same control-plane (`@/health`, `@/errors`, `@/alive`, `@/status`, and the
opt-in `@/artifact`) and key conventions from `zensight-sensor-core` — see the
[Keyspace Reference](KEYSPACE.md) for the full key tree and
[Architecture](ARCHITECTURE.md) for the runtime model.

This document is the per-sensor reference: what each sensor ingests, how to
configure it, and the exact Zenoh keys it publishes/serves.

**Conventions shared by all sensors**

- Telemetry: `zensight/<protocol>/<source>/<metric>` (payload: `TelemetryPoint`).
- Control-plane: `zensight/<protocol>/@/{health,errors,status,alive}` (+
  `devices/<device>/{liveness,alive}` where per-device tracking applies).
- Large-data artifacts: every sensor can serve on-demand large-data artifacts
  over the unified `@/artifact/*` channel — a redacted `tar.zst` **report** bundle
  (config + health + counters, Tier-1 blob delivery) and a directory **snapshot**
  (Tier-2 content-addressed tree). The **netring** sensor additionally serves an
  on-demand pcap **capture** (issue #333) off a live packet tap. **Opt-in per
  kind** via the `artifacts.{report,snapshot}` config section (plus netring's
  `capture.on_demand`); every kind is disabled by default. See KEYSPACE.md §3.1a.
- Config: a JSON5 file under [`configs/`](../configs/); pass with `--config`.
  Every config has a `zenoh` block (`mode`, `connect`, `listen`) and a
  `logging` block. The `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` env vars override
  the `zenoh` block (used by `just run` to pin a loopback rendezvous).
- **`source` (identity, #301):** every sensor takes a `source` config field — the
  `<source>` chunk in its telemetry keys and its host identity. It defaults to the
  local hostname (`"auto"`). This unified the old per-sensor names: netlink
  `hostname`, netring `sensor_id`, and sysinfo `hostname` are all now `source`. On
  remote-device sensors (snmp, gnmi, modbus, netflow, logs) `source` names the
  **agent host** (used for identity evidence and artifact routing), while each
  observed device keeps its own `<source>` in telemetry keys. Every sensor also
  publishes a self-report `HostEvidence` claim and a `SensorInfo` registration
  keyed by `source` (see KEYSPACE.md §4.1).
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
| [systemd](#systemd) | `systemd` | systemd D-Bus Manager (unit state + boot perf) | `configs/systemd.json5` | none (read-only system-bus) |

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
  `udp`/`tcp`/`unix`/`journald`. Among the journald fields is
  `sd.journald.invocation_id` (#303) — the unit's `_SYSTEMD_INVOCATION_ID`, which
  joins a log line to the exact systemd unit invocation that produced it (matches
  `UnitDetail.invocation_id`).
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

- **Telemetry:** `zensight/sysinfo/<source>/<metric>`. The metric families:

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
  request rather than streamed. Each `ProcessRecord` carries identity/context
  fields (#302): `cmdline`, `exe`, `ppid`, `cgroup` (v2 path — the join key to a
  systemd unit's `control_group`), `start_time` (stat field-22 ticks — the
  `(pid, start_time)` identity pair used fleet-wide), and `user`. The command line
  is **scrubbed of secret-looking argv values** before publish and byte-capped;
  tune via `processes.scrub_args` (default `true`), `processes.custom_sensitive_words`,
  and `processes.strip_proc_arguments`.
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
  `sockets?state=&port=`, `addresses`, `events`, `route_changes`, `tc`, `xfrm`, `nft`,
  `bandwidth?top=N`.
- **nlink 0.24 sockdiag depth (#322):** per-rule nft counters now come from nlink's
  native `RuleInfo::counter()` (the hand-rolled TLV parser is gone); `@/query/sockets`
  rows gain structured **congestion-control** info — BBR bottleneck bandwidth
  (`bbr_bw_bps`) + min-RTT (`cc_min_rtt_us`) when a socket runs BBR (cubic/reno report
  none; the per-algorithm fleet count already ships as `sockets/tcp/by_cong/<algo>`);
  and a port-filtered `sockets` query now compiles the selector to **kernel-side**
  INET_DIAG bytecode (`FilterExpr`, local-OR-remote port) so the kernel returns fewer
  rows, with the client-side match kept as a backstop.
- **Per-process bandwidth (#317, epic #320, `collect.bandwidth`, default on):** the
  **unprivileged, TCP-only** bandwidth tier. A background sampler diffs each socket's
  `tcp_info` goodput byte counters (`bytes_acked` = TX, `bytes_received` = RX) **per
  cookie** (never the reusable inode) every couple of seconds; `@/query/bandwidth`
  then runs the #304 `/proc` attribution and returns per-process `BandwidthRecord`s
  (`bw.source=sock_diag`, `bw.semantics=app-goodput`, `bw.proto=tcp`), ranked by rate,
  top-N. Hard limits are labelled, not hidden: **TCP only** (`udp_diag` exposes no
  per-socket byte counters — per-process UDP needs the eBPF tier), **app-goodput**
  (below wire — no headers/retransmits), and short flows opened+closed between samples
  are missed. Sockets whose owner has exited fold into one explicit `unattributed`
  bucket (`pid = -1`) rather than being dropped.
- **Socket → process attribution (#304):** with `collect.socket_processes`
  (default on), each `sockets` `SocketRecord` is annotated — unprivileged — with
  `cookie` (stable socket id; prefer over the reusable inode), `cgroup_id`/`cgroup`
  (v2 path), and the owning `pid`/`process`/`proc_start_time` (the
  `(pid, start_time)` identity pair). Attribution runs a `/proc` fd-scan **per
  query request** (in a blocking task, duration logged), skipped above
  `socket_process_max_procs` (default 4096). Sockets whose owner has exited stay
  unattributed (`—`). An optional eBPF tier annotates recently-closed and
  live-established sockets the fd-scan can't see.
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
- **Rule / nexthop / MDB / netns event families (#323, nlink 0.24):** the event
  stream also folds **policy-routing rules** (`ip rule` add/del — the classic
  "why is routing weird" incident *and* a traffic-redirect primitive), **nexthop
  objects**, **bridge multicast-DB** entries and **netns ids** (container
  lifecycle signal) into `events/{rule,nexthop,mdb,nsid}/*_total` + timeline rows
  with human detail (rule: priority/selector/action/table). A `NewRule`/`DelRule`
  re-evaluates the sentinel instantly. The sentinel gains a **`rules`
  expectation kind** — `forbid` (default: fire on any non-baseline rule matching
  the optional `priority`/`table` selectors; the kernel's 0/32766/32767 lookup
  rules never count) or `require` (fire when the matching rule is missing).
  Violations are tagged ATT&CK **T1599** (Network Boundary Bridging) and appear
  in the GUI Security view's tactic lens.
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
- **Identity evidence feed (#307):** with `evidence` (default on), the neighbor
  poll publishes observed-neighbor `HostEvidence` (ARP/ND table → MAC↔IP,
  `observer=netlink`) to the correlator's `_meta/evidence/**` keyspace, with the
  same rate-limiting and TTL aging as the netring feed.
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
- **Encrypted-traffic frontier (netring 0.29, #326):** the QUIC and SSH handshakes
  now use netring's typed fingerprint handlers — QUIC yields its royalty-free
  `q`-prefixed JA4, a post-quantum key-share flag, and app-protocol; SSH yields
  both client HASSH and server HASSH-Server plus the offered KEXINIT algorithms.
  TLS fingerprints carry the PQ key-share flag, aggregated into a streamed
  `tls/pq_ratio` PQ-readiness gauge (GUI badge + stat). With
  `collect.encrypted_dns`, DoT/DoQ/DoH sessions are classified from the handshake
  into streamed `dns/encrypted/*` counts + an `@/query/encrypted_dns` inventory
  (GUI "Encrypted DNS" panel); arming `anomalies.encrypted_dns_bypass` (optionally
  with a `dns_resolver_allowlist`) fires an `encrypted_dns_bypass` anomaly (ATT&CK
  **T1572**) for a session to an un-sanctioned resolver — the DNS-tunnel / policy-
  bypass signal. `collect.ip_reassembly` reassembles IP fragments before L7
  parsing so fragmented DNS/handshakes still parse. (A programmatic
  fragmentation-overlap *evasion* anomaly is deferred — netring 0.29 emits the
  overlap counter only as a log warning, no getter yet.)
- **JA4H HTTP fingerprints (#124, opt-in, license-gated):** with `collect.http_fp`
  on a build that enables `--features ja4plus`, cleartext HTTP requests are
  fingerprinted with JA4H (FoxIO `a_b_c_d` form) into a per-fingerprint inventory
  served on `@/query/ja4h?top=N` — surfaced in the GUI fingerprint explorer
  alongside JA4/JA3/QUIC-SNI/HASSH. The `ja4plus` feature pulls FoxIO-License-1.1
  code (NOT OSI); the default build stays OSI-clean and the channel is absent.
  **JA4SSH is not yet available upstream** (flowscope 0.19 / netring 0.27
  fingerprint SSH via HASSH only), so the SSH side of #124 is deferred.
- **Passive asset inventory (netring 0.27, enriched 0.22/#329):** with
  `collect.assets`, discovers hosts on the wire from ARP / NDP / LLDP (+ CDP via
  `collect.asset_cdp`) into a MAC-keyed inventory served on `@/query/assets`.
  Records carry MAC / IPs / hostname(s) / vendor / platform / capabilities /
  seen-via plus (netring 0.29) a classified **role** (router / switch /
  access-point / phone / iot / host), **first-seen** + **source-count** confidence,
  the full **hostname set**, per-parser **fingerprints** (JA3 / JA4 / HASSH / p0f)
  for cross-pivoting to the fingerprint explorer, and (on `ja4plus` builds) x509
  subject/SANs. The GUI Inventory view adds a role filter, first-seen sort, and
  fingerprint pivots. Covers hosts that emit no telemetry of their own.
- **Passive DNS name resolution (#308):** with `collect.dns` + `names` (default
  on), DNS answers are parsed (flowscope `NameMap` — follows CNAME chains,
  glue-poisoning-safe, PTR-aware) into a client-scoped IP↔name cache. Flow and
  talker records gain provenance-ranked names (`dst_names` on `flows`, `names` on
  `talkers`), and an **FQDN-pivoted RITA beacon** detector
  (`anomalies.rita_beacon_fqdn`, ATT&CK T1071) flags periodic beaconing keyed by
  destination name rather than IP.
- **Identity evidence feeds (#307):** with `evidence` (default on), netring
  publishes observed-asset `HostEvidence` (from the asset inventory,
  `observer=netring`) and passive-DNS `NameObservation`s to the correlator's
  `_meta/evidence/**` keyspace, rate-limited (per-source min-interval + per-tick
  cap) and TTL-aged. **Gating:** asset evidence requires `collect.assets`; name
  evidence requires `collect.dns` — with those collectors off (the shipped
  default), netring emits no evidence even though `evidence.enabled` is true.
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
    Beaconing keys its state on the host-pair (src, dst, dst-port) and the port
    scan on the source host, so activity that rotates its source port each
    connection stays one series instead of fragmenting (#324); the alert still
    carries the triggering flow's full 5-tuple + Community ID.
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
    or indicator files), Sigma rules (build with `--features sigma`), and YARA
    payload scanning (build with `--features yara`, `threat.yara.file`).
- **Runtime threat-intel reload (#328):** the `@/commands/threat_intel` channel
  (status on `@/status/threat_intel`) hot-swaps the live **IOC** set (`set_ioc` /
  `reload_ioc_files` / `clear_ioc`) and **YARA** rules (`set_yara`, `--features
  yara`) without a restart — surfaced in the GUI Security view's *Threat Intel*
  panel. A bad YARA source is rejected with a compile error in the status reply
  and the previous rules keep scanning. The matchers are frozen at build, so set
  `threat.reload = true` (or provide startup indicators / a `threat.yara.file`) to
  arm them; otherwise a reload of an unarmed matcher is a reported no-op.
- **Runtime detection tuning (#121):** the `@/commands/detectors` channel (status
  on `@/status/detectors`) hot-swaps the allowlist and each detector's
  enable/threshold without a restart — surfaced in the GUI Security view's
  *Detection Tuning* panel. A detector that was off at startup isn't built into
  the pipeline, so enabling it still needs a restart; tuning and mute/unmute of
  built detectors are immediate.
- **On-demand pcap capture (#333, opt-in):** with `capture.on_demand.enabled`, an
  operator can pull a bounded packet capture over the unified `@/artifact`
  channel (GUI *Capture* tab or any client). A dedicated reloadable packet-tier
  tap streams frames to a `pcap[.zst]` file, delivered as a Tier-1 blob; every
  request is clamped to the configured limits (`max_duration_secs` / `max_bytes`
  / `snaplen_max`), an optional per-request filter *narrows* the capture
  (`allow_filter`), and `compress` zstd-encodes it. Rate-limited by
  `cooldown_secs`, reaped after `ttl_secs`. **Limitation:** the packet tier only
  sees IP/L4 frames — non-IP traffic (ARP/LLDP) is not captured. Backpressure is
  drop-with-count (a lossy capture never stalls global telemetry; drops are
  surfaced in the progress line).
- **Config:** `configs/netring.json5` (`collect.*`, `anomalies.*`, `threat.*`,
  `capture.on_demand.*`, `pcap` for replay).
- **GUI (#257):** the netring device screen is a tabbed, chart-driven view —
  **Overview** (RED hero + per-L4 donut + live anomaly strip) · **Flows** ·
  **Talkers & Matrix** · **DNS** (RED tiles + rcode bars + top-SLD table) ·
  **HTTP/TLS** (RED + TLS/QUIC/SSH inventories, JA3+JA4) · **Bandwidth** (ranked
  bars) · **Assets** (filterable inventory) · **Security** (in-view ATT&CK
  rollup, deep-links to the global Security view) · **Capture**. Endpoints are
  drill-down pivots to filtered flows. Tabs appear only when their data is
  present.

## systemd

Reads the `org.freedesktop.systemd1.Manager` interface on the **system D-Bus**
(read-only, unprivileged) and publishes system-level unit/service state plus
boot-performance timings. Complements `sysinfo` (hardware) and `logs` (messages)
with the *unit* dimension. Fails gracefully on non-systemd hosts (reports
unhealthy on `@/health`, retries — never crashes).

- **Telemetry:** `zensight/systemd/<host>/<metric>`, refreshed every
  `poll_interval_secs` (default 15):
  - **Manager scalars** (`collect` always): `manager/{n_names,n_failed_units,
    n_jobs,n_installed_jobs}` (Gauge).
  - **Unit-state aggregates** (`collect.list_units`, default on, from `ListUnits`):
    `units/{total,active,failed,loaded,inactive}` (Gauge).
  - **Boot performance** (`collect.boot`, default on, Gauge µs): `boot/{firmware,
    loader,kernel,initrd,userspace,total}_usec`, computed from the Manager
    monotonic timestamps exactly like `systemd-analyze`.
  - **Per-unit watchlist** (#273, `watch_units` globs, capped by `watch_max`):
    `unit/<name>/{active,state,restarts_total,active_since_usec,mem_bytes,
    cpu_usec,tasks,exit_code}` (+ `ip/io_*_bytes` when `ip_io_accounting` and the
    unit enables accounting). Overflow → `other/units_total`.
  - **Per-service bandwidth** (#315, the cheapest bandwidth-by-* tier — epic #320):
    with `ip_io_accounting`, successive `IPIngressBytes`/`IPEgressBytes` deltas give
    `unit/<name>/{ip_ingress_bps,ip_egress_bps}` (bytes/sec). These are
    **wire-L3** (cgroup_skb: L3+ bytes incl. retransmits, no L2 framing) and carry
    `bw.source=systemd`/`bw.semantics=wire-l3` labels so the GUI never blends them
    with app-goodput (sock_diag/eBPF) or wire-L2 (capture) numbers. A unit restart
    resets the counters → that tick is re-baselined (no negative spike). An *active*
    unit with IPAccounting off emits `unit/<name>/ip_accounting=false` (an explicit
    "off" state, not a silent zero).
  - **Timers/sockets** (#279): watched `.timer` units add `unit/<t>/{last_trigger_usec,
    next_trigger_usec}`; watched `.socket` units add `unit/<s>/{n_accepted,
    n_connections,n_refused}`.
  - **Mounts** (#279, opt-in `collect.mounts`): `mounts/{total,mounted,failed}`.
  - **Journal health** (#279, opt-in `collect.journal`): `journal/{disk_usage_bytes,
    disk_available_bytes}` (unprivileged file-size walk + statvfs).
  - **Event counters** (#275): `events/<kind>_total` (unit_new/removed,
    job_new/removed).
- **On-demand queries** (never streamed): `@/query/units` → `Vec<UnitRecord>`,
  `@/query/failed` (failed only), `@/query/unit?name=<u>` → `UnitDetail`
  (props + deps; identity fields **#303**: `main_pid` + `main_pid_start_time` (the
  `(pid, start_time)` pair joining to a sysinfo process / netlink socket owner),
  `invocation_id` (hex — the same id journald stamps as `_SYSTEMD_INVOCATION_ID`,
  joining a unit to its log lines), and `control_group` (joins to a process
  `cgroup`); detail-only, not streamed), `@/query/timers` → `Vec<TimerRecord>` (with `overdue` flag, #279),
  `@/query/events` → recent control-plane timeline, `@/query/cgroups[?path=<rel>]`
  → `CgroupNode` tree (systemd-cgls-style slice→service→scope with per-node
  mem/cpu/tasks/io + pid/comm, #280; unprivileged `/sys/fs/cgroup` walk, capped).
- **D-Bus event stream** (#275): `Manager.Subscribe()` → watched UnitNew/Removed +
  JobNew/Removed → the bounded timeline ring; job completions carry the
  `ActiveState` from→to transition. Nudges the sentinel for instant re-eval.
- **Threshold alerts** (#276, `alerts.*`) on `@/alerts/*`: `systemd-unit-failed`
  (state-based; dedups with the logs sensor's `MESSAGE_ID`-based rule — set
  `alerts.unit_failed=false` to defer), `systemd-system-degraded`,
  `systemd-restart-storm`, `systemd-timer-overdue`, `systemd-unit-mem`.
- **Sentinel** (#277, `expectations`) — declarative service-health expectations
  (`expect service/target active`, `expect timer triggered_within`,
  `restarts_rate < N/window`, `forbid failed`) → `@/alerts/*`, hot-swappable via
  `@/commands/expectations` (+ `@/status/expectations` queryable). Mirrors the
  netlink sentinel.
- **Gated service control** (#283, `actions.*`, **DEFAULT OFF**): `@/commands/action`
  `{verb,unit}` (start/stop/restart/reload) → `@/status/action`. The unit is
  validated against `actions.allow_units`; the job is tracked to completion via
  `JobRemoved`; every request is audit-logged. Authorization is delegated to
  systemd/polkit (run as root, or a scoped polkit rule granting
  `org.freedesktop.systemd1.manage-units` for the allowlisted units). Uses
  `StartUnit`, not `StartTransientUnit`. No `@/commands/action` channel exists
  unless explicitly enabled.
- **Exporters** (#282): per-unit series export with a clean name + `unit` label
  (e.g. `zensight_systemd_unit_active{unit="sshd.service"}`) via the shared
  `zensight-common::semconv` table; aggregates + alerts flow through unchanged.
- **Config:** `configs/systemd.json5` (`systemd.{key_prefix,poll_interval_secs,
  source,watch_units,watch_max,ip_io_accounting,events_capacity,alerts,
  expectations,cgroup,actions,collect.*}`).

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
