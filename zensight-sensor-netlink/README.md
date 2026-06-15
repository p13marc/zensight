# zensight-sensor-netlink

A ZenSight sensor that streams **Linux kernel networking ground truth** as
telemetry, built on [`nlink`](https://github.com/p13marc/nlink). Linux only.

Unlike SNMP, this needs no agent or daemon on the observed host — it reads the
kernel directly via netlink, and the reads are **unprivileged** (no
`CAP_NET_ADMIN`).

## Telemetry

Published under `zensight/netlink/<host>/...`:

| Metric | Type | Notes |
|---|---|---|
| `iface/<name>/rx_bytes`, `tx_bytes`, `rx_packets`, `tx_packets`, `rx_errors`, `tx_errors`, `rx_dropped`, `tx_dropped` | Counter | per-interface, label `ifindex` |
| `iface/<name>/up` | Boolean | admin/oper up |
| `iface/<name>/carrier` | Boolean | physical carrier |
| `iface/<name>/oper_state` | Text | `up`/`down`/`lowerlayerdown`/... |
| `iface/<name>/mtu` | Gauge | |
| `iface/<name>/info` | Text | MAC address (label `mac`) |
| `sockets/tcp/established`, `listen`, `time_wait`, `syn_sent`, `close_wait` | Gauge | counts by TCP state |
| `sockets/tcp/retransmits_total` | Counter | summed across sockets |
| `sockets/tcp/max_rtt_us` | Gauge | worst RTT observed |

## Run

```bash
cargo run -p zensight-sensor-netlink --release -- --config configs/netlink.json5
```

## Configuration (JSON5)

```json5
{
  zenoh: { mode: "peer" },
  netlink: {
    key_prefix: "zensight/netlink",
    hostname: "auto",          // or a fixed name
    poll_interval_secs: 5,
    collect: { interfaces: true, sockets: true },
    interfaces: {
      include: [],             // empty = all
      exclude: [],
      exclude_loopback: false,
      exclude_virtual: false,  // docker*, veth*, br-*, virbr*, vnet*, tap*
    },
  },
  logging: { level: "info" },
}
```

## Status

Implements the telemetry collector (Plan 03). The expectation/alert engine
(Plan 04 — "socket must be listening", "interface must be up", ...) is built on
top of this sensor's netlink access.
