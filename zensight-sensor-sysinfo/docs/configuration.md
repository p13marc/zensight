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
| `artifacts` | all disabled | On-demand artifact limits (report + snapshot) for the `@rpc/sysinfo/artifact/{request,cancel}` procedures; every kind off by default. |

## `sysinfo`

> There is no `key_prefix` knob — it was retired in #465. The producer chunk
> (`sysinfo`) is a constant of this crate and the origin is derived from the host's
> machine-id, so keys land under `zensight/v1/<origin>/…/sysinfo/…` with nothing to
> configure. A `key_prefix:` line left over from 0.7.0 is **silently ignored**.

| Key | Default | Meaning |
|-----|---------|---------|
| `source` | `auto` | `source` field in telemetry payloads (not part of the key); `auto` resolves the local hostname (falls back to `unknown`). |
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
| `process_query` | true | serve `@rpc/sysinfo/processes` |
| `temperatures` | **false** | hwmon temperatures (Linux) |
| `tcp_states` | **false** | `/proc/net/tcp` state counts (Linux) |
| `processes` | **false** | top-N process aggregates (can be heavy) |
| `top_processes` | `10` | how many top processes to report when `processes` is on |
| `cgroups` | **false** | cgroup-v2 container-saturation metrics (Linux) |
| `cgroup_paths` | `[]` | extra cgroup-v2 paths to monitor (with `cgroups`) |
| `power` | **false** | RAPL/fan/battery/entropy depth (Linux) |
| `ebpf` | **false** | opt-in eBPF saturation histograms (see below) |

## `sysinfo.network` / `sysinfo.disk` / `sysinfo.sensors` filters

`network`: `include` (empty = all), `exclude`, `exclude_loopback` (default
true), `exclude_virtual` (default false; matches `docker`/`veth`/`br-`/`virbr`/
`vnet` prefixes).

`disk`: `include` (empty = all), `exclude`, `exclude_pseudo` (default true;
drops tmpfs/sysfs/proc/cgroup/overlay/squashfs/… filesystems).

`sensors`: `exclude_chips` (empty = all), the hwmon chips
`collect.temperatures` and `collect.power`'s fan walk skip. Names are matched
exactly against `/sys/class/hwmon/hwmon*/name`.

Some boards expose one physical embedded controller through two hwmon drivers,
so every temperature and fan arrives twice. Dell laptops are the common case:
the modern `dell_ddv` (labelled — "CPU Fan", "Ambient", …) alongside the legacy
`dell_smm` (identical readings, unlabelled `temp1`…`temp7`). Naming the
redundant one drops it at the source:

```json5
sensors: { exclude_chips: ["dell_smm"] },
```

Prefer excluding the *unlabelled* twin — but only where the labelled one exists.
Pre-2022 Dells expose `dell_smm` alone, and excluding it there loses all their
fan and EC-temperature data; `scripts/gen-configs.sh` probes for `dell_ddv`
before excluding, rather than hardcoding it.

### hwmon labels are disambiguated, not deduplicated

A chip may label several sensors identically — `dell_ddv` labels three separate
sensors `Ambient`. Since the key is `sensors/{chip}/{label}/…`, those would
collapse onto one key and the last sample of each tick would silently win. When
a `(chip, label)` pair repeats, every member gains its hwmon input number
(`Ambient_temp4`, `Ambient_temp6`, `Ambient_temp7`); labels unique within their
chip keep their clean name.

### 0 RPM is a reading, not a gap

A fan reporting 0 is published. Laptops genuinely stop their fans at idle, so
dropping the sample would leave a hole in the series and make "fan idle"
indistinguishable from "fan dead". Desktop Super-I/O boards do report 0 on an
unconnected header — put a chip that invents phantom fans in `exclude_chips`.

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

Applies to `@rpc/sysinfo/processes` command lines (#302):

| Key | Default | Meaning |
|-----|---------|---------|
| `scrub_args` | `true` | replace secret-looking argv values before publish |
| `custom_sensitive_words` | `[]` | extra sensitive argv keys (`*` globs allowed, e.g. `"*_token"`) |
| `strip_proc_arguments` | `false` | publish no arguments at all (cmdline stays empty) |

## eBPF feature (#99)

`collect.ebpf: true` serves the `@rpc/sysinfo/latency` histograms (runqlat +
biolatency) and nothing else — they are never streamed onto the bus. It is a
**no-op** unless *all* of the following hold:

| Requirement | Why |
|-------------|-----|
| built `--features ebpf` | the kernel programs are otherwise an empty host stub |
| `CAP_BPF` + `CAP_PERFMON` | loading a tracing program (kernel ≥ 5.8). **Not** `CAP_NET_ADMIN` — that gates *networking* program types, which this does not use |
| `CAP_DAC_READ_SEARCH` | aya resolves a tracepoint by reading `<tracefs>/events/<cat>/<name>/id` from userspace, and `/sys/kernel/tracing` is mode `0700 root:root`. Without it every attach fails `EACCES` **even with `CAP_BPF`**. It is a broad grant (read any file on the host) |
| not a rootless container | `bpf_capable()` is checked against the **initial** user namespace, so a file capability set inside a rootless userns is void and every load returns `EPERM`, whatever `setcap` says |

Any of these missing → one warning, `available: false`, unprivileged baseline
unchanged. The queryable is declared **regardless** of feature or config, so the
GUI can tell "not built with eBPF" from "no sensor answered".

Build:

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
cargo install bpf-linker
cargo build -p zensight-sensor-sysinfo --release --features ebpf
```

`just build` does this for you when it detects the toolchain (`ebpf := "auto"`;
force with `just ebpf=1 …`, disable with `ebpf=0`), and `just caps` grants the
capabilities above. The feature stays out of the default `cargo build
--workspace` / stable CI.

### Tracepoint offsets are hand-maintained

The kernel programs read tracepoint fields at hardcoded byte offsets, which are
**kernel-version dependent**. Getting one wrong raises no error — it reads the
wrong field and the histograms fill with plausible nonsense. Two things guard
that:

* `zensight-sensor-sysinfo-ebpf-common` declares the offsets and its
  `btf_offsets_match_this_kernel` test checks each against
  `/sys/kernel/btf/vmlinux` on the running kernel. BTF member offsets come from
  the same `offsetof` the tracepoint `format` files are generated from, and BTF
  is world-readable where tracefs is `0700` — so it validates unprivileged and
  in CI. Validated on 7.1.3-200.fc44 (2026-07-16).
* Both histograms are **self-validating**: each joins a key written by one
  tracepoint against a key read by another, so a bad offset yields an *empty*
  histogram rather than a wrong one. A non-empty histogram is evidence the
  offsets are right.

CO-RE is not an option here: its field relocations come from clang's
`__builtin_preserve_access_index`, which rustc/bpf-linker do not emit. The
portable fix is `aya::EbpfLoader::set_global` offset injection at load time.

Sanity-check the numbers against independent kernel counters: biolatency's total
count should track `/proc/diskstats` completed-I/O deltas (drive it with `dd
if=<file> of=/dev/null bs=1M iflag=direct; sync`), and runqlat's p50 should climb
from single-digit µs into ms as `/proc/pressure/cpu`'s `some avg10` rises. Note
runqlat only measures wakeup→switch; bcc's also stamps preempted-but-runnable
tasks, so our tail reads lower than `runqlat.py`'s under heavy load.
