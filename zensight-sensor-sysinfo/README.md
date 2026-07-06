# zensight-sensor-sysinfo

Local host-metrics sensor for the ZenSight observability platform. Collects
USE-method host telemetry — CPU, memory, disk, network, system, and (opt-in)
processes — plus a Linux saturation/error surface (PSI, vmstat, cgroup-v2,
thermal/power) and publishes it to Zenoh as `TelemetryPoint`s.

## What it does

- **USE-method host metrics** — CPU (per-core usage, frequency, times), memory
  (RAM/swap/composition), disk (space + I/O), network (per-interface counters +
  extended drops/fifo), and system (uptime, load, boot time).
- **Linux saturation surface** — PSI, `/proc/stat` derivatives, run-queue depth,
  schedstat, softnet, conntrack, FD/inode ceilings, netstat errors, ECC/EDAC and
  md-RAID health. Linux-only families degrade gracefully (an absent `/proc`/`/sys`
  file is skipped, never emitted as a zero).
- **Derived saturation score** — a `0..100` host saturation score plus a coarse
  `ok`/`warn`/`crit` health state, blended from the already-collected USE signals.
- **Threshold alerting** — OOM / PSI / disk / inode / FD / thermal / swap rules
  published on `zensight/sysinfo/@/alerts/*`.
- **Process explorer** — a per-pid firehose served on demand at
  `@/query/processes` (never streamed), with secret-scrubbed command lines (#302).
- **Optional eBPF saturation histograms** (#99) — `runqlat` + `biolatency` log2
  histograms on `@/query/latency`; opt-in build (`--features ebpf`) and off by
  default.

## Quick start

```bash
cargo build -p zensight-sensor-sysinfo --release
cargo run -p zensight-sensor-sysinfo --release -- --config configs/sysinfo.json5
```

## Configuration

JSON5, three top-level blocks (`zenoh` / `sysinfo` / `logging`, plus optional
`artifacts`). Metric families are gated by `sysinfo.collect.*`; see
[docs/configuration.md](docs/configuration.md). Minimal:

```json5
{ zenoh: { mode: "peer" }, sysinfo: { poll_interval_secs: 5 } }
```

## Documentation

- [docs/telemetry.md](docs/telemetry.md) — published keys, saturation score,
  process/latency queries, alerts.
- [docs/collectors.md](docs/collectors.md) — the USE collectors, saturation
  model, and alert thresholds.
- [docs/configuration.md](docs/configuration.md) — every `collect.*` flag, poll
  interval, filters, alert thresholds, and the eBPF feature.
- [../docs/KEYSPACE.md](../docs/KEYSPACE.md) — the authoritative key-expression
  contract.

## License

Apache-2.0.
