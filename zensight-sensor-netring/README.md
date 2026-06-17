# zensight-sensor-netring

A ZenSight sensor that streams **wire-level telemetry and network-anomaly
alerts** from zero-copy packet capture, built on
[`netring`](https://github.com/p13marc/netring) (AF_PACKET / AF_XDP). Linux only.

It needs no NetFlow exporter device — it generates flow telemetry itself from a
span port, tap, or host NIC. Live capture needs `CAP_NET_RAW`; **offline pcap
replay needs no privileges** (set `netring.pcap`), which is also how the pipeline
is tested.

## Telemetry

Published under `zensight/netring/<sensor>/...`:

| Metric | Type | Notes |
|---|---|---|
| `bandwidth/<app>/bytes_per_sec` | Gauge | per-application top-talkers (label `app`) |
| `flow/started_total`, `flow/ended_total` | Counter | TCP flow lifecycle |
| `flow/active` | Gauge | started − ended |

## Alerts (Pillar A — anomalies)

Detectors emit alerts under `zensight/netring/@/alerts/<key>`:

| Detector | Rule | Notes |
|---|---|---|
| Port scan (TRW) | `PortScanTRW` | emits on `Scanner` verdict; offending IP in labels |

Offending IP/domain detail goes in alert **labels**, never in a metric series
name (cardinality discipline).

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
`pcap: "..."` (replay).

## Status

Implements flow + bandwidth telemetry (Plan 05) and port-scan anomaly alerts
(Plan 06). Anomaly→Alert mapping and telemetry mapping are unit-tested; the live
pipeline is exercised via pcap replay.
