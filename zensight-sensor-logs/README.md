# zensight-sensor-logs

Unified logs sensor for the ZenSight observability platform. Ingests **network
syslog** (RFC 3164 / RFC 5424 over UDP/TCP/Unix) **and/or** the local **systemd
journald**, feeds both into one model, and publishes derived rollups plus
per-unit SLO alerts to Zenoh.

Per-line log events are high-cardinality, so they are **served on demand** from a
bounded in-memory ring (`@/query/events`), never streamed onto the telemetry bus
(#358). Only low-rate rollups (`logs/by_severity/*`, `logs/by_unit/*`,
`logs/ingest/*`, …) ride the bus for charts and alerts.

## Features

- **RFC 3164 / RFC 5424** — BSD + modern syslog, structured data, RFC 6587 framing
- **UDP / TCP / Unix** listeners (`/dev/log`, custom sockets)
- **journald** — reads the local journal via libsystemd (no `journalctl`
  subprocess): scope/namespace, server-side matching, cursor-based gap-free
  resume, known-event alerts (coredump / unit-failed / OOM), drop accounting
- **Per-line events** with the OpenTelemetry logs data model in labels, each with
  a unique time-sortable `uid` (no last-writer-wins loss)
- **Multiline joining** — fold Java/Python/Go stack traces into one event
- **Template mining** (Drain3-style) + novelty / rate-spike detection
- **Derived rollups & SLOs** — per-unit log-rate / error rollups, error budgets,
  burn-rate alerts

## Quick start

```bash
# Network syslog listeners (journald block commented out)
cargo run -p zensight-sensor-logs --release -- --config configs/syslog.json5

# journald-only (used by `just run`)
cargo run -p zensight-sensor-logs --release -- --config configs/logs.json5
```

One-liner config (network):
`{ zenoh: { mode: "peer" }, syslog: { listeners: [{ protocol: "udp", bind: "0.0.0.0:1514" }] } }`.
At least one listener **or** enabled journald is required.

> journald ingestion needs `libsystemd-dev` at build time (the `journald` cargo
> feature is on by default; build `--no-default-features` to drop it). Reading the
> **system** journal needs journal-read access — run as a system service or join
> the `systemd-journal` group; the `user` scope is always readable.

## Documentation

- [Telemetry reference](docs/telemetry.md) — rollup keys + the `@/query/events` queryable.
- [Filtering](docs/filtering.md) — static + dynamic message filters, journald matching.
- [Configuration](docs/configuration.md) — listeners, journald, SLO/alert blocks.
- [../docs/KEYSPACE.md](../docs/KEYSPACE.md) — authoritative key-expression contract.
- [docs/telemetry.md](docs/telemetry.md) — the canonical per-sensor reference.

## License

MIT OR Apache-2.0
