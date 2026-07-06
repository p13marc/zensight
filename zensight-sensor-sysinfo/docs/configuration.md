# sysinfo — configuration

JSON5, loaded with `--config`. Top-level blocks: `zenoh`, `sysinfo`, `logging`,
and an optional `artifacts` block. See `configs/sysinfo.json5` for a fully
commented example. Validation rejects `poll_interval_secs == 0` and a `collect`
block with every metric type disabled.

## Top level

| Key | Default | Meaning |
|-----|---------|---------|
| `zenoh` | — | Zenoh connection (`mode` = `client`/`peer`/`router`, `connect`, `listen`). |
| `sysinfo` | — | Sensor settings (below). |
| `logging.level` | `info` | `trace`/`debug`/`info`/`warn`/`error`. |
| `artifacts` | all disabled | On-demand `@/artifact` channel limits (report + snapshot); every kind off by default. |

## `sysinfo`

| Key | Default | Meaning |
|-----|---------|---------|
| `key_prefix` | `zensight/sysinfo` | Key-expression prefix. |
| `source` | `auto` | Source id in keys; `auto` resolves the local hostname (falls back to `unknown`). |
| `poll_interval_secs` | `5` | Collection interval (must be > 0). |
| `collect` | see below | Which metric families to gather. |
| `network` | see below | Interface filters. |
| `disk` | see below | Mount filters. |
| `alerts` | see below | Threshold alerting. |
| `saturation` | see below | Saturation-score blend + health bands. |
| `processes` | see below | Process-explorer privacy policy. |

## `sysinfo.collect`

Each flag gates one metric family. Non-Linux hosts silently skip the Linux-only
families regardless of the flag.

| Flag | Default | Family |
|------|---------|--------|
| `cpu` | true | CPU usage/frequency |
| `cpu_times` | true | `/proc/stat` CPU-time breakdown (Linux) |
| `memory` | true | RAM/swap + composition |
| `disk` | true | per-mount space |
| `disk_io` | true | `/proc/diskstats` I/O + util/queue (Linux) |
| `network` | true | per-interface counters |
| `net_dev_extended` | true | extended `/proc/net/dev` counters (Linux) |
| `system` | true | uptime, load |
| `pressure` | true | PSI (Linux) |
| `vmstat` | true | vmstat allowlist + `/proc/stat` derivatives (Linux) |
| `fd_inode` | true | FD + inode ceilings (Linux) |
| `netstat` | true | TCP retransmit / listen-overflow / socket occupancy (Linux) |
| `softnet` | true | softnet backlog drops / time-squeezes (Linux) |
| `schedstat` | true | per-CPU scheduler run-delay (Linux) |
| `conntrack` | true | nf_conntrack table fill (Linux) |
| `edac` | true | ECC memory errors (Linux) |
| `mdadm` | true | software-RAID state (Linux) |
| `saturation_score` | true | derived `system/saturation_score` + `health_state` |
| `process_query` | true | serve `@/query/processes` |
| `temperatures` | **false** | hwmon temperatures (Linux) |
| `tcp_states` | **false** | `/proc/net/tcp` state counts (Linux) |
| `processes` | **false** | top-N process aggregates (can be heavy) |
| `top_processes` | `10` | how many top processes to report when `processes` is on |
| `cgroups` | **false** | cgroup-v2 container-saturation metrics (Linux) |
| `cgroup_paths` | `[]` | extra cgroup-v2 paths to monitor (with `cgroups`) |
| `power` | **false** | RAPL/fan/battery/entropy depth (Linux) |
| `ebpf` | **false** | opt-in eBPF saturation histograms (see below) |

## `sysinfo.network` / `sysinfo.disk` filters

`network`: `include` (empty = all), `exclude`, `exclude_loopback` (default
true), `exclude_virtual` (default false; matches `docker`/`veth`/`br-`/`virbr`/
`vnet` prefixes).

`disk`: `include` (empty = all), `exclude`, `exclude_pseudo` (default true;
drops tmpfs/sysfs/proc/cgroup/overlay/squashfs/… filesystems).

## `sysinfo.alerts`

`enabled` (default true) and `for_secs` (debounce, default 0) plus per-rule
sub-blocks. Defaults, grading logic, and the `collect.*` each rule depends on
are documented in [collectors.md](collectors.md#threshold-alerting).

| Rule block | Default | Key fields |
|------------|---------|-----------|
| `oom` | enabled | `enabled` |
| `pressure` | enabled | `cpu_warn` 40 / `cpu_critical` 70, `memory_warn` 10 / `memory_critical` 30, `io_warn` 40 / `io_critical` 70 |
| `disk` | enabled | `warn_percent` 90 / `critical_percent` 95 |
| `inode` | enabled | `warn_percent` 90 / `critical_percent` 95 |
| `fd` | enabled | `warn_percent` 80 |
| `thermal` | **disabled** | `fraction` 0.9 (needs `collect.temperatures`) |
| `swap` | enabled | `warn_pages_per_sec` 1000 |

## `sysinfo.saturation`

Tunes the score blend + health bands (only meaningful with
`collect.saturation_score`):

| Key | Default | Meaning |
|-----|---------|---------|
| `weights.*` | see [collectors.md](collectors.md#saturation-score-model) | per-input blend weights |
| `swap_in_ref_pages_per_sec` | 1000 | swap-in rate normalizing to saturation 1.0 |
| `warn` | 50 | score at/above which `health_state` = `warn` |
| `crit` | 80 | score at/above which `health_state` = `crit` |

## `sysinfo.processes` (privacy)

Applies to `@/query/processes` command lines (#302):

| Key | Default | Meaning |
|-----|---------|---------|
| `scrub_args` | `true` | replace secret-looking argv values before publish |
| `custom_sensitive_words` | `[]` | extra sensitive argv keys (`*` globs allowed, e.g. `"*_token"`) |
| `strip_proc_arguments` | `false` | publish no arguments at all (cmdline stays empty) |

## eBPF feature (#99)

`collect.ebpf: true` is a **no-op** unless the binary was built with
`--features ebpf` **and** the process holds `CAP_BPF` + `CAP_PERFMON` (kernel
≥ 5.8). It serves the `@/query/latency` histograms only — never streamed onto
the bus. Off / missing caps / unsupported kernel → one warning, `available:
false`, and the unprivileged baseline is unchanged.

Build:

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
cargo install bpf-linker
cargo build -p zensight-sensor-sysinfo --release --features ebpf
```

The feature is intentionally out of the default `cargo build --workspace` /
stable CI (the eBPF program crate compiles to an empty host stub off the `bpf`
target). See the commented `AmbientCapabilities` block in
`packaging/systemd/zensight-sensor-sysinfo.service` for the runtime caps.
