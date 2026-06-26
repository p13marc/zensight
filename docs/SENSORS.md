# ZenSight Sensors Reference

ZenSight sensors translate a legacy or host-level monitoring source into the
unified [`TelemetryPoint`] model and publish it to Zenoh. Every sensor inherits
the same control-plane (`@/health`, `@/errors`, `@/alive`, `@/status`) and
key conventions from `zensight-sensor-core` — see the
[Keyspace Reference](KEYSPACE.md) for the full key tree and
[Architecture](ARCHITECTURE.md) for the runtime model.

This document is the per-sensor reference: what each sensor ingests, how to
configure it, and the exact Zenoh keys it publishes/serves.

**Conventions shared by all sensors**

- Telemetry: `zensight/<protocol>/<source>/<metric>` (payload: `TelemetryPoint`).
- Control-plane: `zensight/<protocol>/@/{health,errors,status,alive}` (+
  `devices/<device>/{liveness,alive}` where per-device tracking applies).
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
| [syslog / logs](#syslog--logs) | `syslog` | RFC 3164/5424 + systemd journald | `configs/syslog.json5`, `configs/logs.json5` | journal-read for system journal |
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

- **Telemetry:** `zensight/syslog/<hostname>/<facility>/<severity>`
  (payload value = the log message; structured fields land in labels —
  journald fields under `sd.journald.*`, `source_type` = `udp`/`tcp`/`unix`/`journald`).
- **Derived rollups** (`derived`, default on): cheap aggregates emitted every
  `derived_interval_secs` under `zensight/syslog/<host>/logs/*` — per-severity
  counters (`logs/by_severity/<level>_total`), error/warning totals
  (`logs/errors_total`, `logs/warnings_total`), top-N per-unit message/error
  counters (`logs/by_unit/<unit>/...`, capped to `top_units` + an `other`
  bucket), a `logs/units_in_failure` gauge, and journald throughput
  (`logs/journald/{read,published,dropped,sampled_out}_total`).
- **Sources:**
  - Network: UDP/TCP/Unix listeners (RFC 3164 + RFC 5424).
  - **journald** (`journald.enabled`): reads the local journal via libsystemd
    (no `journalctl` subprocess). Supports scope (system/user), server-side
    matching (`units`, `min_priority`, `transports`), cursor-based no-loss
    resume (`start_from`), and **known-event alerts** — coredump / unit-failed /
    OOM are matched by `MESSAGE_ID` and raised on `@/alerts`.
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
- **Default-route flaps:** a streamed `routes/default_v4_flaps_total` counter plus
  a per-transition history ring served on `@/query/route_changes` (gateway change /
  withdrawal / re-appearance with timestamps) — the #1 connectivity incident.
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
  `elephant_flows`, `dns?top=N`, `http?top=N`, `quic`, `ssh`, `assets`.
- **L7 protocol inventories (netring 0.27, opt-in):** QUIC Initial SNI/ALPN/
  version (`collect.quic`, UDP/443 — passive, no decryption) and SSH banner +
  KEXINIT HASSH fingerprints (`collect.ssh`, TCP/22), each served on its
  `@/query/*` channel with a streamed distinct-count. Cleartext SNMP v1/v2c
  community strings can be flagged as `cleartext-snmp` anomalies with
  `collect.snmp_cleartext` (build with `--features snmp`).
- **Passive asset inventory (netring 0.27):** with `collect.assets`, discovers
  hosts on the wire from ARP / NDP / LLDP (+ CDP via `collect.asset_cdp`) into a
  MAC-keyed inventory (MAC / IP / hostname / platform / capabilities / seen-via),
  served on `@/query/assets`. Covers hosts that emit no telemetry of their own.
- **Alerts:** `@/alerts/<alert_key>` from detectors and threat-intel —
  - Detectors: TRW port-scan, RITA beaconing, connection-flood, DGA/DNS-tunneling.
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
