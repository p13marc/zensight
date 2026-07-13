# zensight-sensor-netlink

Streams **Linux kernel networking ground truth** as ZenSight telemetry, built on
[`nlink`](https://github.com/p13marc/nlink). Linux only.

Unlike SNMP, this needs no agent or daemon on the observed host — it reads the
kernel directly via **RTNETLINK + `sock_diag`**, and the baseline reads are
**unprivileged** (no `CAP_NET_ADMIN`). It covers interfaces/addresses/routes/
neighbors, enriched TCP socket state (`tcp_info` delivery/pacing/retrans/reorder,
BBR congestion control), qdisc/bufferbloat health, ethtool link health, a
real-time control-plane change timeline, and per-process TCP bandwidth.

Two extras sit on top of the raw telemetry:

- **Socket → process attribution (#304):** each `@rpc/netlink/sockets` row is
  joined — unprivileged — to its owning process (`cookie`/`cgroup`/`pid`/
  `start_time`) via a per-request `/proc` fd-scan.
- **Embedded sentinel:** declared expectations over sockets/links/routes/rules
  raise alerts on deviation, hot-swappable at runtime via the
  `@rpc/netlink/expectations/set` procedure.
- **Optional eBPF tier (#114, off by default):** connection lifecycle + latency
  the `sock_diag` snapshot cannot see (opt-in build, `--features ebpf`).

## Quick start

```bash
cargo run -p zensight-sensor-netlink --release -- --config configs/netlink.json5
```

One-liner config: `{ zenoh: { mode: "peer" }, netlink: { source: "auto" } }` —
everything under `collect.*` defaults on except `nftables`/`conntrack`/`ebpf`
(which need extra privilege or an opt-in build).

## Documentation

- [Telemetry reference](docs/telemetry.md) — published keys + `@rpc` detail procedures.
- [Sentinel](docs/sentinel.md) — expectations, alerts, hot-swap control.
- [Configuration](docs/configuration.md) — every config block.
- [../docs/KEYSPACE.md](../docs/KEYSPACE.md) — authoritative key-expression contract.
- [docs/telemetry.md](docs/telemetry.md) — the canonical per-sensor reference.

## Privilege summary

| Capability | Feature |
|---|---|
| none (unprivileged) | interfaces, addresses, routes, neighbors, sockets, tcp_info, ethtool, TC, xfrm, socket→process, per-process TCP bandwidth |
| `CAP_NET_ADMIN` | `collect.nftables`, `collect.conntrack`, full WireGuard peer data |
| `CAP_BPF` + `CAP_NET_ADMIN` | `collect.ebpf` (also needs a `--features ebpf` build) |

## License

MIT OR Apache-2.0
