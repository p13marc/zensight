# zensight-sensor-netring

A ZenSight sensor that streams **wire-level telemetry and network-anomaly
alerts** from zero-copy packet capture, built on
[`netring`](https://github.com/p13marc/netring) (AF_PACKET / AF_XDP). Linux only.

It needs no NetFlow exporter device — it generates flow telemetry itself from a
span port, tap, or host NIC. Live capture needs `CAP_NET_RAW`; **offline pcap
replay needs no privileges** (set `netring.pcap`), which is also how the pipeline
is tested.

> The tables below are a representative subset. See
> [`docs/SENSORS.md#netring`](../docs/SENSORS.md#netring) and
> [`docs/KEYSPACE.md`](../docs/KEYSPACE.md) for the authoritative reference:
> flow/L4/connection-state RED, DNS/HTTP RED, TLS fingerprints, ICMP errors, a
> `(src,dst)` traffic matrix, capture-overload health, the `@/query/*` detail
> channels, and the full detector / threat-intel / asset-inventory surface.

## Telemetry

Published under `zensight/netring/<sensor>/...` (subset):

| Metric | Type | Notes |
|---|---|---|
| `bandwidth/<app>/bytes_per_sec` | Gauge | per-application top-talkers (label `app`) |
| `flow/started_total`, `flow/ended_total` | Counter | TCP flow lifecycle |
| `flow/active` | Gauge | started − ended |

## Alerts (anomalies & threat-intel)

Detectors and threat-intel emit alerts under `zensight/netring/@/alerts/<key>`
(subset):

| Detector | Rule | Notes |
|---|---|---|
| Port scan (TRW) | `PortScanTRW` | emits on `Scanner` verdict; offending IP in labels |
| RITA beaconing | beaconing | periodic C2-style callbacks |
| DNS tunnel / NOD | dns-tunnel | high-entropy / newly-observed-domain |
| Threat-intel | flow-risk / IOC / Sigma | obsolete TLS, cleartext creds, bad IP/domain/JA3/JA4 |

Opt-in detectors (lateral-movement, data-exfil) and the full list are in
`docs/SENSORS.md`. Offending IP/domain detail goes in alert **labels**, never in a
metric series name (cardinality discipline).

## Run

```bash
# Live capture (grant caps once):
sudo setcap cap_net_raw,cap_ipc_lock+ep target/release/zensight-sensor-netring
cargo run -p zensight-sensor-netring --release -- --config configs/netring.json5

# Offline replay (no privileges) — set netring.pcap in the config:
cargo run -p zensight-sensor-netring -- --config configs/netring.json5
```

## Configuration

See `configs/netring.json5`. Set either `interfaces: [...]` (live) or
`pcap: "..."` (replay). The `collect.*`, `anomalies.*`, `threat.*`, and
`overload` blocks are documented inline in the example config and in
[`docs/SENSORS.md#netring`](../docs/SENSORS.md#netring). Some detectors are
behind cargo features (`lateral`, `sigma`, `ja4plus`, `snmp`).
