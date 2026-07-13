# sysinfo — telemetry reference

All telemetry is published as `zensight/@v1/<origin>/telemetry/sysinfo/<metric>`,
where `<origin>` is the host's `h-<12hex>` id — the key carries no source chunk;
the payload `TelemetryPoint` still carries `source` (the resolved hostname with
`source: "auto"`, or the configured id).
Every family is gated by a `collect.*` flag (see
[configuration.md](configuration.md)); the families marked **default off** are
opt-in. Linux-only families degrade gracefully — an absent `/proc`/`/sys` file
is skipped, never emitted as a zero. Per-mount / per-interface / per-device keys
are sanitized for the key expression (e.g. `/` → `_`, the root mount → `root`)
and carry the original name back in a label.

## Metric families

| Family | `collect` flag | Example keys |
|--------|----------------|--------------|
| system | `system` | `system/uptime`, `system/load` (label `period`), `system/boot_time` |
| cpu | `cpu` | `cpu/usage`, `cpu/<n>/usage`, `cpu/<n>/frequency` |
| cpu times (Linux) | `cpu_times` | `cpu/times/{user,nice,system,idle,iowait,irq,softirq,steal}`, `cpu<n>/times/*` |
| memory | `memory` | `memory/{total,used,available,usage_percent,swap_total,swap_used,swap_percent}` |
| memory composition (Linux) | `memory` | `memory/{cached,buffers,slab,dirty,writeback}` |
| disk | `disk` | `disk/<mount>/{total,used,available,usage_percent}` |
| disk I/O (Linux) | `disk_io` | `disk/<dev>/io/{read_bytes,write_bytes,read_ops,write_ops,time_ms,read_rate,write_rate,read_iops,write_iops}`, plus saturation `disk/<dev>/io/{util_percent,queue_depth}` |
| network | `network` | `network/<iface>/{rx_bytes,tx_bytes,rx_packets,tx_packets,rx_errors,tx_errors,rx_rate,tx_rate}` |
| network extended (Linux) | `net_dev_extended` | `network/<iface>/{rx_dropped,rx_fifo,rx_frame,multicast,tx_dropped,tx_fifo,tx_colls,tx_carrier}` |
| pressure / PSI (Linux) | `pressure` | `pressure/<cpu\|memory\|io>/<some\|full>_{avg10,avg60,avg300,total_us}` |
| vmstat (Linux) | `vmstat` | `memory/{oom_kills_total,page_faults_major_total,page_faults_total,paging_in_total,paging_out_total,pgpgin_total,pgpgout_total}` |
| kernel derivatives (Linux) | `vmstat` | `system/{context_switches_total,forks_total,procs_running,procs_blocked}` |
| fd / inode ceilings (Linux) | `fd_inode` | `system/file_descriptors_{used,max,used_percent}`, `disk/<mount>/{inodes_total,inodes_used,inodes_free,inode_used_percent}` |
| processes | `processes` **(default off)** | `system/{processes_total,processes_zombie}`, `process/<rank>/{cpu,memory}` |
| temperatures (Linux) | `temperatures` **(default off)** | `sensors/<chip>/<label>/{temp,critical,max}` |
| tcp states (Linux) | `tcp_states` **(default off)** | `tcp/<state>`, `tcp/total` |
| cgroup-v2 (Linux) | `cgroups` **(default off)** | `cgroup/cpu/{nr_throttled,throttled_usec}`, `cgroup/memory/{current,max,used_percent,oom_kills_total,oom_total}`, `cgroup/<res>/pressure/<scope>_{avg10,total_us}` |
| thermal / power (Linux) | `power` **(default off)** | `power/rapl/<zone>/watts`, `sensors/<chip>/<fan>/rpm`, `battery/<name>/{capacity,status}`, `system/entropy_avail` |

Additional Linux USE error/saturation collectors (all default on, all gated by
their own flag — see [collectors.md](collectors.md)): `netstat` (TCP
retransmits / listen-overflow / socket occupancy), `softnet` (backlog drops /
time-squeezes), `schedstat` (per-CPU scheduler run-delay), `conntrack`
(nf_conntrack table fill), `edac` (ECC memory errors), `mdadm` (software-RAID
degraded/failed state).

## Derived saturation score

Gated by `collect.saturation_score` (default on). Each tick the sensor emits:

- `system/saturation_score` — a `0..100` host saturation score blended from the
  already-collected USE saturation signals.
- `system/health_state` — a coarse `ok` / `warn` / `crit` band derived from the
  score (see [collectors.md](collectors.md) for the model and thresholds).

## On-demand queries (`@rpc/sysinfo/<topic>`)

Served on request rather than streamed, as read procedures (GETs) on the `@rpc`
plane: `zensight/@v1/<origin>/@rpc/sysinfo/<topic>`. A fleet-wide caller selects
`zensight/@v1/*/@rpc/sysinfo/<topic>` with query target `All`; the sensor also
serves `@rpc/sysinfo/introspect`, returning its registry slice.

- **`processes?sort=cpu|mem|io&top=N`** (`collect.process_query`, default on) —
  the per-pid firehose, returned as `Vec<ProcessRecord>`. Each record carries
  identity/context fields (#302): `cmdline`, `exe`, `ppid`, `cgroup` (v2 path —
  the join key to a systemd unit's `control_group`), `start_time` (`/proc/<pid>/
  stat` field-22 ticks — the `(pid, start_time)` identity pair used fleet-wide),
  and `user`. The command line is **scrubbed of secret-looking argv values**
  before publish and byte-capped; tune via `processes.scrub_args` (default
  `true`), `processes.custom_sensitive_words`, and `processes.strip_proc_arguments`.
- **`latency`** (`collect.ebpf`, **default off**, opt-in build — #99) — a
  `LatencyReport`: `runqlat` (scheduler run-queue latency) and `biolatency`
  (block-I/O latency) as log2 histograms with derived p50/p95/p99 + max. These
  are the saturation *tails* that `/proc` 5s averages cannot see. Reply shape:
  `{ available, window_secs, runqlat: {unit, buckets:[{le_us,count}], total,
  p50_us, p95_us, p99_us, max_us}, biolatency: {...} }`. Off / missing caps /
  unsupported kernel → `available:false` with the unprivileged baseline
  unchanged.

## Alerts

Threshold alerts (`sysinfo.alerts.*`) are published on the standard alert
family `zensight/@v1/<origin>/state/sysinfo/alert/<alert_key>` as a
firing → resolved → Delete-tombstone lifecycle (`<alert_key>` = 16-hex FNV-1a
of `rule + labels`), same as every other sensor (the GUI and exporters pick
them up with no extra wiring). Rules: `oom`, `pressure` (PSI), `disk`, `inode`,
`fd`, `thermal`, `swap` — see [collectors.md](collectors.md) for the grading
logic and [configuration.md](configuration.md) for the thresholds.

## Control-plane keys

Standard sensor state documents are published alongside telemetry (the health
doc absorbs the retired `@/status` running flag; free-form metadata rides the
registration doc):

```
zensight/@v1/<origin>/state/sysinfo/health             # sensor health document
zensight/@v1/<origin>/state/sysinfo/alive              # liveliness token (presence)
zensight/@v1/<origin>/state/sysinfo/errors             # rolling error window
zensight/@v1/<origin>/state/sysinfo/alert/<alert_key>  # threshold alerts
zensight/@v1/<origin>/state/sysinfo/sensor             # SensorInfo registration
zensight/@v1/<origin>/state/sysinfo/evidence/self      # self-identity claim
```

Telemetry selectors never reach the state class or the `@rpc` plane: narrow
with `zensight/@v1/*/telemetry/sysinfo/**` (all sysinfo telemetry, fleet-wide)
or `zensight/@v1/*/state/*/alert/*` (all alerts). The legacy `zensight/**`
firehose matches **nothing** v1 — the verbatim `@v1` chunk blocks `**`. See
[../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the full contract.
