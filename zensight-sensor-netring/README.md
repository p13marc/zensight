# zensight-sensor-netring

A ZenSight sensor that streams **wire-level flow / L7 / network-detection (NDR)
telemetry and anomaly alerts** from zero-copy packet capture, built on
[`netring`](https://github.com/p13marc/netring) (AF_PACKET / AF_XDP) with the
`flowscope` parsers. **Linux only.**

It needs no NetFlow exporter device — it generates flow telemetry itself from a
span port, tap, or host NIC. **Live capture needs `CAP_NET_RAW`**
(`+CAP_IPC_LOCK` for AF_XDP); **offline pcap replay needs no privileges**
(set `netring.pcap`), which is also how the pipeline is tested. It publishes
under `zensight/netring/<sensor>/...` and the `zensight/netring/@/...` control
plane.

## Quick start

```bash
# Live capture (grant caps once, then run unprivileged):
sudo setcap cap_net_raw,cap_ipc_lock+ep target/release/zensight-sensor-netring
cargo run -p zensight-sensor-netring --release -- --config configs/netring.json5

# Offline replay (no privileges) — set netring.pcap in the config:
cargo run -p zensight-sensor-netring -- --config configs/netring.json5
```

## Configuration

Set either `netring.interfaces: [...]` (live) or `netring.pcap: "..."` (replay).
See [`configs/netring.json5`](../configs/netring.json5) and
[`docs/configuration.md`](docs/configuration.md) for the full `collect.*`,
`anomalies.*`, `threat.*`, `overload`, `capture`, and `bandwidth_attribution`
blocks.

## Cargo detector features

All detector features are **off by default** so the shipped build stays
OSI-clean and lean; enable them at build time to compile the extra parsers in.

| Feature | Effect |
|---|---|
| `lateral` | Lateral-movement detection: SMB/RDP/Kerberos parsers (ATT&CK T1021/T1558) |
| `sigma` | Sigma rule evaluation (`sigma-rust`) over flow observations |
| `yara` | YARA payload scanning (`yara-x`) with runtime rule hot-reload |
| `ja4plus` | JA4 / JA4H fingerprints — **FoxIO License 1.1 (NOT OSI)**; default build omits |
| `snmp` | Cleartext-SNMP v1/v2c community-string detection |

## Documentation

- [`docs/telemetry.md`](docs/telemetry.md) — the full published keyspace: flow /
  L4 / connection-state RED, DNS / HTTP RED, TLS/QUIC/SSH fingerprints,
  encrypted-DNS, ICMP errors, traffic matrix, bandwidth, capture health, and the
  `@/query/*` detail channels.
- [`docs/detectors.md`](docs/detectors.md) — the NDR + threat-intel +
  asset-inventory surface, runtime tuning / hot-reload contracts, and MITRE
  ATT&CK tagging.
- [`docs/configuration.md`](docs/configuration.md) — every config block.
- [`../docs/KEYSPACE.md`](../docs/KEYSPACE.md) — the authoritative,
  cross-sensor key-expression contract.
