# zensight-sensor-systemd

systemd unit/service/boot telemetry for the ZenSight observability platform.
Reads the `org.freedesktop.systemd1.Manager` interface on the **system D-Bus**
(read-only, unprivileged) and publishes system-level unit state, boot-performance
timings, and per-unit series to Zenoh. It adds the *unit* dimension that
complements `sysinfo` (hardware) and `logs` (messages). Fails gracefully on
non-systemd hosts — reports unhealthy on its `state/systemd/health` document
and retries, never crashes.

## What it does

- **Unit-state aggregates + Manager scalars** — `units/{total,active,failed,…}`
  and `manager/*`, refreshed every `poll_interval_secs` (default 15).
- **Boot performance** — `boot/{firmware,loader,kernel,initrd,userspace,total}_usec`,
  computed from the Manager monotonic timestamps exactly like `systemd-analyze`.
- **Per-unit watchlist** (#273) — glob-scoped `unit/<name>/*` series (active
  state, restarts, memory, CPU, tasks, and optional IP/IO accounting including a
  per-service bandwidth tier).
- **On-demand queries** — units, failed units, unit detail, timers, events, and
  a `systemd-cgls`-style cgroup tree, served as `@rpc/systemd/*` read
  procedures (never streamed).
- **Embedded sentinel** (#277) — declarative service-health expectations →
  `state/systemd/alert/*`, hot-swappable at runtime via
  `@rpc/systemd/expectations/set`.
- **Threshold alerts** (#276) — unit-failed, system-degraded, restart-storm,
  timer-overdue, unit-mem.
- **Gated service control** (#283, **default OFF**) — an opt-in, allowlisted,
  polkit-authorized `start/stop/restart/reload` surface. The sensor is strictly
  read-only unless this is explicitly enabled.

## Quick start

```bash
cargo build -p zensight-sensor-systemd --release
cargo run -p zensight-sensor-systemd --release -- --config configs/systemd.json5
```

Reading the system Manager works unprivileged. The gated action surface needs
extra authorization (root or a scoped polkit rule) — see
[docs/units-and-actions.md](docs/units-and-actions.md).

## Configuration

JSON5, top-level blocks `zenoh` / `systemd` / `logging` (plus optional
`artifacts`). The watchlist, sentinel, and actions are all opt-in; see
[docs/configuration.md](docs/configuration.md). Minimal:

```json5
{ zenoh: { mode: "peer" }, systemd: { poll_interval_secs: 15 } }
```

## Documentation

- [docs/telemetry.md](docs/telemetry.md) — published keys, boot math, per-unit
  series, on-demand queries, alerts.
- [docs/units-and-actions.md](docs/units-and-actions.md) — the watchlist, the
  sentinel expectation language, and the **security-sensitive gated action
  surface** (authorization & gating).
- [docs/configuration.md](docs/configuration.md) — every config key.
- [../docs/KEYSPACE.md](../docs/KEYSPACE.md) — the authoritative key-expression
  contract.

## License

Apache-2.0.
