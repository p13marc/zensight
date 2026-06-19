# Plan v3‑01 — sysinfo sensor

Today the sensor publishes ~68 metrics: CPU (global/per‑core/freq + `/proc/stat`
times), memory + swap, disk space + `/proc/diskstats` IO, network counters +
rates, hwmon temps, `/proc/net/tcp` state counts, top‑N processes, uptime/load/
boot. Strong on **utilization**; thin on **saturation/errors** — exactly the
under‑collected, high‑signal USE dimension.

> Sources verified against the sensor (`collector.rs`, `linux.rs`) and the
> `procfs`/`sysinfo` crates it already depends on. Research: USE method
> (brendangregg.com/usemethod.html), node_exporter collectors, kernel PSI docs.

---

## A. Pressure Stall Information (PSI) — the #1 saturation signal **[Wave 1]**

`/proc/pressure/{cpu,memory,io}` — for each, `some` and (memory/io) `full` lines
with `avg10 avg60 avg300 total=<µs>`. `full` = *all* non‑idle tasks stalled =
true thrashing. **Verified:** procfs 0.17 exposes these as typed structs —
`CpuPressure{some}`, `MemoryPressure{some,full}`, `IoPressure{some,full}`, each a
`PressureRecord{avg10,avg60,avg300,total}` — use them directly (no raw parsing).

| metric | type |
|---|---|
| `pressure/{cpu,memory,io}/some_avg{10,60,300}` | Gauge (%) |
| `pressure/{memory,io}/full_avg{10,60,300}` | Gauge (%) |
| `pressure/<res>/{some,full}_total_us` | Counter |

Config toggle `collect.pressure` (default true; absent file ⇒ skip, kernel <4.20
/ `CONFIG_PSI=n`). **Derive rate from the cumulative `total`**, don't just trust
the rolling averages. **Why:** PSI catches the bursts averaged %util hides — the
single best "is this host starved?" signal. *Live‑verifiable here.*

## B. vmstat saturation allowlist + `/proc/stat` derivatives **[Wave 1]**

Mirror node_exporter's vmstat saturation regex (`^(oom_kill|pgpg|pswp|pg.*fault)`).
**Note (verified):** procfs 0.17 has **no `vmstat`** module → write a small
`/proc/vmstat` parser (a flat `key value` file) into a typed `VmStat { oom_kill,
pgmajfault, pswpin, pswpout, … }` struct (unit‑tested on a fixture). `/proc/stat`
derivatives *are* in the typed `procfs::KernelStats { ctxt, processes,
procs_running, procs_blocked }`.

| metric | type | why |
|---|---|---|
| `memory/oom_kills_total` | Counter | the canonical failure event |
| `memory/page_faults_major_total` (`pgmajfault`) | Counter | working‑set > RAM (saturation) |
| `memory/paging_{in,out}_total` (`pswpin/out`) | Counter | swap thrash |
| `system/context_switches_total` (`ctxt`) | Counter | scheduler thrash |
| `system/forks_total` (`processes`) | Counter | churn / fork‑bomb |
| `system/procs_{running,blocked}` (`/proc/stat`) | Gauge | run‑queue depth / I/O‑blocked |

**Why:** OOM/major‑faults/swap are leading indicators of memory exhaustion; alert
`increase(oom_kills[5m])>0`. *Live‑verifiable.*

## C. Saturation‑ceiling metrics: FD + inode **[Wave 1]**

Cheap metrics that catch silent failures (exhausted table with free disk).

| metric | source | type |
|---|---|---|
| `system/file_descriptors_{used,max}` | `/proc/sys/fs/file-nr` | Gauge |
| `disk/<mount>/inodes_{total,used,free}` | `statfs()` (via `nix`/`rustix`) | Gauge |

Add `disk/<mount>/inode_used_percent`. **Why:** inode/FD exhaustion is a classic
silent outage. *Live‑verifiable.*

## D. NIC drops + richer `/proc/net/dev` **[Wave 1]**

The `sysinfo` network counters lack the **drop** fields. Read `/proc/net/dev`
directly (or `procfs`) for per‑interface `rx_dropped`/`tx_dropped`/`rx_fifo`/
`rx_frame` and publish under `network/<iface>/{rx_dropped,tx_dropped,...}`
(Counter). **Why:** drops = RX/TX buffer overflow = the host‑side saturation
signal that pairs with netlink's ethtool ring stats (Plan 02). *Live‑verifiable.*

## E. cgroup‑v2 (container saturation) **[Wave 2]**

For containerized hosts, the high‑signal fields are **throttling + OOM**, not raw
usage (cAdvisor dropped these on v2 — read the files directly):

| metric | source |
|---|---|
| `cgroup/cpu/{nr_throttled,throttled_usec}` | `/sys/fs/cgroup/<g>/cpu.stat` |
| `cgroup/memory/{current,max}` | `memory.current`/`memory.max` |
| `cgroup/memory/oom_kills_total` | `memory.events` (`oom_kill`) |
| `cgroup/<res>/pressure/*` | per‑cgroup `*.pressure` (PSI) |

Config `collect.cgroups` (default false; opt‑in for container hosts). Scope: the
sensor's own cgroup + optionally a configured list. *Live‑verifiable on a v2 host.*

## F. Per‑process detail (on‑demand, cardinality‑disciplined) **[Wave 2]**

Today only top‑N by CPU stream. Add a **query channel** `@/query/processes?
sort=cpu|mem|io&top=N` returning per‑process `{pid,name,cpu,mem,rss,threads,
state,io_read,io_write,uid}` from `procfs::process` + `/proc/<pid>/io`. Stream
only the small aggregates (`system/processes_{total,zombie}`); serve the firehose
on demand (P2). **Why:** "what's eating the box?" without per‑pid metric cardinality.

## G. Thermal/power depth (feature‑gated) **[Wave 3]**

Beyond current hwmon temps: RAPL energy → power (`/sys/class/powercap/*/energy_uj`
→ Gauge watts via rate), fan speeds (hwmon `fan*_input`), battery
(`/sys/class/power_supply/*`: capacity/status), entropy
(`/proc/sys/kernel/random/entropy_avail`). Default off (hardware‑specific, higher
cardinality).

---

## Testing & sequencing

> Follow [Plan 05](05-architecture-and-conventions.md): typed sample structs
> (`Psi`, `VmStat`, …) → pure `map.rs` → `TelemetryPoint`; small `/proc` reads
> inline on the tick, but the per‑process walk (F) under `spawn_blocking`.

- Pure parsers per source on synthetic `/proc` fixtures (no kernel) — PSI line,
  vmstat allowlist, file‑nr, /proc/net/dev, cgroup stat. Unit‑tested.
- Live (unprivileged): all of A–D + F read real `/proc`/`/sys` in this sandbox —
  verify end‑to‑end with a throwaway subscriber (the proven pattern).
- Each behind a `collect.*` toggle; absent‑file ⇒ graceful skip (no zero spam).
- Wave 1 = A+B+C+D (all live‑verifiable, ~1–2 days). Wave 2 = E+F. Wave 3 = G.
- Wire the new saturation metrics into the metric‑threshold expectation keystone:
  e.g. `pressure/cpu/some_avg10 > 50`, `memory/oom_kills_total` increase, as
  default sentinel expectations / GUI‑authored alerts.
