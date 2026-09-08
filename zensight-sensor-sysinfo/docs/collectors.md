# sysinfo — collectors, saturation model, and alerting

The sensor is organized around the **USE method** (Utilization, Saturation,
Errors): every poll tick it gathers a set of independently config-gated
collectors, derives a single host saturation score, and grades the collected
data against threshold alert rules. The cross-platform base uses the `sysinfo`
crate; the deeper saturation/error collectors are Linux-only (`src/linux.rs`)
and skip gracefully when a `/proc`/`/sys` file is absent.

## Rates are measured, not assumed (#1069)

Every derived rate here — `network/*/rx_rate` and `tx_rate`, `disk/*/read_rate`,
`write_rate`, `*_iops`, `util_percent`, `queue_depth`, `power/rapl/*/watts`, the
saturation score's swap-in rate, and the alert evaluator's tick — divides by the
seconds that **elapsed** between the two collection passes, not by
`poll_interval_secs`.

The loop runs `collect_and_publish().await` and *then* sleeps the interval, so
the true period is `interval + collection_time`. Under load a 5 s tick takes
12 s: rates read 2.4× the truth, and `util_percent` read 240 % — clamped to a
flat 100, so a disk at 40 % charted as saturated. The sensor was already
measuring the error and publishing it as `last_poll_duration_ms` without using
it.

The counter deltas go through `zensight_sensor_core::rate::CounterTracker`, so a
counter that goes backwards (a reset) costs one reading rather than producing a
spike.

## Collectors

### Utilization (cross-platform, default on)

- **cpu** — global + per-core usage, frequency.
- **memory** — RAM/swap totals, used/available, usage percent.
- **disk** — per-mount space totals, used/available, usage percent.
- **network** — per-interface byte/packet/error counters plus derived
  rx/tx rates.
- **system** — uptime, load averages, boot time.

### Utilization/errors depth (Linux, default on)

- **cpu_times** — `/proc/stat` breakdown (user/nice/system/idle/iowait/irq/
  softirq/steal).
- **disk_io** — whole block devices from `/proc/diskstats` (partitions dropped,
  `dm-*` kept — see `sysinfo.disk_io` in
  [configuration](configuration.md)): read/write bytes, ops, IOPS, plus the
  saturation-flavored `util_percent` and `queue_depth`.
- **net_dev_extended** — richer `/proc/net/dev` counters (drops, fifo, frame,
  collisions, carrier) that the base `sysinfo` counters omit.
- **memory composition** — cached/buffers/slab/dirty/writeback.

### Saturation signals (Linux, default on)

- **pressure (PSI)** — `/proc/pressure/{cpu,memory,io}`; the #1 saturation
  signal in the USE method (Linux 4.20+ with `CONFIG_PSI`).
- **vmstat** — the saturation allowlist (`oom_kill`, `pgmajfault`, `pswpin/out`,
  page faults, paging) plus `/proc/stat` derivatives (context switches, forks,
  run-queue depth via `procs_running`/`procs_blocked`).
- **schedstat** — per-CPU scheduler run-delay (`/proc/schedstat`), the canonical
  CPU-saturation signal (#98).
- **softnet** — NIC→kernel backlog drops / time-squeezes
  (`/proc/net/softnet_stat`, #98).
- **fd_inode** — file-descriptor table occupancy (`/proc/sys/fs/file-nr`) and
  per-mount inode ceilings (`statvfs()`); cheap metrics that catch silent
  table-exhaustion outages.

### Error signals (Linux, default on)

- **netstat** — TCP retransmits, listen-overflow, socket occupancy from
  `/proc/net/{snmp,netstat,sockstat}` (#98).
- **conntrack** — nf_conntrack table fill from `/proc/sys/net/netfilter/
  nf_conntrack_{count,max}`; absent when the module isn't loaded (#98).
- **edac** — ECC memory errors from `/sys/devices/system/edac/mc/mc*/
  {ce_count,ue_count}`; no ECC hardware → emits nothing (#98).
- **mdadm** — software-RAID degraded/failed state from `/proc/mdstat`; no md
  arrays → emits nothing (#98).

### Opt-in (default off)

- **processes** — top-N by CPU/memory aggregates streamed; the per-pid firehose
  is served on demand (see [telemetry.md](telemetry.md)).
- **temperatures** — hwmon sensor readings (also provides the critical trip
  points the thermal alert needs).
- **tcp_states** — `/proc/net/tcp` connection-state counts.
- **cgroups** — cgroup-v2 container-saturation metrics (CPU throttling, memory
  limit/OOM, per-cgroup pressure) for the sensor's own cgroup plus any
  `cgroup_paths`.
- **power** — RAPL energy→watts, hwmon fan RPM, battery capacity/status, kernel
  entropy pool.
- **smart** — drive SMART health (#823): NVMe wear/spare/media-errors via the
  admin health log (CAP_SYS_ADMIN), the three classic ATA attributes via SG_IO
  (CAP_SYS_RAWIO). mdadm reports a drive *after* it left the array; this asks
  the drives themselves. Ioctls run every 60 s off-thread; missing devices or
  permission skip silently, per arm. NVMe temperature deliberately rides the
  existing `temperatures` collector (kernel nvme hwmon), not this one.
- **ebpf** — `runqlat` + `biolatency` histograms on `@rpc/sysinfo/latency`;
  opt-in build only (see [configuration.md](configuration.md)).

## Saturation score model

Gated by `collect.saturation_score` (default on). Implemented in
`src/saturation.rs` as a pure, total function: it never panics and always
returns a finite `0..100`. The score is a weighted blend of normalized USE
saturation signals; a **missing input contributes 0** (not-saturated) but its
weight still counts toward the denominator, so the score degrades gracefully as
inputs drop out. Raising any single signal can only raise the score (monotonic).

Default input weights (`sysinfo.saturation.weights`, need not sum to 1.0 — the
score renormalizes by their total):

| Input | Signal | Default weight |
|-------|--------|----------------|
| `psi_cpu` | PSI cpu `some/avg10` (%) | 0.19 |
| `psi_memory` | PSI memory `some/avg10` (%) | 0.24 |
| `psi_io` | PSI io `some/avg10` (%) | 0.16 |
| `run_queue` | `procs_running / nCPU` (1.0 = one runnable task per CPU) | 0.08 |
| `swap_in` | swap-in pages/s normalized by `swap_in_ref_pages_per_sec` | 0.12 |
| `disk_util` | busiest block device `%util` | 0.16 |
| `fd` | FD-table occupancy (%) | 0.05 |

`swap_in_ref_pages_per_sec` (default 1000 pages/s ≈ 4 MiB/s) is the swap-in rate
that normalizes to a saturation fraction of 1.0.

### Health-state bands

`system/health_state` is derived from the score (`sysinfo.saturation.warn` /
`crit`):

| State | Condition |
|-------|-----------|
| `ok` | score < `warn` (default 50) |
| `warn` | `warn` ≤ score < `crit` (default 80) |
| `crit` | score ≥ `crit` (default 80) |

## Threshold alerting

Gated by `sysinfo.alerts.enabled` (default on). Each poll tick the
already-collected saturation data is graded against the rule thresholds and
firing/resolved alerts publish on `zensight/v1/<origin>/state/sysinfo/alert/*`.
A firing alert
is only published after the violation persists for `for_secs` (`0` = fire on the
first violation).

| Rule | Fires on | Default | Requires |
|------|----------|---------|----------|
| `oom` | new OOM kills since last poll (`memory/oom_kills_total` delta > 0) — always Critical | on | `collect.vmstat` |
| `pressure` | PSI `some/avg10` per resource ≥ warn/critical | on; cpu 40/70, memory 10/30, io 40/70 | `collect.pressure` |
| `disk` | per-mount space usage ≥ warn/critical % | on; 90 / 95 | `collect.disk` |
| `inode` | per-mount inode usage ≥ warn/critical % | on; 90 / 95 | `collect.fd_inode` |
| `disk_fill_rate` | projected time-to-full ≤ warn/critical hours, least-squares fit over `window_secs` | on; 24h / 4h, 30-min window | `collect.disk` |
| `fd` | FD-table occupancy ≥ warn % (Warning only) | on; 80 | `collect.fd_inode` |
| `thermal` | temp ≥ `fraction` × critical trip point (Critical) | **off**; fraction 0.9 | `collect.temperatures` |
| `swap` | `pswpin + pswpout` ≥ `warn_pages_per_sec` (Warning) | on; 1000 pages/s | `collect.vmstat` |

`thermal` is off by default because it needs `collect.temperatures` for the
critical trip points — enable both together. Memory PSI is graded harder than
CPU/IO by design.

### Per-mount overrides (#822)

One global threshold pair cannot fit both a build-scratch volume that lives at
85% by design and a root disk that must never get there. `disk`, `inode` and
`disk_fill_rate` each take a `mounts` list overriding their thresholds per
mount path:

```json5
disk: {
  enabled: true, warn_percent: 90, critical_percent: 95,
  mounts: [
    { path: "/",        warn_percent: 75, critical_percent: 85 },
    { path: "/srv/*",   warn_percent: 92 },   // critical falls back to 95
  ],
}
```

Paths are exact or glob (`glob` crate semantics: `*` does not cross `/`).
**First match wins** — order specific entries before broad globs. Each omitted
field falls back to the rule-level value independently. Invalid patterns are
rejected at startup by config validation. The resolved warn value feeds the
key-affecting `threshold` label, so editing a mount's override retires the old
alert key and raises a fresh one — the same lifecycle as editing the global
threshold.

### `disk_fill_rate` (#822)

A percentage is a lagging indicator: "this filesystem will be full in under 4
hours at the current rate" is actionable at 40% occupancy and useless at 95%.
The rule keeps a per-mount `used_bytes` history, fits a least-squares trend
over `window_secs` (default 30 min), and grades the projected hours-to-full
against `warn_hours_to_full` / `critical_hours_to_full` (default 24h / 4h,
per-mount overridable via its own `mounts` list).

Deliberate silences, all reading as *not asked* rather than "fine": fewer than
`min_samples` samples or less than `min_window_secs` of history; a flat or
shrinking disk; and the ticks after a **reclaim** (a drop of >5% of the
filesystem between two samples — a sweep or rotation invalidates the trend, so
the history resets rather than chasing it). The projection and the fill rate
live in the alert summary; the alert key stays stable as the projection moves.
