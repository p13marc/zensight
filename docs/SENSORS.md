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
  `logging` block.

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

Local host metrics (CPU, memory, disk, network, load, temperatures).

- **Telemetry:** `zensight/sysinfo/<hostname>/<metric>` (e.g. `cpu/usage`,
  `memory/used`, `net/<iface>/rx_bytes`).

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
  `sockets?state=&port=`, `addresses`, `events`, `tc`, `xfrm`, `nft`.
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
  composition, TCP resets, DNS RED, HTTP RED, TLS fingerprint counts, ICMP errors.
- **On-demand detail** (`@/query/<topic>`): `flows`, `tls`, `talkers?top=N`,
  `elephant_flows`, `dns?top=N`, `http?top=N`.
- **Alerts:** `@/alerts/<alert_key>` from detectors and threat-intel —
  - Detectors: TRW port-scan, RITA beaconing, connection-flood, DGA/DNS-tunneling.
  - **Threat-intel (netring 0.27):** flow-risk scoring (obsolete TLS, cleartext
    HTTP credentials), IOC matching (bad IPs/domains/JA3/JA4, from config lists
    or indicator files), Sigma rules (build with `--features sigma`).
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

Reading the **system** journal needs journal-read access — run as a system
service or add the user to the `systemd-journal` group. The `user` scope is
always readable. Building the sensor needs `libsystemd-dev` (the `journald`
cargo feature is on by default; build with `--no-default-features` to drop it).

[`TelemetryPoint`]: ../zensight-common/src/telemetry.rs
