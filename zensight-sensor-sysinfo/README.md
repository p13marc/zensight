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
  published on `zensight/v1/<origin>/state/sysinfo/alert/*`.
- **Process explorer** — a per-pid firehose served on demand at
  `@rpc/sysinfo/processes` (never streamed), with secret-scrubbed command lines (#302).
- **GPU, opt-in** (#954) — inventory on `state/sysinfo/gpu/{card}` plus
  utilisation, VRAM, temperature, power, fan and clock, read from the kernel's
  DRM sysfs with **no vendor library**. What that costs is stated rather than
  hidden: amdgpu publishes a busy percentage and Intel does not, so
  **utilisation is absent on i915/xe** — a zero would say the GPU is idle.
  Passthrough and vGPU both surface as a DRM card *inside* the guest, so a
  guest running this sensor reports its own GPU with no host-side work.
- **Clock discipline, opt-in** (#959) — `state/sysinfo/timesync` from
  `chronyc -c tracking`, falling back to `timedatectl show`. **Absent when no
  time daemon answers**, never a zero offset: a zero is what a perfectly
  disciplined clock looks like. This is the half `probe`'s `ntp` check cannot
  see — that one measures a server against *this* host's clock.
- **Optional eBPF saturation histograms** (#99) — `runqlat` + `biolatency` log2
  histograms on `@rpc/sysinfo/latency`; opt-in build (`--features ebpf`) and off
  by default.

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
