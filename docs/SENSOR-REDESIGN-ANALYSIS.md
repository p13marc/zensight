# ZenSight — Sensor & Frontend Redesign Analysis

> **Status:** draft for review · **Date:** 2026-06-25 · **Scope:** the four first-wave
> sensors — **sysinfo**, **logs** (syslog + journald), **netlink**, **netring** — and
> their integration in the Iced desktop frontend.
>
> **Method:** five parallel deep-dives, each combining a precise read of the current
> code (with `file:line` references) and internet research into the state of the art
> (node_exporter, OpenTelemetry semconv, the USE/RED methods, eBPF, Drain3, Zeek/
> Suricata/RITA/Corelight, JA4+, Cilium Hubble, Coroot, Netdata, PagerDuty). Backward
> compatibility is treated as **breakable**; redesign is encouraged.
>
> This document is analysis + proposal only — no code has been changed.

---

## 1. Executive summary

ZenSight's four sensors are, individually, strong — several are ahead of a naive
wrapper (the sysinfo Wave-1 saturation work, the netlink sentinel + events-as-Stream
model, the netring cardinality discipline, the journald robustness/drop-accounting).
The platform's weakness is **not the sensors — it's the distance between what they
collect and what the operator can see and act on.** The same handful of structural
gaps recur in every section:

1. **"Collected but never surfaced."** Every sensor ships rich data the frontend
   silently ignores: sysinfo's `@/query/processes` channel has **no UI consumer at
   all**; **5 of 8** netlink query channels and **4 of 9** netring query channels are
   served but never fetched; netring leaves a large set of `flowscope`/`netring`
   detectors and L7 parsers (RITA beaconing, DNS-tunnel primitives, SMB/RDP/Kerberos,
   p0f, ARP/NDP spoofing, JA4H/JA4SSH) **entirely unwired**. The sensors have raced
   ahead of the GUI.

2. **Snapshots, not trends.** Only `Counter`/`Gauge` enter the (otherwise excellent,
   Netdata-style tiered) store; booleans, text, and on-demand tables are
   snapshot-only. Roughly half of each sensor's output has no history or chart.

3. **Alerting is uneven, and there is no incident.** sysinfo raises **zero alerts**
   despite collecting PSI / OOM / thermal / FD-exhaustion data; the others alert but
   there is **no unified incident object** spanning alert ↔ host ↔ metric ↔ flow ↔ log.

4. **One physical host fragments into N protocol-devices** everywhere except the
   topology view (`DeviceId = (protocol, source)`), defeating the project's own
   "one host = sysinfo + netlink + logs + netring" promise on the main surfaces.

5. **The data model is stringly-typed**, and the logs model in particular
   (`metric = <facility>/<severity>`, message-as-value) makes Zenoh storage
   **last-writer-wins per (host, facility, severity)** — so no log history survives at
   the wire level. Nothing is OpenTelemetry-aligned.

6. **eBPF is the strategic frontier** that `/proc`/netlink polling structurally cannot
   reach (tail-latency histograms, connection lifecycle, per-flow/per-process
   attribution) — and the obvious next leap toward the Coroot/Hubble service-map model.

The good news: **most of the highest-ROI work is small** — wiring up channels that
already exist, fixing two latent bugs, and adding alerting/correlation on top of data
already collected. The architectural redesign (a `Host` aggregate, an `Incident`
object, a real service map, an OTel-aligned model) is larger but rests on foundations
that already exist in the codebase (the topology merge logic, the `alert_key` dedup +
timelines, the tiered store).

### Consolidated top-10 (cross-sensor, by ROI)

| # | Change | Area | Effort | Breaks compat |
|---|--------|------|--------|---------------|
| 1 | **Fetch the orphaned query channels** — sysinfo `processes`; netlink `addresses/events/tc/xfrm/nft`; netring `talkers/elephant_flows/dns/http` | all + GUI | S–M | No |
| 2 | **sysinfo alerting** (OOM/PSI/disk/FD/thermal → `@/alerts`) — the one mute sensor | sysinfo | M | No |
| 3 | **`Host` aggregate** — one card per physical host, facets per protocol | frontend | L | GUI-only |
| 4 | **Unified `Incident` object** + timeline + alert↔host↔metric↔flow↔log pivots | frontend | L | No (additive) |
| 5 | **Universal trend layer** — everything chartable; logs→rate series | frontend/store | M | No |
| 6 | **Topology → real service map** — all hosts, netring flow + netlink neighbor (#49) edges, drill-down, per-link health | frontend | M | No |
| 7 | **Logs: Drain3 templating + novelty detection** (noise collapse + unknown-unknowns) | logs | M | No |
| 8 | **netring: Community ID + ATT&CK tags + RITA/DNS-tunnel/NOD detectors + flow-pivot** | netring | S–M | No |
| 9 | **netlink: enrich `tcp_info` (delivery/pacing/bytes_retrans) + bufferbloat/qdisc score** | netlink | S–M | No |
| 10 | **OTel-aligned model + `syslog→logs` rename + decouple log key from facility/severity** | logs/common | L | **Yes** |

---

## 2. Cross-cutting themes

These patterns appear in three or more of the five sections and are best addressed once,
platform-wide, rather than per sensor.

- **T1 — The "fetch-on-drill-in" gap.** The `Fetch<T>` query-channel pattern
  (`specialized/fetch.rs`) is good, but each topic is hand-wired and nothing prefetches.
  Net result: backend channels that exist and are tested are invisible in the UI.
  *Fix once:* a declarative `QueryChannel` registry + `fetch_on_open` policy
  (frontend P4), then point every sensor's orphaned channels at it.

- **T2 — Snapshot vs. trend.** The tiered store (`store.rs`) is a genuine strength but
  only ingests numerics. *Fix once:* a typed `Sample` projection per `TelemetryValue`
  arm + booleans-as-step-series + logs-as-rate series, and a universal "chart this"
  affordance (frontend P3).

- **T3 — Data → decision.** Rich saturation/anomaly data exists but rarely becomes an
  alert, and alerts never become an incident. *Fix once:* sysinfo alerting (parity),
  plus the `Incident` aggregate (frontend P2) that every sensor's alerts roll into.

- **T4 — Host identity.** `(protocol, source)` fragmentation. *Fix once:* the `Host`
  aggregate (frontend P1); the topology `update_from_devices` merge is the template.

- **T5 — Typed contract + OTel alignment.** Stringly-typed metric paths, no per-sensor
  typed sample, ad-hoc keys that don't map to OTel semconv, and a log model that loses
  the stream. *Fix once:* an internal key↔semconv mapping + a typed-sample contract;
  for logs specifically, decouple the key from facility/severity.

- **T6 — Unused dependency capability.** netring/flowscope and netlink/`nlink` (and the
  kernel `tcp_info` struct) already expose far more than is wired. Cheap wins by
  turning on what's already paid for.

- **T7 — eBPF as the opt-in frontier.** Both sysinfo (runqueue/block-I/O latency
  histograms) and netlink (tcplife/connlat/retransmit attribution) want the same
  thing: a `CAP_BPF`-gated, off-by-default eBPF module (`aya`, pure-Rust) for signals
  polling cannot produce. Worth a shared design.

- **T8 — Stale docs / dead code.** `docs/SENSORS.md` + `docs/KEYSPACE.md` are stale for
  sysinfo and netring; `view/overview/syslog.rs` reads a contract the logs sensor no
  longer emits (dead code). Cheap correctness debt to clear alongside the rename.

---

## 3. Proposed roadmap (waves)

Organized so each wave is independently shippable and later waves rest on earlier ones.
Effort: **S** ≈ hours–day, **M** ≈ days, **L** ≈ week+.

### Wave 0 — Quick wins (mostly GUI plumbing + bugfixes; no compat breaks)
- Fetch + render the orphaned query channels (sysinfo `processes`; netlink
  `addresses/events/tc/xfrm/nft`; netring `talkers/elephant_flows/dns/http`). *(S–M)*
- Fix the **liveness protocol bug** dropping netlink/netring device status
  (`app.rs:1791-1800`). *(S)*
- netring **Community ID** on flows+anomalies; **ATT&CK technique** labels. *(S)*
- sysinfo derive **disk `%util`/queue-depth** + **`MemAvailable`-based** memory
  pressure. *(S)*
- netlink enrich **`delivery_rate`/`pacing_rate`/`bytes_retrans`** from `tcp_info`. *(S)*
- Fix stale `docs/SENSORS.md`/`KEYSPACE.md`; retire/rewrite the dead `overview/syslog.rs`. *(S)*

### Wave 1 — Make the data actionable (M)
- **sysinfo alerting** (OOM/PSI/disk/inode/FD/thermal/swap-thrash/conntrack) + a
  derived **host saturation score**.
- **Universal trend layer** (T2): booleans + logs-rate into the store; chart-anything.
- **logs: Drain3 templating + novelty detection** (+ MESSAGE_ID catalog enrichment,
  per-unit error budgets).
- **netring: RITA beaconing + DNS-tunnel + Newly-Observed-Domain detectors**, and a
  **flow drill-down pivot** from the Security view.
- **netlink: bufferbloat/qdisc health score + AQM classification**; **neighbor-adjacency
  topology edges (#49)** + per-link health.

### Wave 2 — Architecture redesign (L; some breaking, all permitted)
- **`Host` aggregate** (T4) — one card per host, per-protocol facets, composite health.
- **Unified `Incident` object** (T3) — grouping, timeline, cross-domain pivots; ATT&CK
  lens in Security.
- **Topology → service map** — all hosts; flow + neighbor edges; edge drill-down + RTT.
- **OTel-aligned model** + **`syslog→logs`** protocol/prefix rename + **decouple the log
  key from facility/severity** (stop losing the stream).
- **Store retention/eviction** + downsample-on-read; **god-struct decomposition**.
- Information-architecture shift: navigate by **host/incident**, protocol becomes a facet.

### Wave 3 — Strategic frontier (L; opt-in)
- **eBPF modules** (T7): sysinfo runqlat/biolatency histograms; netlink
  tcplife/connlat/retransmit-attribution → connection lifecycle + per-process/flow
  attribution (the Coroot/Hubble leap).
- netring **lateral-movement** parsers (SMB/RDP/Kerberos), **JA4H/JA4SSH** (FoxIO
  license decision), **exfil** heuristics, and **PCAP retro-hunt** (needs a packet store).

---

## 4. Backward-compatibility & migration notes

The brief permits breaking changes. Most high-ROI work is **additive** (new metrics,
new channels, new GUI). The genuinely breaking items, and how to stage them:

- **GUI-only (no wire impact):** the `Host` aggregate (P1), god-struct decomposition
  (P9), retention (P8). Safe to do anytime; no sensor or exporter coordination needed.
- **Wire-breaking, do together in Wave 2:**
  - **`syslog → logs` rename** of `Protocol`, the `zensight/syslog/...` key prefix, the
    serde tag, the runner name, and the frontend `Syslog*` types. (The *crate* was
    already renamed; the *wire identity* was deliberately left stable — this finishes it.)
  - **Decouple the log key from `facility/severity`** and stop using the message as a
    Zenoh value (publish per-line under a uid/stream key; reserve `facility/severity`
    for the `logs/*` rollup counters). This fixes the silent last-writer-wins
    stream-loss and is the prerequisite for any log history/search.
  - **OTel-aligned metric keys** (sysinfo `system.*` with `state`/`direction`
    attributes). Coordinate with the prometheus/otel exporters, which currently
    hand-map ad-hoc keys.
- **Coordination surface:** sensors, the two exporters (`zensight-exporter-{prometheus,
  otel}`), the frontend decoder (`subscription.rs::decode_sample`), and `docs/KEYSPACE.md`
  all encode the keyspace. A single "keyspace v2" change touching all of them, landed
  behind one release, is cleaner than incremental drift.
- **Sequencing:** Wave 0 → 1 are non-breaking and deliver most of the visible value;
  do them first to de-risk. Save the breaking keyspace/model changes (Wave 2) for one
  coordinated release.

---

## 5. Per-sensor analysis

The five detailed sections follow verbatim (each is self-contained with its own
current-state map, SOTA research, and prioritized proposals).

---

## Sysinfo sensor

### Part A — Current state (precise map)

**Crate shape.** `zensight-sensor-sysinfo` is a per-host self-monitoring sensor. It collects via the `sysinfo` 0.33 crate for the cross-platform core and `procfs` 0.17 + `rustix` (statvfs) + raw `/sys` walks for the Linux depth (`Cargo.toml`). Polls every `poll_interval_secs` (default 5s, `config.rs:70`), publishes one `TelemetryPoint` per metric as JSON (`main.rs:31` pins `Format::Json` — CBOR is never used here even though the model supports it), key `zensight/sysinfo/<hostname>/<metric>` (`collector.rs:1091`). Hostname auto-detected via the `hostname` crate (`config.rs:288`).

**The collectors and their exact emitted keys** (all under `zensight/sysinfo/<host>/`):

| Collector (`collect.*` flag, default) | Source | Keys (metric template) · value type |
|---|---|---|
| `system` (on) | `sysinfo` | `system/uptime` ctr, `system/boot_time` ctr, `system/load` gauge ×3 (`period=1m/5m/15m` label) — `collector.rs:306-367` |
| `cpu` (on) | `sysinfo` | `cpu/usage` gauge, `cpu/<i>/usage` gauge (`core`,`name` labels), `cpu/<i>/frequency` gauge MHz — `collector.rs:371-417` |
| `cpu_times` (on, Linux) | `/proc/stat` | `cpu/times/{user,nice,system,idle,iowait,irq,softirq,steal}` gauge %, and per-core `cpu<i>/times/…` — `collector.rs:773-874`, delta math `linux.rs:131-186` |
| `memory` (on) | `sysinfo` | `memory/{total,used,available}` ctr, `memory/usage_percent` gauge, `memory/{swap_total,swap_used}` ctr, `memory/swap_percent` gauge — `collector.rs:420-509` |
| `disk` (on) | `sysinfo` | `disk/<mount>/{total,used,available}` ctr, `disk/<mount>/usage_percent` gauge (`mount`,`fs_type`,`name` labels) — `collector.rs:512-585` |
| `disk_io` (on, Linux) | `/proc/diskstats` | `disk/<dev>/io/{read_bytes,write_bytes,read_ops,write_ops,time_ms}` ctr + `…/{read_rate,write_rate}` gauge B/s + `…/{read_iops,write_iops}` gauge — `collector.rs:877-980` |
| `network` (on) | `sysinfo` | `network/<if>/{rx_bytes,tx_bytes,rx_packets,tx_packets,rx_errors,tx_errors}` ctr + `…/{rx_rate,tx_rate}` gauge — `collector.rs:587-698` |
| `net_dev_extended` (on, Linux) | `/proc/net/dev` | `network/<if>/{rx_dropped,rx_fifo,rx_frame,multicast,tx_dropped,tx_fifo,tx_colls,tx_carrier}` ctr — `map.rs:393-415` |
| `pressure` (on, Linux) | `/proc/pressure/*` | `pressure/<cpu\|memory\|io>/<some\|full>_avg{10,60,300}` gauge % + `…_total_us` ctr — `map.rs:105-138` |
| `vmstat` (on, Linux) | `/proc/vmstat` + `/proc/stat` | `memory/{oom_kills_total,page_faults_major_total,page_faults_total,paging_in_total,paging_out_total,pgpgin_total,pgpgout_total}` ctr; `system/{context_switches_total,forks_total}` ctr, `system/{procs_running,procs_blocked}` gauge — `map.rs:194-247` |
| `fd_inode` (on, Linux) | `file-nr` + `statvfs` | `system/file_descriptors_{used,max,used_percent}` gauge; `disk/<mount>/{inodes_total,inodes_used,inodes_free,inode_used_percent}` gauge — `map.rs:276-332` |
| `temperatures` (**off**, Linux) | hwmon | `sensors/<chip>/<label>/{temp,critical,max}` gauge °C — `collector.rs:982-1031` |
| `tcp_states` (**off**, Linux) | `/proc/net/tcp{,6}` | `tcp/<state>` ctr ×11 + `tcp/total` ctr — `collector.rs:1033-1080` |
| `processes` (**off**) | `sysinfo` | `system/processes_{total,zombie}` gauge; `process/<rank>/cpu` gauge + `process/<rank>/memory` ctr for top-N by CPU (`pid`,`name`,`rank` labels) — `collector.rs:700-771` |
| `cgroups` (**off**, Linux) | `/sys/fs/cgroup` v2 | `cgroup/cpu/{nr_throttled,throttled_usec}` ctr, `cgroup/memory/{current,max,used_percent}` gauge, `cgroup/memory/{oom_kills_total,oom_total}` ctr, `cgroup/<res>/pressure/<scope>_avg10` gauge + `…_total_us` ctr — `map.rs:514-577` |
| `power` (**off**, Linux) | powercap/hwmon/sysfs | `power/rapl/<zone>/watts` gauge (rate-derived from energy counter, `map.rs:627-647`), `sensors/<chip>/<fan>/rpm` gauge, `battery/<name>/{capacity,status}` gauge+Text, `system/entropy_avail` gauge — `map.rs:651-696` |

The codebase is well-architected: a clean Plan-05 pipeline of `typed sample → pure map fn (map.rs) → Metric → TelemetryPoint`, with all parsers unit-tested on synthetic fixtures and graceful `Option`-degradation per missing file. The Wave-1 saturation work (PSI, vmstat allowlist, FD/inode ceilings, NIC drops, cgroup-v2, RAPL) is genuinely good and already ahead of a naive sysinfo wrapper.

**On-demand process detail.** A `@/query/processes?sort=cpu|mem|io&top=N` queryable (`query.rs`) returns a bounded `Vec<ProcessRecord>{pid,name,cpu,rss,vsz,threads,state,io_read,io_write,uid}` (`query_detail.rs`), run under `spawn_blocking`, top clamped to 200 (`map.rs:733`). Correctly keeps the per-pid firehose off the bus.

**Frontend.** `view/specialized/sysinfo.rs` renders the host detail: system overview (uptime/load/boot), CPU gauges + per-core grid (scans `cpu/0..128/usage`, `sysinfo.rs:204`), memory/swap bars, disk bars, network rx/tx, and conditional cards for CPU-times, disk-IO, temps, TCP states, top processes, PSI, cgroup, and a "System health" card (FD%, runqueue, ctx-switches, forks, zombies, entropy — `sysinfo.rs:850-877`). `view/overview/sysinfo.rs` does a fleet roll-up (avg CPU/mem, count of hosts >80%). `view/topology/mod.rs:393` ingests `cpu/usage`, `memory/used`/`total`, and network rates to size/tint host nodes.

**Critical gaps in the current integration:**
- **No alerting.** Unlike snmp/syslog/netlink/netring (which use `AlertReporter` → `@/alerts/<key>`), sysinfo emits **zero alerts** (`grep` confirms no `Alert` usage). OOM kills, PSI spikes, near-full disks, thermal criticals, and FD exhaustion are all *published as numbers but never raised* — the host sensor is mute on the very saturation events it now collects.
- **`@/query/processes` has no consumer.** The queryable is declared and tested sensor-side, but **nothing in `zensight/src/` ever calls it** (`grep` for `query/processes`/`ProcessRecord` in the frontend returns nothing). The richest signal — "what's eating the box" — is unreachable from the UI. The streamed `process/<rank>` top-10 is the only process data the UI sees, and only when `processes` is enabled (off by default).
- **Snapshots, not trends.** `DeviceDetailState` keeps 500 samples of history per metric (`device.rs:101`), and `metric_sparkline`/`metric_trend_and_alert` exist, yet only `cpu/usage`, `memory/used`, and PSI `some_avg10` get a sparkline. Disk-IO, network rates, temperatures, throttling, paging — all render as instantaneous text. PSI `avg60` and the `total_us` rate-derivable counters are collected but not visualized as rates.
- **Collected-but-not-shown:** `net_dev_extended` drops/fifo/colls (no card at all), `system/forks_total`, `memory/page_faults_*`, `paging_in/out`, `pgpgin/out`, RAPL watts/fans/battery (no power card), `cpu/<i>/frequency` (only shown inline in the per-core label). `docs/SENSORS.md:92-98` is also stale — it lists `net/<iface>/rx_bytes` (real key is `network/…`) and omits the entire PSI/cgroup/power/vmstat surface.

### Part B — State of the art

**node_exporter** ([README](https://github.com/prometheus/node_exporter/blob/master/README.md), [guide](https://prometheus.io/docs/guides/node-exporter/)) enables by default a superset ZenSight partly matches, but ZenSight is **missing** several default-on collectors: `netstat`/`sockstat` (`/proc/net/netstat`+`sockstat`: TCP retransmits, listen-queue overflows, socket-memory pressure), `softnet` (`/proc/net/softnet_stat`: dropped/squeezed packets — NIC-to-kernel backpressure), `schedstat` (per-CPU scheduler run-delay — the canonical CPU *saturation* signal), `edac` (ECC memory errors — the canonical memory *error* signal), `mdadm` (RAID degraded/failed), `conntrack` (nf_conntrack table fill — a silent firewall outage), and `infiniband`. PSI, `thermal_zone`, `hwmon`, `processes`, `ethtool`, `tcpstat`, and `perf` are disabled-by-default in node_exporter too; `perf`/`slabinfo` need elevated privileges.

**OpenTelemetry host-metrics semconv** ([spec](https://opentelemetry.io/docs/specs/semconv/system/system-metrics/)) defines a stable `system.*` namespace: `system.cpu.{utilization,time,frequency}`, `system.memory.{usage,utilization}` with a `state` attribute (`used/free/cached/buffered`), `system.paging.{usage,operations,faults}`, `system.disk.{io,operations,io_time,operation_time}`, `system.network.{io,packets,errors,dropped,connections}`, `system.filesystem.{usage,utilization}`. ZenSight's keys are ad-hoc (`memory/used` vs `system.memory.usage{state}`); it lacks the `state`-attribute factoring (no `cached`/`buffered`/`slab` breakdown of memory) and a coherent naming contract that would map cleanly through the OTEL exporter.

**The USE method** ([Linux checklist](https://www.brendangregg.com/USEmethod/use-linux.html)) is the right organizing frame and exposes ZenSight's holes precisely. Per resource, the saturation/error signals SOTA expects:
- **CPU**: util = busy% (have); **saturation** = run-queue `r > nCPU` (have `procs_running`, but no per-CPU `schedstat` run-delay); **errors** = CPC/ECC via perf (missing).
- **Memory**: util = used% (have); **saturation** = `si`/`so` swap + `pgscan` reclaim scanning + major-fault rate (have paging/majfault counters as raw totals, not rates); **errors** = EDAC ECC + OOM (have `oom_kill` count, missing EDAC).
- **Network**: util = bytes (have); **saturation** = `/proc/net/dev` drops + `netstat -s` retransmits + softnet squeezes (have netdev drops, **missing** retransmits/softnet); **errors** = errs (have).
- **Storage**: util = `%util` from `io_time` (collected as `time_ms` counter but **not derived to %util**); **saturation** = `avgqu-sz`/`await` (missing — needs `/proc/diskstats` field 11 `weighted_io_time` and in-flight); **errors** = `ioerr_cnt`/SMART (missing).

**eBPF** ([Gregg](https://www.brendangregg.com/blog/2021-07-03/how-to-add-bpf-observability.html), [ebpf.html](https://www.brendangregg.com/ebpf.html)): tools like `runqlat` (scheduler run-queue latency — the *distribution* of CPU saturation, not just queue depth), `biolatency` (block-I/O latency histogram), and `runqlen` are explicitly "low enough overhead to run 24×7" with **zero overhead at rest**. These give histogram-quality saturation signals `/proc` polling fundamentally cannot (polling sees averages over 5s; eBPF sees the tail). The cost: a kernel-version-sensitive dependency (`aya`/`libbpf`) and `CAP_BPF`/`CAP_PERFMON`. For ZenSight, runqueue-latency and block-I/O-latency histograms are the two highest-value eBPF additions; full on-CPU profiling (Parca/Pixie-style) is overkill for a fleet host sensor.

### Part C — Redesign proposal

#### Gap analysis (vs SOTA)

1. **No saturation-to-alert path.** The sensor collects best-in-class saturation data (PSI, OOM, throttling, thermal crit, FD%) and raises nothing. This is the single biggest gap: it is the only sensor in the platform that doesn't participate in `@/alerts`.
2. **Process explorer unreachable.** `@/query/processes` is dead code from the UI's perspective.
3. **Missing USE error/saturation primitives**: `schedstat` run-delay, `netstat`/softnet retransmits & squeezes, conntrack fill, EDAC ECC, mdadm, disk `%util`/queue-depth derivation.
4. **Counters shipped raw, not rated.** Paging, faults, ctx-switches, forks, OOM, throttle-usec are monotonic counters the UI shows as ever-growing integers; the actionable signal is their *rate*. The exporter/store can rate them, but the UI doesn't.
5. **No memory composition.** No `cached`/`buffered`/`slab`/`available`-vs-`free` breakdown; "used%" over-reports pressure (page cache counts as used).
6. **Snapshot-only UI** for everything except 3 metrics.
7. **No coherent metric contract** mapping to OTEL semconv.

#### Concrete proposals

| # | Proposal | Rationale | Effort | Breaks compat |
|---|---|---|---|---|
| P1 | **Add `AlertReporter` to sysinfo** with threshold rules: OOM-kill delta>0, PSI `some_avg10` over configurable threshold (cpu/mem/io), disk usage% & inode% > hi-watermark, FD% > 80, thermal `temp ≥ 0.9×crit`, swap thrash (`pswpin/out` rate), cgroup throttling rate, conntrack fill. Emit on `@/alerts/<key>` like the other sensors. | The collected saturation data is useless if no one is paged. Brings sysinfo to parity with snmp/netlink. | **M** | No (additive) |
| P2 | **Wire the process explorer**: add a `Message::QueryProcesses{host,sort}` → call `@/query/processes` → a sortable process table panel with a CPU/Mem/IO toggle and drill-in. | The richest signal is already produced and tested; only the UI consumer is missing. Highest ROI for least new code. | **S–M** | No |
| P3 | **USE-completeness collectors** (new `collect.*` flags, all Linux, default-on where cheap): `netstat`/`sockstat` (TCP retransmits, listen-overflows, sockets-in-use, TCP mem), `softnet` (dropped/squeezed), `schedstat` (per-CPU run-delay → derive ns/s), `conntrack` (count/max → fill%), `edac` (ce/ue counts), `mdadm` (degraded). | Closes the explicit USE saturation/error holes; all are `/proc` or `/sys` reads, unprivileged, fit the existing pure-map pipeline. | **M** | No |
| P4 | **Derive disk `%util` and queue depth** from `/proc/diskstats` (`io_time` delta / interval → util%; `weighted_io_time` / `io_time` → avg queue). Emit `disk/<dev>/io/util_percent` + `…/queue_depth`. | The #1 storage saturation signal; `time_ms` is already read but thrown into a raw counter. | **S** | No |
| P5 | **Memory composition + `MemAvailable`-based pressure.** Emit `memory/{cached,buffers,slab,dirty,writeback}` and compute used% from `MemTotal-MemAvailable` not `used`. | Current used% double-counts reclaimable cache; misleads the topology heatmap and overview "high memory" count. | **S** | Behavioral change to `memory/usage_percent` (acceptable per brief) |
| P6 | **Derived host saturation score (0–100)** computed in the sensor: a weighted blend of PSI some_avg10 (cpu/mem/io), run-queue/nCPU, swap-in rate, disk %util, FD%. Emit `system/saturation_score` + a `system/health_state` Text (`ok/warn/crit`). | One number the dashboard, topology tint, and alerting can all key off — turns the USE matrix into an at-a-glance signal. | **M** | No |
| P7 | **eBPF saturation histograms (opt-in, `collect.ebpf`, `CAP_BPF`)**: `runqlat`-style scheduler run-queue latency and `biolatency`-style block-I/O latency as bucketed histograms, served on a new `@/query/latency` channel (high-cardinality → not streamed). Use `aya` (pure-Rust, no libbpf C dep). | Tail-latency saturation `/proc` polling cannot see; "24×7-safe, zero overhead at rest" per Gregg. Gated + off-by-default keeps the no-privilege default intact. | **L** | No |
| P8 | **Adopt an OTEL-aligned metric contract** (internal key → semconv mapping table) so the OTEL exporter emits `system.*` with proper `state`/`direction` attributes, and document it. | Makes ZenSight host metrics portable and dashboard-compatible; current ad-hoc keys are a per-metric translation burden. | **M** | Yes (key rename — brief permits) |
| P9 | **Fix `docs/SENSORS.md` sysinfo section** to the real keyspace (it says `net/<iface>` and omits PSI/cgroup/power/vmstat). | Doc is wrong today; cheap correctness win. | **S** | No |

#### Frontend proposals

- **Trends over snapshots**: give every numeric panel the existing `metric_sparkline` treatment (disk-IO rates, net rates, temps, PSI avg10/avg60 dual-line, throttle rate). The 500-sample history (`device.rs:101`) already exists — it's just not rendered.
- **Host health score header**: surface P6's `system/saturation_score` as a prominent gauge + `ok/warn/crit` LED at the top of the host view and as the topology node tint (replacing the current raw `cpu/usage`-only tint at `topology/mod.rs:394`).
- **USE matrix card**: a compact CPU/Mem/Net/Disk × Util/Sat/Err grid, each cell green/amber/red from the P3/P4 signals — the single most legible expert view.
- **Process explorer (P2)**: sortable table from `@/query/processes`, with sort toggle and per-process drill-in (rss/vsz/threads/io/state/uid are already in the DTO).
- **Pressure visualization**: stacked PSI area chart (cpu/mem/io `some` vs `full`) over time, not three text rows.
- **Power/thermal card**: render the already-collected RAPL watts, fan RPM, battery, and `temp` vs `crit` as a gauge bank (currently `power` produces data with no UI at all).

#### Prioritized shortlist (top 5 by ROI)

1. **P1 — sysinfo alerting.** Turns a pile of collected saturation numbers into actionable pages; closes the one-sensor-can't-alert gap. (M)
2. **P2 — wire `@/query/processes` into a UI process explorer.** Dead code → flagship feature; minimal new code since the channel + DTO + tests already exist. (S–M)
3. **P4 + P5 — disk `%util`/queue depth and memory-available pressure.** Two small derivations that fix the two most misleading current signals (storage saturation invisible; memory over-reported). (S each)
4. **P6 — host saturation score + topology/dashboard integration.** One coherent USE-derived number that the whole UI and alerting can consume. (M)
5. **P3 (subset) — `netstat`/`softnet`/`schedstat`/`conntrack`.** Closes the explicit USE network/CPU saturation holes with cheap unprivileged `/proc` reads in the existing pipeline. (M)

eBPF (P7) and the OTEL contract rename (P8) are higher-effort follow-ons worth doing after the above land.

**Sources**: [node_exporter README](https://github.com/prometheus/node_exporter/blob/master/README.md) · [node-exporter guide](https://prometheus.io/docs/guides/node-exporter/) · [OTEL system semconv](https://opentelemetry.io/docs/specs/semconv/system/system-metrics/) · [USE method](https://www.brendangregg.com/usemethod.html) · [USE Linux checklist](https://www.brendangregg.com/USEmethod/use-linux.html) · [Add BPF observability](https://www.brendangregg.com/blog/2021-07-03/how-to-add-bpf-observability.html) · [Linux eBPF tools](https://www.brendangregg.com/ebpf.html) · [runqlat](https://www.brendangregg.com/blog/2016-10-08/linux-bcc-runqlat.html)

---

## Logs sensor (syslog + journald)

> Crate `zensight-sensor-logs/` (renamed from `…-syslog`, commit `4ac267e`). Note a load-bearing inconsistency: the crate is "logs" but the wire identity is still `syslog` everywhere — `Protocol::Syslog`, key prefix `zensight/syslog/...`, the CLI default `syslog.json5`, and the runner name `"syslog"` (`main.rs:30‑36`). The frontend types are also still `SyslogMessage` / `SyslogSeverity` / `SyslogFilterState`. This is a SOTA gap in itself (see B/§OTel) and the cheapest high-leverage rename to fold in during the redesign.

### A. Current state

#### Ingestion paths

Four sources funnel into one `mpsc::channel(1000)` of `ReceivedMessage` (`receiver.rs:120‑184`), so everything downstream (filter → event-detect → telemetry map → publish → derived rollup) is source-neutral:

- **UDP** (`receiver.rs:187`): one datagram = one message; UTF‑8 with lossy fallback; `max_message_size` buffer; unparseable datagrams are dropped at `debug` with no counter.
- **TCP / Unix** (`receiver.rs:241`, `:340`): newline-delimited via `BufReader::lines()`, per-connection `max_connections` semaphore + per-read `connection_timeout`. **Note:** this is LF-framed only — it does **not** implement RFC 6587 octet-counting (`MSG-LEN SP MSG`), so a syslog-over-TLS/TCP sender using octet framing will be mis-parsed. (No TLS at all, incidentally.)
- **journald** (`journald.rs`, feature-gated `journald`): a dedicated OS thread (because `systemd::journal::Journal` is `!Send`) reads via libsystemd and `blocking_send`s into the same channel (`receiver.rs:160‑181`).

#### Parser (`parser.rs`) — regex, three-tier fallback

`parse()` tries RFC 5424 (`RFC5424_REGEX`, `:184`), then RFC 3164 (`:190`), then a PRI-only `SIMPLE_REGEX` (`:197`). Facility/severity decoded from PRI (`pri >> 3`, `pri & 0x07`). RFC 5424 SD parsing is a hand-rolled regex (`SD_REGEX`, `SD_PARAM_REGEX`) with correct RFC 6.3.3 unescaping (`unescape_sd_value`, `:234`). Weaknesses: regex-per-line (no zero-copy / SIMD), RFC 3164 timestamp assumes *current* year (`:379` — breaks around New Year and on replayed logs), no RFC 5425 (TLS) / 6587 (framing), and escaped `]` inside SD values is acknowledged-broken (`:548` test comment).

#### Field / label mapping (`receiver.rs:501‑565`)

```
metric  = "<facility>/<severity>"        e.g. "auth/crit"    (:549)
value   = Text(message)                                       (:550)
key     = zensight/syslog/<hostname>/<facility>/<severity>    (build_key_expr :556)
labels  = facility, severity(string name), app, pid, msgid,
          sd.<sd_id>.<key>  (flattened SD),                   (:526)
          source_type = "<addr>"|"unix"|"journald",           (:533)
          raw (optional, include_raw_message)
```

This is the crux of the data-model problem. **The metric path encodes facility/severity, so the key-expression namespace is dimensioned by `(host × facility × severity)` and the message text is the *value*.** Every distinct line `PUT`s to one of ~24×8 keys per host, last-writer-wins — so Zenoh storage/last-sample retains only the *latest* line per (host, facility, severity); the full stream only exists transiently in subscribers. `severity` is the abbreviated string (`"crit"`, `"err"`), `facility` likewise — there is no numeric severity label, which the frontend then has to re-parse (`syslog.rs:938` `syslog_message_from_point` splits the metric path).

#### journald field mapping (`journald.rs:503‑586`)

`map_record` bypasses the syslog regex entirely (entries are already structured). Severity from `PRIORITY` (default Notice), facility from `SYSLOG_FACILITY` else inferred kernel/user from `_TRANSPORT` (`:519‑528`), `app_name` = `SYSLOG_IDENTIFIER` ?? `_COMM`, timestamp prefers `_SOURCE_REALTIME_TIMESTAMP` then journal recv time (`:539`). `MESSAGE_ID` → `msg_id` (`:535`). Rich fields collected under one SD-element `journald` → surfaced as `sd.journald.<label>` labels: `STANDARD_FIELDS` (`:475`, unit/user_unit/slice/comm/exe/cmdline/uid/gid/boot_id/machine_id/transport), gated `DEV_FIELDS` (`:489`, code_file/line/func/errno), plus an `extra_fields` allowlist copied verbatim. So `sd.journald.unit` is the per-unit key the rest of the system pivots on.

#### Severity / facility handling

8-level RFC 5424 severity enum (`parser.rs:104`) and 24 facilities (`:11`). Mapping to telemetry uses the *abbreviated* string form (`as_str`, `:132`). The error/warning thresholds in derived rollups use `severity ≤ Error` (`derived.rs:62`).

#### Known-event detection → alerts (`events.rs`)

A **hardcoded table of 4 MESSAGE_IDs** (`known_event`, `:40‑49`): coredump (Critical), unit-failed (Warning), oomd-kill (Critical), kernel-oom (Critical). `process-exited` deliberately omitted as noise (`:38`). Fires a one-shot `Alert{Kind::Anomaly, rule:"journald-event"}` on `zensight/syslog/@/alerts/*`, deduped by `(event,unit)` for `event_dedup_secs`, auto-resolved via a reconcile loop (`:175`). Severity overridable per-ID via config. Detection runs **before** filtering (`main.rs:272`) so a coredump alerts even if filtered out. This is the *entire* MESSAGE_ID story — no catalog, no enrichment, no surfacing of the ID anywhere but the alert label.

#### Robustness (#62, journald-only)

- **Overflow** (`journald.rs:312‑338`): `OverflowPolicy::Block` (backpressure via `blocking_send`) vs `DropNewest` (`try_send` + count). Closed channel = shutdown, persists cursor.
- **Rate-limit** (`RateLimiter`, `:40‑77`): token bucket over a 1s window; over `max_eps`, keeps 1-in-`sample_ratio`, rest counted `sampled_out`. Applied **before** mapping (saves decode cost).
- **Drop accounting → health** (`receiver.rs:57‑115` `JournaldStats`; `main.rs:170‑218`): atomic counters read/published/dropped/sampled_out/decode_errors/invalidations; a 10s monitor computes `loss_ratio_since` and edge-triggers an `ErrorReport` ("journald dropping logs: X% loss…") when loss exceeds `drop_alert_ratio` (default 1%). This is genuinely good — "healthy ≠ process up."
- **Cursor resume** (`:341‑455`): atomic temp+rename cursor file, `STATE_DIRECTORY`/XDG resolution, `start_from` head/tail/since/boot/cursor with `on_missing_cursor` fallback, rotation handled (`JournalWaitResult::Invalidate`). Solid.
- **Network paths have *none* of this** — UDP drops are silent (`receiver.rs:227`), no rate-limit, no drop counter, no parse-failure metric.

#### Derived telemetry (`derived.rs`, #63)

A `LogAggregator` observes each *post-filter* message (`main.rs:288`) and on a tick emits, under `zensight/syslog/<sensor-host>/logs/...`:

- `logs/by_severity/<level>_total` (counter, 8 levels, full names `emergency..debug`, `:118`)
- `logs/errors_total`, `logs/warnings_total` (`:121`)
- `logs/by_unit/<unit>/messages_total` and `/errors_total`, **bounded** to `top_units` + an `other` bucket (`:124‑136`, cardinality-safe)
- `logs/units_in_failure` (windowed gauge, reset each emit, `:140`)
- `logs/journald/{read,published,dropped,sampled_out}_total` (when journald active, `:146`)

Source is the sensor's own hostname (`main.rs:235`) to keep cardinality bounded — a deliberate, correct choice. This is the closest thing to RED-for-logs in the system.

#### Frontend (`zensight/src/view/specialized/syslog.rs`, `overview/syslog.rs`)

The app keeps a rolling buffer of `SyslogMessage` (built from points via `syslog_message_from_point`, `:938`). Two entry points: a top-level **Logs view** (`logs_view`, `:412`) and a per-device **syslog_event_view** (`:302`). Features present:

- Severity pick-list (`Emergency+`…`Debug`), facility toggle-buttons, **unit chips** (journald lens, #64, `:589`), app substring, message substring — all applied **locally** (`apply_local_filters`, `:1011`). Dynamic sensor-side filters wired via `Message::ApplySyslogFilters` (`filter.rs`/`commands.rs`).
- **Source/provenance badge** per row (journald/unix/net, `:862`) and a **Unit** column (`:875`).
- **Severity summary** bar + the **`render_logs_rollup` panel** (`:337`) consuming `logs/*` (errors/warnings/units-in-failure/journald throughput/top-10 noisiest units).
- Stream rendered via Iced `table`, **sorted desc, truncated to 100 rows** (`:795`), message text truncated to 100 chars (`:890`).

**What's NOT done** (tracked #64/#93): no structured-field drill-down (you can't expand a row to see `sd.journald.*`, `pid`, `code_file`, `errno`); no MESSAGE_ID catalog / explanation; no follow/pause live-tail (buffer just grows and re-sorts); no boot selector; no full-text/regex search beyond naive `contains`; no template/pattern view; no novelty surfacing; no per-unit error-budget panel; no correlation jump from an alert to the originating lines. The overview (`overview/syslog.rs`) is also **stale** — it keys on `metric.starts_with("message/")` (`:135`) and `labels["severity"]` as a *numeric* string (`:139`), neither of which the current sensor emits (it emits `<facility>/<severity>` paths and string severity). **The overview is effectively dead code against the live contract.**

### B. State of the art

**OpenTelemetry Logs data model.** The reference model is `Timestamp`, `ObservedTimestamp`, `SeverityNumber` (1–24, monotone), `SeverityText`, `Body` (`AnyValue` — string *or* structured map), `Attributes` (free k/v), `Resource` (fixed per-source), and trace-correlation `TraceId`/`SpanId`/`TraceFlags` ([data-model](https://opentelemetry.io/docs/specs/otel/logs/data-model/)). Semantic conventions add `log.record.original` (preserve the raw syslog string), `log.record.uid` for **dedup** (ULID/UUID — same uid ⇒ duplicate, safely droppable), and an explicit **syslog→OTel mapping** via OTTL ([general/logs](https://opentelemetry.io/docs/specs/semconv/general/logs/), [semconv logs.md](https://github.com/open-telemetry/semantic-conventions/blob/main/docs/general/logs.md)). Mature systems treat a log as a *structured record with severity + attributes + trace links*, not a string under a `facility/severity` key.

**journald structure & catalog.** `MESSAGE_ID` is a 128-bit catalog UUID; systemd ships a **message catalog** (since v196) keyed off it, with `Subject`, `Defined-By`, and `Documentation` URI headers, designed exactly to turn an opaque ID into an operator-facing explanation ([systemd.io/CATALOG](https://systemd.io/CATALOG/), [journal-fields(7)](https://man7.org/linux/man-pages/man7/systemd.journal-fields.html)). COREDUMP entries (via `systemd-coredump`) and `_AUDIT_*` / SELinux fields are first-class structured fields. SOTA = enrich on the ID, expose the structured fields, and special-case coredumps/audit.

**Log-based metrics / RED-for-logs.** Loki/LogQL, Vector, and Fluent Bit all generate metrics *from* the log stream — error-rate, request-rate, top-N producers, and recording rules that materialize LogQL as time series for SLO/error-budget alerting ([LogQL metric queries](https://grafana.com/docs/loki/latest/query/metric_queries/), [Fluent Bit→Loki](https://grafana.com/docs/loki/latest/send-data/fluentbit/)). ZenSight's `derived.rs` is a (good, narrow) instance of this pattern.

**Templating / clustering / novelty.** [Drain3](https://github.com/logpai/Drain3) is the de-facto streaming log-template miner: a fixed-depth parse tree extracts a template + parameters per line online (`"Unable to access DB1"` → `"Unable to access <*>"`), collapsing millions of lines into hundreds of clusters. This is the foundation for noise reduction, **novelty detection** ("a template never seen before"), per-template rate/anomaly, and ML-on-logs ([Drain paper](https://netman.aiops.org/~peidan/ANM2023/6.LogAnomalyDetection/phe_icws2017_drain.pdf), [TPLogAD](https://arxiv.org/html/2411.15250v1)). Severity/dedup/sampling at scale: dedup via `log.record.uid`, keep-all-errors + sample-the-rest (which `RateLimiter` already half-does), and template-aware sampling (cap per template, never drop a novel one).

### C. Redesign proposal

#### Gap analysis vs SOTA

| Area | Current | SOTA | Gap |
|---|---|---|---|
| Data model | `metric=facility/severity`, `value=Text`, string severity, LWW key | OTel record: SeverityNumber, Body, Attributes, raw, uid, trace links | **Large** — string-as-value + LWW loses the stream; not OTel-mappable without rework |
| Retention | Transient (LWW key + 100-row UI buffer) | Indexed store / object store + recording rules | **Large** — no history, no search-back |
| MESSAGE_ID | 4 hardcoded → alert label only | catalog enrichment (Subject/Docs), arbitrary IDs | **Large** |
| Templating/novelty | none | Drain3 clusters + "what's new" | **Large** — biggest signal-quality win |
| RED/SLO | per-severity & per-unit counters | error-budget/burn-rate per unit | **Medium** — data is there, no budgets |
| Multiline/stacktrace | none (LF-split shatters them) | continuation joining | **Medium** |
| Audit/coredump | coredump *alert* only, no payload | structured coredump/audit records | **Medium** |
| Network robustness | none | rate-limit/drop-accounting parity | **Medium** |
| Framing/transport | LF only, no TLS | RFC 6587 octet-count, RFC 5425 TLS | **Medium** |

#### Proposals

**C1 — OTel-aligned record model + stop using the message as a Zenoh value. [L, breaking]**
Add a numeric `severity_number` label (or, better, a first-class field) and keep `severity_text`; carry `log.record.original` (raw) and a `log.record.uid` (ULID from host+timestamp+hash) for dedup. Critically, **decouple the key from facility/severity** so the per-line stream isn't last-writer-wins: publish each line under a monotonic/uid-suffixed key (`zensight/logs/<host>/events/<uid>`) or treat lines as a Zenoh *stream* the GUI subscribes to, and reserve the `facility/severity` rollups for `logs/*` counters (already correct). *Rationale:* the current model silently discards all but the newest line per (host,facility,severity) in any store; it's the root cause of "no history." *Compat:* breaks subscribers and the exporters' syslog mapping — acceptable per the brief. Bundle the `syslog`→`logs` protocol/prefix rename here.

**C2 — Drain3-style streaming template miner. [M, additive]**
Add a `template.rs`: fixed-depth parse tree, per-message `template_id` (FNV of the masked template) + masked `template`. Emit as labels and as derived series `logs/by_template/<id>/{count,errors}_total` (bounded top-N like units). Mask obvious variables (ints, IPs, UUIDs, hex, paths) before clustering. *Rationale:* single biggest noise-reduction + the substrate for novelty. *Compat:* purely additive.

**C3 — Novelty / "what's new" detection → alerts. [S, additive, builds on C2]**
Maintain a seen-templates set with first-seen + a warm-up window; when a template appears that's new after warm-up (or whose rate jumps N× over its EWMA baseline), raise an `Alert{Anomaly, rule:"log-novelty"}` reusing the existing `AlertReporter`/dedup machinery from `events.rs`. *Rationale:* catches unknown-unknowns that the 4 hardcoded MESSAGE_IDs never will. *Compat:* additive.

**C4 — MESSAGE_ID catalog enrichment. [M, additive]**
Replace the 4-entry hardcode (`events.rs:40`) with a catalog lookup: read the local systemd catalog (`sd_journal_get_catalog_for_message_id` / `journalctl --list-catalog`) to attach `Subject` + `Documentation` to any MESSAGE_ID, surface them as labels (`catalog.subject`, `catalog.docs`) and in the alert summary. Keep a small built-in severity map for the high-signal IDs. *Rationale:* turns every catalogued event (not just 4) into an explained record; directly feeds the frontend MESSAGE_ID catalog (#93). *Compat:* additive; alert labels gain fields.

**C5 — Per-unit error budgets / SLOs. [S, additive]**
On top of `logs/by_unit/*` add `logs/by_unit/<unit>/error_ratio` and a burn-rate gauge against a configurable budget; raise an alert on multi-window burn. *Rationale:* the counters already exist (`derived.rs`); this is the missing SLO layer. *Compat:* additive.

**C6 — Multiline / stacktrace joining. [M]**
Before parsing on TCP/Unix, join continuation lines (leading-whitespace / "Caused by:" / language-specific stack frames) with a flush timeout, so a Java/Python traceback is one record. journald is already one-record-per-entry (newlines preserved in `MESSAGE`), so this is network-path only. *Rationale:* LF-splitting shatters stacks today, destroying both readability and templating. *Compat:* additive (behavior change on multiline input).

**C7 — Coredump & audit handling. [M, additive]**
For coredump entries, capture the structured `COREDUMP_*` fields (exe, signal, pid) into attributes and link to `coredumpctl`; for `_AUDIT_TYPE`/SELinux, map to a `security`-tagged record so the Security view can lens them. *Rationale:* today a coredump only yields a truncated 160-char alert summary. *Compat:* additive (extend `extra_fields`/`DEV_FIELDS`).

**C8 — Network-path robustness parity + framing/TLS. [M]**
Lift `RateLimiter`/`JournaldStats` drop-accounting out of `journald.rs` into a shared `ingest` layer so UDP/TCP get rate-limit, drop counters, and parse-failure metrics (`logs/ingest/{received,parsed,parse_failed,dropped}_total`). Add RFC 6587 octet-counted framing and RFC 5425 TLS to the TCP listener. *Rationale:* UDP drops and parse failures are invisible today (`receiver.rs:227,228`). *Compat:* additive metrics; framing/TLS are opt-in config.

**C9 — Retention/store strategy. [L]**
Per the v3 design ("hot ring + redb"), give the GUI (and/or a small store sensor) a bounded hot ring + redb-backed cold store keyed by uid/timestamp, with template-aware sampling (keep all errors + all novel templates, sample repetitive info). *Rationale:* makes search-back, follow/pause, and boot selection actually possible. *Compat:* additive.

#### Frontend proposals

- **F1 — Structured drill-down [M]:** expand a row to a detail panel showing all labels (`sd.journald.*`, pid, code_file/line/func, errno, raw). Today none of the rich journald structure is reachable in the UI. (#64/#93)
- **F2 — Live tail follow/pause [S]:** a follow toggle that pins to newest and a pause that freezes the buffer; today `render_log_stream` always re-sorts+truncates to 100 (`syslog.rs:795`).
- **F3 — Template explorer [M, needs C2]:** a "Patterns" tab listing templates by volume/error-rate with sparklines and a "show matching lines" drill — the highest-ROI noise-reduction surface.
- **F4 — Novelty surfacing [S, needs C3]:** a "New today" strip highlighting first-seen templates and rate spikes.
- **F5 — Unit error-budget panel [S, needs C5]:** extend `render_logs_rollup` (`syslog.rs:337`) with per-unit budget bars / burn-rate, reusing the netring RED card pattern it already mirrors.
- **F6 — Alert↔logs correlation [M]:** from a `journald-event`/novelty alert, jump to the originating lines (filter buffer by unit+window). The labels (`unit`, `event`, `message_id`) already exist on the alert.
- **F7 — Real search + boot selector [M]:** regex/field search (replace `contains`, `:1044`) and a boot dropdown (needs C9 history). **Also: retire/rewrite `overview/syslog.rs`** — it reads a contract (`message/*` keys, numeric severity label) the sensor no longer emits, so it renders nothing.

#### Prioritized shortlist (top-5 ROI)

1. **C2 Drain3 templating** (+F3 explorer) — collapses noise, unlocks everything downstream. *[M]*
2. **C3 novelty detection** — catches unknown-unknowns the 4 MESSAGE_IDs can't; cheap on top of C2. *[S]*
3. **C1 OTel-aligned model + decouple key from facility/severity** (incl. `syslog→logs` rename) — fixes the silent stream-loss root cause and makes the system OTel-mappable. *[L, breaking]*
4. **C4 MESSAGE_ID catalog enrichment** (+F1 drill-down) — turns opaque IDs into explained, navigable records. *[M]*
5. **C5 per-unit error budgets** (+F5 panel) — the SLO layer the counters already imply; near-free given `derived.rs`. *[S]*

(Honorable mention: **C8 network robustness parity** — small, closes a real observability blind spot where UDP drops/parse failures are invisible.)

**Sources:** [OTel logs data model](https://opentelemetry.io/docs/specs/otel/logs/data-model/) · [OTel general/logs semconv](https://opentelemetry.io/docs/specs/semconv/general/logs/) · [systemd message catalog](https://systemd.io/CATALOG/) · [journal-fields(7)](https://man7.org/linux/man-pages/man7/systemd.journal-fields.html) · [Drain3](https://github.com/logpai/Drain3) · [Drain paper](https://netman.aiops.org/~peidan/ANM2023/6.LogAnomalyDetection/phe_icws2017_drain.pdf) · [Loki LogQL metrics](https://grafana.com/docs/loki/latest/query/metric_queries/)

---

## Netlink sensor

The `zensight-sensor-netlink` crate is the most ambitious sensor in the workspace: a single-host (it monitors `self`) agent that polls RTNETLINK / sock_diag / genetlink via the `nlink` git dependency, streams a low-cardinality telemetry summary, serves high-cardinality detail on demand via Zenoh queryables, and embeds a **sentinel** (Pillar B) expectations engine that emits `Alert`s on deviation. It is architecturally the cleanest sensor in the repo — a strict *pure-map / live-collector* split (`map.rs` carries zero `nlink` dependency, `collector.rs` does all the I/O) — and that discipline is what makes the redesign below tractable.

### PART A — Current state

#### Telemetry surface (every streamed point)

All points are `Protocol::Netlink`, `source = <hostname>`, published at `zensight/netlink/<host>/<metric>` via a cached `AdvancedPublisherRegistry` (late joiners get last value through `history()`). Poll interval default 5 s (`config.rs:12`). Pure builders in `map.rs`:

| Domain | Metrics | Value type | Labels | Builder |
|---|---|---|---|---|
| **iface/`<name>`/** | `rx_bytes tx_bytes rx_packets tx_packets rx_errors tx_errors rx_dropped tx_dropped multicast collisions` | `Counter` | `ifindex` | `iface_points` `map.rs:190` |
| | `oper_state` (Text), `up`/`carrier` (Bool), `mtu` (Gauge), `info`=MAC (Text, `mac` label) | mixed | `ifindex`,`mac` | `map.rs:208-254` |
| **sockets/tcp/** | `established listen time_wait syn_sent close_wait` (Gauge), `retransmits_total` (Counter), `max_rtt_us rtt_p50_us rtt_p95_us` (Gauge) | mixed | — | `socket_points` `map.rs:261` |
| | `mem/{snd,rcv}_buf_total` (Gauge, omitted if 0), `by_cong/<algo>` (Gauge, bounded) | Gauge | — | `map.rs:309-329` |
| **routes/** | `ipv4_count ipv6_count total` (Gauge), `default_v4_present default_v6_present` (Bool), `default_v4_gw` (Text, `gateway` label) | mixed | `gateway` | `route_points` `map.rs:334` |
| **neighbors/** | `by_state/{reachable,stale,failed,incomplete,permanent,other}`, `total` | Gauge | — | `neighbor_points` `map.rs:373` |
| **diagnostics/** | `issues/{info,warning,error,critical,total}` (Gauge), `bottleneck_score` (Gauge 0..1), `bottleneck` (Text + `location`/`recommendation`/`drop_rate` labels) | mixed | loc/rec | `diagnostics_points` `map.rs:390` |
| **events/`<family>`/** | `{added,removed,changed}_total` for family ∈ {link,addr,route,neighbor} (12 counters) | Counter | — | `EventState::counter_points` `events.rs:293` |
| **ethtool/`<iface>`/** | `carrier full_duplex autoneg pause/{rx,tx,autoneg}` (Bool), `speed_mbps duplex(Text) rings/{rx,tx,rx_max,tx_max}` (Gauge), `pause/{rx,tx}_frames` (Counter), `features/<name>` (Bool, curated 6) | mixed | — | `ethtool_points` `map.rs:562` |
| **addresses/** | `ipv4_count ipv6_count global_count total` | Gauge | — | `address_points` `map.rs:685` |
| **tc/`<iface>`/`<kind>`/** | `drops overlimits requeues bytes packets` (Counter), `backlog_bytes backlog_pkts` (Gauge) | mixed | `handle` | `tc_points` `map.rs:736` |
| **xfrm/** | `sa/total sa/by_mode/<m> sa/by_proto/<p> policy/total` | Gauge | — | `xfrm_points` `map.rs:825` |
| **nft/** | `tables_total chains_total rules_total`, `<family>/<table>/{chains,rules}` | Gauge | — | `nft_points` `map.rs:885` |
| **conntrack/** | `entries by_proto/{tcp,udp,icmp,other} max utilization` | Gauge | — | `conntrack_points` `map.rs:484` |
| **wireguard/`<iface>`/** | `peers` (Gauge), `<peer>/{rx_bytes,tx_bytes}` (Counter, `endpoint` label), `<peer>/last_handshake_age_s` (Gauge), `<peer>/up` (Bool) | mixed | `endpoint` | `wireguard_points` `map.rs:61` |

**Aggregation logic** (`collector.rs`): `aggregate_sockets` (`:772`) walks every sock_diag `Inet` entry, sums `tcp_info.retrans`, tracks max + builds an RTT vector for nearest-rank `percentile` (`:814`); counts congestion algorithm only for established sockets; sums `mem_info.sndbuf/rcvbuf`. The sock_diag filter requests `with_tcp_info().with_mem_info().with_congestion()` (`:742`). `aggregate_conntrack`/`aggregate_routes`/`aggregate_neighbors`/`aggregate_xfrm`/`aggregate_addresses` are all pure and unit-tested. `nf_conntrack_max` is a one-shot procfs read (`read_conntrack_max` `:1067`).

**Cardinality discipline (P2)** is consistently applied: per-issue diagnostics collapse to severity *counts*; ethtool features are a curated 6-element set (`CURATED_FEATURES` `:883`); WG peer ids are 8-char base64 pubkey prefixes (`wg_peer_view` `:1021`, hand-rolled `base64_encode` `:1043`). Absent optionals emit *no* point (no misleading zeros) — verified throughout the tests.

#### Events as a Stream (#8)

`run_event_stream` (`collector.rs:1000`) holds a *dedicated* `Connection::<Route>` (its `events()` borrows the connection for the stream lifetime and holds the request lock, so the poll loop's connection stays free), subscribes to 6 multicast groups (`:279`), folds each `NetworkEvent` into `EventState` (atomics + bounded ring, `events.rs:216`), and — for `is_sentinel_relevant` events (`events.rs:135`) — calls `wake.notify_one()` so the sentinel re-sweeps in ~0 s instead of at its tick. `refine_action` (`events.rs:270`) distinguishes link *add* vs *change* by tracking seen ifindexes. This is the strongest part of the design.

#### Sentinel / expectations (`sentinel.rs`)

Pure checks (`check_socket`, `check_link`, `check_neighbor`, `check_route`, `check_metric`) operate on `*Observation` structs built from live `nlink`; `Evaluator::sweep` (`:521`) runs them on a cadence OR on event-wake, feeds `AlertReporter` (firing/resolved/reconcile, per-rule debounce via `for_secs`), and resolves rules removed by hot-swap (`seen_rules` diff `:618`). `SentinelHandle` (`:376`, `Arc<RwLock<ExpectationsConfig>>`) is hot-swappable from the `command` channel / GUI. Five expectation kinds: socket (listen / forbid_listen / established_to≥min), link up, neighbor reachable, default route present/via, and a generic **metric-threshold** (`MetricExpectation` `:142`) that reads the collector's `MetricCache` (`collector.rs:46`) — the keystone for promoting a GUI threshold rule into a headless expectation (shares `ComparisonOp` with the frontend).

#### Config & runtime control

`CollectConfig` (`config.rs:91`) has 12 per-collector toggles, hot-swappable at runtime via `CollectHandle` (`collector.rs:83`) and the `collection` command. `nftables` + `conntrack` default **off** (need CAP_NET_ADMIN); all else on. `IfaceFilter` supports include/exclude/loopback/virtual. Connections open unprivileged and degrade gracefully (each `.ok()` with a warn). `warned_xfrm` is a warn-once latch for the recurring EPERM SA dump (`:150`).

#### Frontend integration

- **Specialized view** (`view/specialized/netlink.rs`): renders Diagnostics (with a trend sparkline on `bottleneck_score`, `:299`), Interfaces (full counter table #46), Sockets (RTT p50/p95, mem buffers, congestion distribution #46), Neighbors, Routes, and conditionally Conntrack/WireGuard/TC/xfrm cards (shown only `has_prefix`). On-demand detail (`render_detail` `:388`) has **fetch buttons for only Sockets / Routes / Neighbors**.
- **Detail client** (`netlink_detail.rs`): `NetlinkDetailTopic` enum has **only 3 variants** (Sockets/Routes/Neighbors); `fetch_records` is Iced-independent and integration-tested against a live in-process queryable.
- **Query channel** (`query.rs`): serves **8** topics — `routes neighbors sockets addresses events tc xfrm nft`.
- **Topology** (`view/topology/mod.rs`): netlink hosts become nodes (#83); `update_from_metrics` (`:379`) extracts `iface up/total`, `tcp_established/listen`, `routes_total`, `neighbors_total` into the info panel "Kernel Networking" section (`:702`). `apply_alerts` tints nodes by worst firing severity and `recompute_edge_health` (#49, `:192`) tints *edges* by worst endpoint.

**Collected-but-not-shown gaps:**
1. `@/query/{addresses, events, tc, xfrm, nft}` have **no GUI fetch path** — 5 of 8 queryables are dead from the frontend (`netlink_detail.rs` only knows 3 topics). The recent-events ring and full TC/xfrm/nft tables are unreachable in the UI.
2. **Edges are derived only from netring `FlowRecord`s** (`apply_flow_edges` `:178`). The deferred **#49 neighbor-adjacency edges** — building topology edges from the netlink ARP/NDP neighbor table — are **not implemented**. "Per-link health" today is only endpoint-alert tinting, not real link telemetry.
3. `events/*` counters stream but have no dedicated UI (no control-plane change timeline).
4. `addresses/*`, full `nft` inventory, per-SA xfrm detail, `ethtool/*` rings/pause/features stream or are queryable but get no first-class view (ethtool is not rendered at all in `netlink.rs`).
5. `socket explorer` is a flat 200-row table with no filtering UI, despite the sensor supporting `?state=&port=` selectors (`SocketSelector` `map.rs:441`).

Docs (`docs/SENSORS.md:107`, `docs/KEYSPACE.md:33/92/103`) are accurate and current.

### PART B — State of the art

**Netlink/sock_diag is the cheap, unprivileged floor.** The current sensor already extracts the right `tcp_info` fields per the SOTA — `rtt`, `cwnd` (`snd_cwnd` in `SocketRecord`), `retrans` — which is exactly what `ss -ti` surfaces via sock_diag ([ss CWND/RTT](https://oneuptime.com/blog/post/2026-03-20-ss-display-tcp-internal-cwnd/view)). But the kernel `tcp_info` exposes more that the sensor drops on the floor: **`delivery_rate`** and **`pacing_rate`** ([netdev: data delivery rate](https://lists.openwall.net/netdev/2016/09/17/42)), `bytes_retrans`, `total_retrans`, `rcv_rtt`, `lost`, `reord`. These are the difference between "12 retransmits somewhere" and "this flow is delivering 40 Mbps against a 1 Gbps pacing rate with growing reordering."

**eBPF is what netlink fundamentally can't give cheaply.** sock_diag is a *sampled snapshot* — it cannot see connection *lifecycle* (a connection that opens and closes between two 5 s polls is invisible) or *attribute* sockets to processes without racing `/proc`. The BCC/libbpf canon is mature ([eunomia 2024-25 review](https://eunomia.dev/blog/2025/02/12/ebpf-ecosystem-progress-in-20242025-a-technical-deep-dive/), [BCC tools](https://www.cloudraft.io/blog/ebpf-based-network-observability-using-cilium-hubble)):
- **`tcplife`** — connection lifetime, bytes, PID/COMM: the per-flow event netlink can't produce.
- **`tcprtt`** — RTT *distributions* (histograms), passively, from `tcp_rcv_established`.
- **`tcpconnlat`** — connect() → first-ACK latency (SYN→SYN-ACK), the canonical "is the network or the peer slow" signal.
- **`tcpretrans` / `tcp:tcp_retransmit_skb` tracepoint** — retransmits *attributed to a flow and PID* in real time. This is precisely how Coroot's node-agent measures loss ([Coroot service map](https://coroot.com/blog/engineering/building-a-service-map-using-ebpf/), [coroot-node-agent](https://github.com/coroot/coroot-node-agent)).

**The service-map model** (Cilium Hubble, Coroot) is the industry consensus: topology is *derived from observed flows*, enriched with per-edge RTT / retransmit / connection-status, with the agent resolving real destinations via the **conntrack table over netlink** ([Coroot](https://coroot.com/blog/engineering/building-a-service-map-using-ebpf/)). ZenSight already adopted the flow-derived model for netring edges — but does not enrich edges with the netlink RTT/retransmit data it already collects.

**Bufferbloat / latency-under-load** is a first-class signal class the sensor under-uses. `fq_codel` is the RHEL 8 default and CAKE adds per-host fairness + shaping ([Bufferbloat.net CAKE](https://www.bufferbloat.net/projects/codel/wiki/Cake/), [Dave Täht, netdev 0x17](https://netdevconf.info/0x17/docs/netdev-0x17-paper19-talk-slides/Low%20Latency%20Life%20Lessons%20Learned.pdf)). The qdisc `drops`/`overlimits`/`backlog` the sensor already streams (`tc/*`) are the raw bufferbloat indicators — fq_codel's per-flow drops + sustained backlog *are* the bufferbloat fingerprint — but they are presented as raw counters, not scored. Node_exporter/Netdata expose qdisc stats but likewise leave scoring to the user; a composite "qdisc health" score is a real differentiation opportunity.

### PART C — Redesign proposal

#### Gap analysis vs SOTA

| Capability | SOTA | ZenSight today | Gap |
|---|---|---|---|
| Per-socket RTT/cwnd snapshot | ✅ sock_diag | ✅ (`SocketRecord`) | Drops `delivery_rate`/`pacing_rate`/`bytes_retrans` |
| Per-flow connection lifecycle | eBPF `tcplife` | ❌ (5 s snapshot misses short conns) | **No lifecycle visibility** |
| Connect latency (SYN→ACK) | eBPF `tcpconnlat` | ❌ | **Missing** |
| Retransmit attribution (flow+PID) | eBPF tracepoint | ⚠️ host-total counter only | **No attribution** |
| Process→socket attribution | eBPF | ❌ | **Missing** |
| Service map / topology edges | flow-derived + RTT | ⚠️ netring flows only, no RTT | **No netlink-derived edges, no per-link RTT** |
| Bufferbloat scoring | raw qdisc stats | ⚠️ raw counters | **No score / no AQM classification** |
| Control-plane change timeline | — | ⚠️ counters + dead ring | **Ring not surfaced** |
| Route/BGP-ish change tracking | route-monitor | ⚠️ event counters only | **No default-route flap history** |

#### Sensor-side proposals

**N1 — Enrich `tcp_info` extraction (S, compat-additive).** Add `delivery_rate`, `pacing_rate`, `bytes_retrans`, `total_retrans`, `rcv_rtt`, `lost`, `reord` to `SocketCounts`/`SocketRecord` and the aggregate (e.g. `sockets/tcp/delivery_rate_p50`, a `sockets/tcp/reordered_total` counter). Pure extension of `aggregate_sockets` (`collector.rs:772`). *Rationale:* these are free — already in the dumped struct — and turn the socket view from "count of states" into "are flows actually delivering." Highest ROI for least effort.

**N2 — eBPF augmentation as an optional `cap`-gated module (L, compat-additive).** A new `ebpf` collector (feature-flagged, off by default, CAP_BPF/CAP_NET_ADMIN) attaching to `tcp:tcp_retransmit_skb`, `tcp_v4/v6_connect`+return (connlat), and `tcp_set_state` (tcplife). Emit:
- `sockets/tcp/connlat_us_p50/p95` (Gauge) — connect latency distribution.
- `sockets/tcp/retransmits/by_peer/<ip>` is too high-cardinality to stream → instead a bounded **top-K retransmit peers** ring served via a new `@/query/retransmits` channel (mirror the events ring pattern).
- A `@/query/connections` channel streaming recent `tcplife` records (PID, COMM, peer, duration, bytes, segs-retrans). *Rationale:* this is the single biggest capability gap vs Coroot/Hubble and the only way to get lifecycle + attribution. Keep it strictly opt-in so the unprivileged default story is preserved. Effort is L because it needs a vetted eBPF dependency (`aya` or `libbpf-rs`) and CO-RE skeletons.

**N3 — Bufferbloat / qdisc health score (M, compat-additive).** Add `tc/<iface>/<kind>/health_score` (Gauge 0..1) computed from drop-rate vs throughput, sustained backlog relative to BDP, and overlimit ratio — plus classify the AQM (`tc/<iface>/aqm_class` Text: `aqm` for fq_codel/CAKE, `fifo` for pfifo_fast/bfifo, `none`). A host without an AQM under load is itself a finding. *Rationale:* turns raw counters into the signal operators actually want and is a clear differentiator; node_exporter/Netdata don't score this.

**N4 — Default-route / control-plane change tracking (S→M, compat-additive).** Today only event *counters* exist. Add a bounded **default-route history ring** (`@/query/route_changes`) capturing default-route gateway/withdrawal transitions with timestamps (the event task already sees every `NewRoute`/`DelRoute`). Add a `routes/default_v4_flaps_total` counter. *Rationale:* a flapping default route is the #1 connectivity incident; the data flows past `run_event_stream` already.

**N5 — Per-link health from neighbor + ethtool + tc (M, compat-additive).** Synthesize a per-interface composite `iface/<name>/link_health` (Gauge) from carrier, error/drop rate deltas, ethtool half-duplex/autoneg-off, and qdisc backlog — the L1/L2 analog of N3. Feeds topology edges (see F2).

**N6 — Richer sentinel rules (M, compat-additive).** Extend expectations with: **rate-of-change** expectations (e.g. "rx_errors must not increase by >N/min" — needs the `MetricCache` to keep a previous sample + timestamp), **`delivery_rate` floor** per socket-group, **conntrack utilization** threshold (already a metric, just needs a typed rule), and **route-flap** expectations. The `MetricExpectation` path already generalizes the comparison; rate-of-change is the missing primitive. *Rationale:* the sentinel is the product's distinguishing feature; today its rules are all level-triggered on instantaneous state.

**N7 — Decode nft per-rule counters (M, compat-impacting if `nlink` bumped).** The comment at `map.rs:856` notes the pinned `nlink` `RuleInfo` exposes no decoded counters — only `expression_bytes`. Decoding the counter expression yields per-rule packet/byte traffic (firewall hit-rate, the actual value of nft telemetry). Effort/compat depends on an `nlink` upgrade.

#### Frontend proposals

**F1 — Wire the 5 dead queryables (S).** Extend `NetlinkDetailTopic` (`netlink_detail.rs:17`) and `render_detail` (`netlink.rs:388`) with Addresses, Events, TC-tree, XFRM, NFT fetch buttons. The query channels and JSON shapes already exist — this is pure frontend plumbing, immediate value.

**F2 — Topology neighbor-adjacency edges (#49) + router classification (M).** Build edges from the netlink neighbor table (ARP/NDP `is_router` is already captured in `NeighborRecord` `query.rs:237`): a `reachable` neighbor that is also a known node → an L2/L3 adjacency edge, distinct from netring flow edges (different style). Classify nodes as routers when they have many neighbors / a default-gateway role. Enrich edges with N5 `link_health` and (N1) RTT so edges carry real per-link health, not just endpoint-alert tint. *This closes the explicitly-deferred #49.*

**F3 — Socket / connection explorer (M).** Replace the flat 200-row table with a filterable explorer driving the existing `?state=&port=` selector (and, with N2, PID/COMM columns + the connection-lifecycle list). Sort by RTT/retrans to surface the worst flows.

**F4 — Control-plane change timeline (S→M).** A timeline view over `@/query/events` (the recent-events ring) + N4 route history: link up/down, address add/del, route changes, neighbor failures, on a time axis. This is the natural home for the `events/*` counters that currently stream into nowhere.

**F5 — Sentinel/expectations authoring UX (M).** Expectations authoring exists (`view/expectations.rs`); extend it to author the N6 rate-of-change rules and to **promote a socket/route from the detail explorer into an expectation** in one click (right-click a listening port → "expect this listening").

**F6 — Per-interface trend/error view (S).** Interface counters are rendered as instantaneous values; add error/drop-rate sparklines (the `metric_trend_and_alert` helper used for `bottleneck_score` at `netlink.rs:299` already exists — reuse per interface).

#### Prioritized shortlist (top 5 by ROI)

1. **F1 — wire the 5 dead queryables (S).** Five fully-built backend channels are invisible in the UI; pure plumbing, ships today.
2. **N1 — `delivery_rate`/`pacing_rate`/`bytes_retrans` (S).** Free data already in the dump; transforms the socket story from "state counts" to "delivery health."
3. **F2 / #49 — neighbor-adjacency topology edges + per-link health (M).** Closes the explicitly-deferred issue and is the visible payoff of all the neighbor/RTT data already collected; moves ZenSight toward the Hubble/Coroot service-map model.
4. **N3 — bufferbloat/qdisc health score + AQM classification (M).** Real differentiation on data already streamed (`tc/*`); the signal operators want and competitors leave raw.
5. **N2 — opt-in eBPF module (connlat + retransmit attribution + tcplife) (L).** The strategic gap vs SOTA; largest effort but the only path to connection lifecycle and per-process/per-flow attribution. Keep CAP-gated and off by default to protect the unprivileged baseline.

**Sources:** [Coroot eBPF service map](https://coroot.com/blog/engineering/building-a-service-map-using-ebpf/) · [coroot-node-agent](https://github.com/coroot/coroot-node-agent) · [Cilium Hubble / BCC tools overview](https://www.cloudraft.io/blog/ebpf-based-network-observability-using-cilium-hubble) · [eBPF ecosystem 2024-25](https://eunomia.dev/blog/2025/02/12/ebpf-ecosystem-progress-in-20242025-a-technical-deep-dive/) · [Bufferbloat.net CAKE](https://www.bufferbloat.net/projects/codel/wiki/Cake/) · [Dave Täht low-latency lessons (netdev 0x17)](https://netdevconf.info/0x17/docs/netdev-0x17-paper19-talk-slides/Low%20Latency%20Life%20Lessons%20Learned.pdf) · [ss CWND/RTT via sock_diag](https://oneuptime.com/blog/post/2026-03-20-ss-display-tcp-internal-cwnd/view) · [TCP delivery_rate (netdev)](https://lists.openwall.net/netdev/2016/09/17/42)

---

## Netring sensor (NDR)

The netring sensor (`zensight-sensor-netring/`) is ZenSight's passive wire-level NDR. It wraps `netring 0.27` (capture: AF_PACKET / AF_XDP / pcap replay) and `flowscope 0.19` (parsers + detectors), decomposes capture-path callbacks into pure views in `map.rs`, and ships them off the capture path via channels drained in `publish.rs`. This is the most capable sensor in the workspace — and, as Part A shows, it currently exposes a small fraction of what the underlying crates already provide.

### Part A — Current state (precise map)

#### Telemetry contract (`map.rs`, streamed to `zensight/netring/<sensor>/<metric>`)

| Domain | Metric strings | Type | Source |
|---|---|---|---|
| Flow lifecycle | `flow/started_total`, `flow/ended_total`, `flow/active` | Counter/Gauge | `map.rs:104-118` |
| Flow volume (RED) | `flow/bytes_total`, `flow/packets_total`, `flow/retransmits_total` | Counter | `map.rs:122-141` |
| Flow duration (RED) | `flow/duration_p50_ms`, `flow/duration_p95_ms` | Gauge | `map.rs:159-177` |
| Per-L4 composition | `flow/by_l4/{tcp,udp,icmp}/{bytes,flows}_total` | Counter | `map.rs:451-476` |
| TCP teardown | `tcp/resets_total`, `tcp/refused_total`, `tcp/closed_{fin,rst,idle}_total` | Counter | `map.rs:492-529` |
| Capture health | `capture/<src>/{packets,drops,drop_rate,freezes}`, `capture/<src>/xdp/<cause>` | Counter/Gauge | `map.rs:201-252` |
| TLS | `tls/handshakes_total`, `tls/distinct_fingerprints` | Counter/Gauge | `map.rs:285-300` |
| QUIC | `quic/distinct_sni` | Gauge | `map.rs:304-311` |
| SSH | `ssh/distinct_hassh` | Gauge | `map.rs:314-321` |
| Assets | `assets/discovered` | Gauge | `map.rs:359-366` |
| ICMP (RED) | `icmp/{unreachable,time_exceeded,mtu_signal}_total`, `icmp/by_kind/<slug>_total` | Counter | `map.rs:396-420` |
| DNS (RED) | `dns/queries_total`, `dns/unanswered_total`, `dns/responses_by_rcode/<slug>_total`, `dns/query_rtt_p{50,95,99}_ms` | Counter/Gauge | `map.rs:585-624` |
| HTTP (RED) | `http/requests_total`, `http/status_{2,3,4,5}xx_total`, `http/methods/<m>_total`, `http/latency_p{50,95}_ms` | Counter/Gauge | `map.rs:670-711` |
| Bandwidth | `bandwidth/<app>/bytes_per_sec` | Gauge | `map.rs:93-101` |

Discipline is good: cardinality guards are everywhere (`TLS_INVENTORY_CAP`, `TALKER_CAP`, etc. `monitor.rs:66-74`), and idle ticks deliberately do not re-publish zero-valued percentiles so cached gauges retain their last meaningful value (`map.rs:159-177`).

#### Anomaly detectors → `@/alerts` (`monitor.rs`)
- **PortScanTRW** — `PortScanDetector` (TRW), fed by `FlowEnded<Tcp>` success/fail (`monitor.rs:887-907`).
- **BeaconCv** — `BeaconDetector` (coefficient-of-variation), `FlowPacket`, threshold-gated + allowlisted (`monitor.rs:910-937`). Note: the **superior `RitaBeaconDetector` (Bowley-skew + MAD, bit-faithful to RITA, catches jittered Cobalt-Strike C2) exists in flowscope but is NOT wired** (`flowscope .../detect/patterns/rita_beacon.rs`).
- **ConnectionFlood** — local `TimeBucketedCounter` per `(dst,port)` (`monitor.rs:940-968`).
- **DgaScorer** — bigram log-likelihood on DNS SLD (`monitor.rs:971-1001`).
- **IcmpFlowError** — flow-killing ICMP (unreachable/TTL) with a correlated inner flow (`monitor.rs:498-552`).
- **Threat-intel (netring 0.27):** `flow_risk()` (nDPI-style), `ioc()` (IP/domain/JA3/JA4), `sigma()` — all emit `OwnedAnomaly` onto the same `ChannelSink` (`monitor.rs:1003-1028`). Decoded as detector kinds `obsolete_tls`, `cleartext_http_credentials`, `flow_risk`, `ioc_match`, `sigma_match` in `security.rs:57-77`.
- **cleartext-snmp** — `snmp` feature, v1/v2c community string (`monitor.rs:790-814`).
- **capture-overload** — debounced hysteresis `SensorHealth` alert (`monitor.rs:836-884`).

#### On-demand query channels (`query.rs`, served at `@/query/<topic>`)
`flows`, `tls`, `talkers?top=N`, `elephant_flows`, `dns?top=N`, `http?top=N`, `quic`, `ssh`, `assets` — high-cardinality detail pulled on drill-in, never streamed (principle P2).

#### Frontend integration
- `specialized/netring.rs` renders cards: Flows, TCP health, Bandwidth, TLS, DNS, HTTP, per-L4, QUIC, SSH, Assets, Capture health, and on-demand Recent Flows.
- `specialized/netring_detail.rs` + `fetch.rs` fetch only **5 of 9** query channels: `flows`, `tls`, `quic`, `ssh`, `assets`.
- `security.rs` is the NDR lens: anomalies grouped by detector with a "what it means" line (`detector_meta`, `security.rs:41-80`), a by-source "top offenders" rollup, severity filter, and per-anomaly evidence drill-down.

#### Collected-but-not-shown / not-fetched (gaps in the existing surface)
1. **`talkers` and `elephant_flows` query channels are served but never fetched by the GUI** (`netring_detail.rs` has no `talkers`/`elephants` fetch; `fetch.rs` keys list omits them). The sensor maintains an 8192-entry talker histogram and a 128-flow elephant ring that the operator can never see — only the per-*app* bandwidth gauge is rendered (mislabeled "Top Talkers" in `netring.rs:657`, which is actually per-app bandwidth, not the per-destination talker histogram).
2. **`dns?top=N` / `http?top=N` are served but never fetched** — the DNS/HTTP cards show only RED aggregates, never the top-SLD / top-host / top-NXDOMAIN tables the sensor already ranks (`map.rs:628`, `map.rs:714`).
3. **`AssetRecord.vendor` is collected (`monitor.rs:367-379`) but never rendered** (`netring.rs:264-293` shows mac/ip/hostname/platform/caps/seen-via only).
4. **JA3 is captured into `TlsRecord` but the TLS card only shows JA4** (`netring.rs:101-108`).
5. **`security.rs` shows alert *labels* as evidence but offers no pivot** — no flow drill-down, no "show me the flows for this src," even though `@/query/flows` exists.

#### Massive unused capability in the pinned deps (verified against `~/.cargo` sources)
The sensor wires **zero** of these netring 0.27 builder hooks: `on_p0f` (passive TCP/IP OS fingerprint), `on_yara_match`, `on_arp_anomaly` / `on_ndp_anomaly` (ARP/NDP spoofing — a classic lateral-movement/MITM signal), `on_ml_features` (`CicFlowFeatures`), `on_nprint`, `on_http_fingerprint` (JA4H) — confirmed unused via grep. flowscope 0.19 additionally ships **parsers the sensor never enables**: SMB2/3 + NTLM + DCE-RPC (`T1021.002`), RDP (`T1021.001`), Kerberos, LDAP, SMTP (named-exfil via MAIL FROM/RCPT TO), FTP, WireGuard (shadow-VPN tunnel detection), STUN, NTP, RADIUS, DNP3, Modbus, mDNS/SSDP/NetBIOS. flowscope also provides `TimeBucketedSet` (distinct-value-per-key sliding window — the exact primitive for DNS-tunneling "distinct labels per source" and host-scan detection) which the sensor doesn't use. JA4H/JA4SSH are present but gated behind flowscope's `ja4plus` feature (FoxIO License 1.1).

**`docs/KEYSPACE.md:104` and `docs/SENSORS.md:124-157` are also stale** — they list 6 query topics (`flows, tls, talkers, elephant_flows, dns, http`) and omit `quic`, `ssh`, `assets`.

### Part B — State of the art (NDR)

Mature NDR (Corelight/Zeek+Suricata, Vectra, Arkime, RITA) converges on a recognizable signal set:

- **Flow fingerprinting & correlation:** **Community ID** flow hashing is the de-facto standard for correlating a flow across tools (Zeek, Suricata, Wireshark, Security Onion all emit it) — turning cross-tool correlation into a string compare ([corelight/community-id-spec](https://github.com/corelight/community-id-spec), [Security Onion docs](https://docs.securityonion.net/en/2.4/community-id.html)). **JA4+** is the modern fingerprint suite: JA4 (TLS client, BSD), JA4S (server), JA4H (HTTP), JA4SSH (SSH session), JA4T/JA4X — human- and machine-readable, designed for threat-hunting and grouping actors ([FoxIO ja4](https://github.com/FoxIO-LLC/ja4), [FoxIO blog](https://blog.foxio.io/ja4+-network-fingerprinting)). Zeek shipped first-class JA4 support in 2026 ([zeek.org](https://zeek.org/2026/01/how-to-use-ja4-network-fingerprints-in-zeek/)).
- **Detections:** C2 **beaconing** (RITA's robust skew/MAD scoring, surviving jitter), **DGA**, **DNS tunneling** (long/encoded subdomains, high distinct-label cardinality), **newly-observed domains (NOD)** — first-time-seen domains catch phishing/C2 pre-block; Akamai flagged 13M malicious domains/month via NOD ([Akamai](https://www.akamai.com/blog/security-research/newly-observed-domains-discovered-13-million-malicious-domains), [Unit 42](https://unit42.paloaltonetworks.com/malicious-newly-observed-domains/)), **lateral movement** (peer-to-peer SMB, WMI TCP/135+ephemeral, reversed RDP direction — `T1021`), **data exfiltration** (outbound volume > 3σ over host baseline, transfers to cloud storage, DNS exfil) ([Corelight lateral movement](https://corelight.com/blog/detecting-lateral-movement-and-evasion), [CyberDefenders NTA guide](https://cyberdefenders.org/blog/the-ultimate-guide-to-network-traffic-analysis-for-soc-analysts/)).
- **Workflow:** every alert maps to **MITRE ATT&CK**; analysts **pivot from a detection to its flows and then to PCAP** (Arkime SPI-view pivot on JA3/SNI/host, background **retro-hunt** over stored PCAP) ([Arkime](https://github.com/arkime/arkime)); **detection tuning / allowlisting** is a first-class UX with measured FP rates.

### Part C — Redesign proposal

#### Gap analysis vs SOTA

| SOTA capability | ZenSight today | Gap |
|---|---|---|
| Community ID flow hashing | absent | No cross-tool flow correlation key |
| JA4 / JA4S / JA4H / JA4SSH | JA4+JA3 captured, JA4 shown; JA4H/JA4SSH unwired (flowscope gated) | Partial; HTTP/SSH session fingerprints missing |
| RITA robust beaconing | CV beacon only | Misses jittered C2 (flowscope `RitaBeaconDetector` unused) |
| DNS tunneling | DGA scorer only | No distinct-label-cardinality / long-qname detector (`TimeBucketedSet` unused) |
| Newly-observed domains | absent | High-ROI, cheap to add |
| Lateral movement (SMB/RDP/Kerberos) | absent | flowscope parsers exist, unwired |
| Exfil heuristics | elephant flows (not even shown) | No baseline/3σ, no cloud-egress, no DNS-exfil byte volume |
| ATT&CK tagging | absent | Alerts carry no technique IDs |
| Traffic matrix / service map | per-app bandwidth only | No src→dst matrix despite talker histogram |
| Evidence → flow → PCAP pivot | label evidence only | No drill-down, no PCAP export |
| Detection tuning UX | static config allowlist only | No runtime allowlist, no per-detector enable/threshold from GUI |
| Retro-hunt | absent | No historical store to hunt |

#### Sensor / contract proposals

1. **Community ID on every flow & anomaly (S, additive).** Compute the Community ID v1 hash (5-tuple + seed) in `map.rs::flow_record` and attach it as a label on flow records and anomaly alerts. It's ~30 lines (the spec is a sorted-tuple SHA1+base64) and unlocks correlation with Zeek/Suricata/Arkime. Highest leverage-per-line item here.
2. **Wire the unused detectors/parsers behind opt-in `collect.*` flags (M, additive).** Add `RitaBeaconDetector` as `anomalies.rita_beacon` (replacing or complementing CV); add a DNS-tunnel detector using flowscope `TimeBucketedSet` (distinct labels per `(src,SLD)` over a window, plus a max-qname-length gate); enable SMB/RDP/Kerberos parsers → lateral-movement alerts (peer-to-peer SMB to `C$`/`ADMIN$`, reversed-direction RDP). Each is gated and off by default so the capture prefilter stays narrow.
3. **Newly-observed-domain detector (S→M, additive).** A bounded LRU/Bloom of seen SLDs persisted to the GUI's redb store; first sight of a domain in a flow with an outbound connection → `NewlyObservedDomain` anomaly (Info severity, allowlist-friendly). Cheap, high signal.
4. **Exfil heuristic (M, additive).** Per-`(src)` outbound-byte EWMA baseline; raise `DataExfiltration` when a window exceeds baseline + 3σ, or when a single elephant flow targets a known cloud-storage SNI. Reuses the elephant ring already maintained.
5. **MITRE ATT&CK tagging on alerts (S, additive).** Add a `technique` label in `anomaly_alert` / the threat-intel decode: PortScanTRW→T1046, BeaconCv/Rita→T1071, DgaScorer→T1568.002, DNS-tunnel→T1071.004, lateral SMB→T1021.002, RDP→T1021.001, cleartext-snmp→T1078/T1040. flowscope's own docs already carry these IDs (`lib.rs:262` etc.).
6. **Expose `talkers`/`dns`/`http` top-N as a traffic-matrix source (S).** Add a `matrix` query channel keyed by `(src,dst)` byte volume (the histogram is already per-dst; widen the key) to feed a service-map view.
7. **JA4H / JA4SSH (M, compat: needs FoxIO-licensed flowscope `ja4plus` feature).** Gate behind a `ja4plus` cargo feature; surface via `on_http_fingerprint` and the SSH path. Document the license constraint.

#### Frontend proposals

1. **Fetch the orphaned query channels (S):** add `talkers`, `elephant_flows`, `dns?top`, `http?top` to `NetringDetailState`/`fetch.rs` and render top-talker, elephant-flow, top-SLD/NXDOMAIN, and top-host tables. The data already exists server-side; this is pure GUI plumbing.
2. **Per-detector NDR cards with evidence + ATT&CK (S→M):** extend `security.rs::detector_meta` to render a technique badge (linking to the ATT&CK page) and the new detector kinds (Rita, DNS-tunnel, NOD, lateral-SMB/RDP, exfil).
3. **Flow drill-down from an anomaly (M):** in the `security.rs` evidence panel, add a "Show flows" action that issues `@/query/flows` filtered by the anomaly's `src`/Community ID — the analyst pivot SOTA tools center on.
4. **Traffic-matrix / service-map view (M→L):** a new view consuming the `matrix`/`talkers` channel, rendered with the existing canvas/topology layout machinery (`view/topology/`) — src→dst edges weighted by bytes, nodes enriched from the asset inventory.
5. **Asset inventory view (M):** promote assets out of the per-host card into a first-class inventory (sortable by last-seen/vendor/caps), cross-linked to topology and devices — it's the only discovery surface for hosts that emit no telemetry of their own. Render the unused `vendor` field.
6. **Fingerprint explorer (M):** a unified JA4/JA4H/HASSH/QUIC-SNI table with count + first/last-seen and an allowlist toggle — group-by-fingerprint is how analysts hunt actor infrastructure.
7. **Detection-tuning UX (M):** a panel that adds runtime allowlist entries and per-detector enable/threshold via a Zenoh command channel (the netlink sensor already has the `command_key`/`status_key` SetExpectations pattern — reuse it), so tuning doesn't require a config edit + restart.

#### Prioritized shortlist (top-5 highest ROI)

1. **Fetch + render the orphaned `talkers` / `elephant_flows` / `dns` / `http` top-N channels** (S, GUI-only) — the sensor already computes and serves this; it's invisible today. Biggest value-per-effort in the whole section.
2. **Community ID on flows + anomalies** (S) — standards-grade correlation key, trivial to compute, unlocks interop and the flow-pivot.
3. **MITRE ATT&CK tagging on every anomaly + technique badge in `security.rs`** (S) — turns the security view from slugs into an analyst-grade triage surface.
4. **Wire `RitaBeaconDetector` + a `TimeBucketedSet` DNS-tunnel detector + Newly-Observed-Domain** (M) — three high-signal detections using primitives already in the pinned flowscope, closing the biggest SOTA detection gaps.
5. **Flow drill-down pivot from an anomaly (security → `@/query/flows`)** (M) — the central NDR workflow (alert → evidence → flows → PCAP); even without PCAP, alert→flows is the missing link.

(Deferred-but-noted: full PCAP retro-hunt is an L item requiring a packet store; lateral-movement SMB/RDP and JA4H/JA4SSH are strong M items but the latter carries a FoxIO License 1.1 constraint that must be a deliberate feature-gated decision.)

**Sources:** [corelight/community-id-spec](https://github.com/corelight/community-id-spec) · [Security Onion: Community ID](https://docs.securityonion.net/en/2.4/community-id.html) · [FoxIO JA4+](https://github.com/FoxIO-LLC/ja4) · [JA4+ network fingerprinting](https://blog.foxio.io/ja4+-network-fingerprinting) · [Zeek JA4](https://zeek.org/2026/01/how-to-use-ja4-network-fingerprints-in-zeek/) · [Akamai NOD](https://www.akamai.com/blog/security-research/newly-observed-domains-discovered-13-million-malicious-domains) · [Unit 42 malicious NOD](https://unit42.paloaltonetworks.com/malicious-newly-observed-domains/) · [Corelight lateral movement](https://corelight.com/blog/detecting-lateral-movement-and-evasion) · [Arkime](https://github.com/arkime/arkime)

---

## Frontend integration & architecture

### Scope

This section covers how the four first-wave sensors — **sysinfo**, **logs** (syslog + journald), **netlink**, **netring** — integrate into the Iced 0.14 desktop frontend, and the cross-cutting frontend/data-model architecture that binds them.

### Part A — Current state

#### A.1 The data model is a flat, untyped key-value stream

Every sensor emits one shape: `TelemetryPoint { timestamp, source, protocol, metric: String, value: TelemetryValue, labels }` (`zensight-common/src/telemetry.rs:7-26`). `TelemetryValue` is a 5-arm enum — `Counter(u64)`, `Gauge(f64)`, `Text`, `Boolean`, `Binary` (`telemetry.rs:62-82`). There is **no typed sample per sensor**: a netlink socket table, a CPU gauge, and a journald log line are all `metric: String → value`. This is simple and uniform but pushes all structure into stringly-typed `metric` paths and `labels`.

Three orthogonal record types travel beside telemetry:
- **`Alert`** (`alert.rs:90-110`) — a durable, sensor-decided alert with `kind ∈ {Anomaly, Expectation, SensorHealth}`, `severity`, `rule`, `summary`, `labels`, and a firing/resolved `state`. Keyed by a stable FNV-1a `alert_key()` over `source+rule+labels` (`alert.rs:165-180`).
- **`HealthSnapshot` / `DeviceLiveness` / `ErrorReport`** (`health.rs:65-140`) — sensor-self-health and per-device status (`Online/Offline/Degraded/Unknown`).
- **`CorrelationEntry`** (`health.rs:143-155`) — the only cross-sensor join: `ip → {hostnames, sensors, sources}`.

The keyspace (`keyexpr.rs`) is `zensight/<protocol>/<source>/<metric>` for telemetry, with a parallel control plane under `zensight/<protocol>/@/{health,errors,devices/*/liveness,alerts/*,status,alive,query/*}` and `zensight/_meta/{sensors,correlation}/*`. Serialization is JSON or CBOR, auto-detected on decode (`decode_auto`). Sensors use AdvancedPublisher (caching/late-joiner) for telemetry; the control plane uses plain puts.

#### A.2 Subscription: one wildcard in, fan-out by string parsing

`zenoh_subscription` (`subscription.rs:21-225`) opens an `AdvancedSubscriber` on `zensight/**` (history + recovery + late-publisher detection) **plus** a separate plain subscriber on `zensight/*/@/**` — because Zenoh `**` does not cross a verbatim `@` chunk, so health/alerts/liveness need their own subscriber (`subscription.rs:66-78`). Liveliness tokens and an alert late-joiner seed (`zensight/*/@/query/alerts`, `subscription.rs:129-138`) round it out. `decode_sample` (`subscription.rs:297-386`) is a hand-rolled positional key parser that routes each sample to a `Message` variant. The whole feed collapses into the single `ZenSight` god-struct in `app.rs`.

#### A.3 The store is already a good tiered time-series layer — but under-used

`store.rs` is the strongest part of the architecture: a Netdata-style tiered store — hot in-memory `RingBuffer` (per-second, 1h capacity, `DEFAULT_HOT_CAPACITY = 3_600`) + redb-backed warm/cold tiers (per-minute/per-hour, last-observation downsample) keyed by a packed `u128 (metric_id, tier, bucket_ts)` (`store.rs:104-112`). Metric paths are interned to `MetricId(u32)`. Disk I/O is correctly off-thread (`spawn_blocking` + `Task::future`, `app.rs:629-642`). Every numeric point flows through it (`app.rs:1820`), device detail pre-seeds from the warm tier on open (`app.rs:1890-1932`), and dashboard sparklines read the hot ring (`trend.rs build_device_sparks`).

**But:** only `Counter`/`Gauge` are chartable (`store.rs:95-102`, `chart.rs DataPoint::from_telemetry`). Booleans (`iface/*/up`), text (log lines), and the on-demand netring/netlink tables never enter the store, so large swaths of sensor output are snapshot-only with no trend or history.

#### A.4 Per-sensor integration — how each of the four flows in

**sysinfo** is the first-class citizen. CPU/mem/network gauges populate `DeviceState.metrics`, drive dashboard cards + sparklines, become topology nodes (`topology/mod.rs:394-402`), and chart cleanly. This is the "happy path" the rest of the UI was modeled on.

**netlink** is bolted on with effort. It produces both charted gauges (`sockets/tcp/established`, `routes/total`) and on-demand tables (sockets/routes/neighbors) fetched lazily through a `Fetch<T>` state machine (`specialized/netlink_detail.rs`, `specialized/fetch.rs:7-50`) against `@/query/{sockets,routes,neighbors}`. Its sentinel is the only sensor with a **command channel + status queryable** (expectations authoring, `app.rs:1081-1183`). It contributes topology nodes and a kernel-networking panel section (`topology/mod.rs:702-727`).

**netring** is query-channel-heavy: flows, TLS, QUIC, SSH, and asset inventories are all on-demand `Fetch<T>` pulls (`netring_detail.rs`, wired in `app.rs:1197-1251`). Its `FlowRecord`s are the **only source of topology edges** (`apply_flow_edges`, `topology/mod.rs:178-188`) and its anomalies are the sole content of the Security view.

**logs** (syslog/journald) is a third, separate path: log lines bypass the store entirely and land in a bounded `VecDeque<SyslogMessage>` (`MAX_RECENT_LOGS = 5000`, `app.rs:1828-1840`), rendered by a top-level Logs view and a per-host slice in the device view. Filtering is local + a dynamic command to the sensor (`app.rs:1037-1070`). Journald provenance/unit (#64) is surfaced as a per-row badge.

#### A.5 Recurring structural gaps (confirmed in code)

1. **One host = N devices.** `DeviceId = (protocol, source)` (`message.rs`), so a single physical host running sysinfo+netlink+logs+netring appears as **four separate dashboard cards** (`dashboard.rs DeviceState` keyed by `DeviceId`; `handle_telemetry` `app.rs:1842-1856`). Only the topology view re-merges them by `source` into one node (`topology/mod.rs:80-106`). The cross-sensor story the project's own docs promise ("one host = sysinfo+netlink+logs+netring") exists *only* in topology.

2. **No unified incident object.** Alerts are a flat `HashMap<alert_key, Alert>` plus per-key firing→resolved `timelines` (`alerts.rs`). `ExternalIncident` is just a by-`source` rollup for rendering — there is no object spanning alert ↔ device ↔ metric ↔ flow ↔ log. The `InvestigateAlert` message (`app.rs:429-437`) hops alert→device→metric, which is the *only* cross-domain pivot, and it is one-directional.

3. **Snapshot-vs-trend split.** `metrics` is a latest-value snapshot; `history`/store is the trend. Non-numeric values (booleans, logs, tables) have neither trend nor store presence. There is no "make this metric a chart" affordance outside numeric device-detail.

4. **Query channels collected but not driven.** The `Fetch<T>` panels stay `Idle` unless the user clicks a drill-in button; nothing prefetches on device open, and there's no generic framework — each topic is a hand-written `query_netring_*` method (`app.rs:1499-1565`). Six near-identical fetch wrappers exist.

5. **Sysinfo-centric topology + node panel.** Nodes come from sysinfo+netlink *only* (`topology/mod.rs:81`); logs and netring-only hosts never become nodes. The node panel hard-codes sysinfo/netlink metric names (`topology/mod.rs:391-446`). Neighbor edges from netlink (#49) are deferred — edges are netring-flow-only.

6. **Liveness protocol gap (latent bug).** `handle_device_liveness` (`app.rs:1791-1800`) matches only the legacy protocols and `return`s on netlink/netring — those sensors' liveness updates are silently dropped.

7. **God-struct.** `ZenSight` holds ~25 fields and `update()` is a 1000-line match (`app.rs:280-1276`); every view's state, the session, the store, and all transient form state live in one place.

### Part B — State of the art

**Service maps are built from live flows, not config.** Cilium Hubble UI renders services as cards connected by animated edges generated *from live flow data*; denied connections render as red edges; clicking a node shows in/out connections and drills to individual flow records with L7 detail ([Hubble UI service map](https://docs.cilium.io/en/stable/observability/hubble/hubble-ui/), [DeepWiki: Service Map Visualization](https://deepwiki.com/cilium/hubble-ui/3.1-service-map-visualization)). ZenSight's `apply_flow_edges` already follows this exact model — the gap is breadth (only 2 node sources, only netring edges) and L7 drill-down.

**Auto-built dependency maps + SLOs + RCA are the frontier.** Coroot auto-generates a live, dependency-aware service map covering "100% of the system" from eBPF, computes per-service SLIs/SLOs, and ships a single actionable alert per SLO breach *with the inspection results attached* rather than alert spam — its RCA claims to auto-identify 80%+ of issues ([Coroot overview](https://coroot.com/overview), [Coroot RCA blog](https://coroot.com/blog/we-built-ai-powered-root-cause-analysis-that-actually-works/)). The lesson for ZenSight: the map and the incident object should be the *primary* surfaces, derived automatically, with alerts rolled into health, not listed flat.

**RED/USE + hierarchical drill-down.** The canonical pattern (Tom Wilkie's RED for services, Brendan Gregg's USE for resources) is hierarchical dashboards: top-level health → drill to rate/errors/duration or utilization/saturation/errors → correlate to logs/traces via a shared correlation ID ([Better Stack RED/USE](https://betterstack.com/community/guides/monitoring/red-use-metrics/), [Grafana dashboard best practices](https://grafana.com/docs/grafana/latest/visualizations/dashboards/build-dashboards/best-practices/)). ZenSight has the drill primitives (sparkline→chart, alert→device→metric) but no consistent RED/USE framing and no shared correlation key threading metrics↔logs↔flows.

**Incidents are first-class objects with timelines and grouping.** PagerDuty's model: many alerts dedup into one **incident** via a `dedup_key`; time-based grouping folds subsequent alerts into an open incident; each incident has a **Timeline tab** of status transitions and actions; alerts can move between incidents ([PagerDuty Incidents](https://support.pagerduty.com/main/docs/incidents), [Time-Based Alert Grouping](https://support.pagerduty.com/main/docs/time-based-alert-grouping)). ZenSight has the dedup key (`alert_key`) and per-key timelines already — it lacks the incident *aggregate* and any grouping beyond by-source.

**NDR consoles map detections to ATT&CK and lead with triage.** Corelight maps every detection to MITRE ATT&CK, fuses Zeek metadata + Suricata, and offers "one-click pivot to the context required for triage," plus passive asset classification ([Corelight ATT&CK](https://corelight.com/products/overview/mitre-attack), [Corelight use cases](https://corelight.com/products/use-cases/)). ZenSight's Security view groups netring anomalies by detector with evidence drill-down but has **no ATT&CK mapping** and no pivot from anomaly → the flow/host/log evidence.

**Tiered local storage is the right desktop pattern.** Netdata's dbengine stores per-second/per-minute/per-hour tiers with progressive downsampling at ~0.6 bytes/sample, yielding ~14 days/3 months/1+ year on local disk ([Netdata tiered retention](https://www.netdata.cloud/features/dataplatform/tiered-retention/), [Netdata database docs](https://learn.netdata.cloud/docs/netdata-agent/database)). ZenSight's `store.rs` is a faithful 3-tier reimplementation — but lacks retention/eviction (redb grows unbounded) and only stores numerics.

**Iced 0.14** ships reactive rendering, headless testing, smart scrollbars, a `Lazy` widget (cache a subtree by a data dependency), and canvas 2D ([Iced 0.14 release](https://github.com/iced-rs/iced/releases), [iced.rs](https://iced.rs/)). There is **no built-in virtualized table** — data-dense tables (flows, sockets, logs) must be windowed manually, and `canvas::Cache` (already used in topology) is the performance lever for large graphs.

### Part C — Redesign proposal

#### C.1 Gap analysis (summary)

| # | Gap | Evidence | Impact |
|---|-----|----------|--------|
| G1 | One host fragments into N protocol-devices everywhere except topology | `dashboard.rs` keys on `DeviceId=(proto,source)` | Defeats the cross-sensor value proposition |
| G2 | No incident object; alerts are flat + by-source | `alerts.rs ExternalIncident` is render-only | No triage flow, no RCA, alert fatigue |
| G3 | Only numerics are charted/stored; logs/booleans/tables are snapshot-only | `store.rs:95-102`, `chart.rs` | Half the sensor output has no history |
| G4 | Query-channel fetch is per-topic boilerplate, never prefetched | 6× `query_netring_*` in `app.rs` | Drill-ins feel empty/manual; code duplication |
| G5 | Topology nodes = sysinfo+netlink only; node panel hard-codes metrics; no netlink neighbor edges | `topology/mod.rs:81`, `:391-446`, #49 | logs/netring-only hosts invisible in the map |
| G6 | No RED/USE framing, no per-host health score | dashboard is flat cards | No at-a-glance fleet triage |
| G7 | God-struct + 1000-line `update()` | `app.rs` | Every change risks the whole app; hard to test |
| G8 | Security view has no ATT&CK / evidence pivot | `security.rs` | Not competitive as an NDR lens |
| G9 | redb store has no retention | `store.rs` (no eviction path) | Unbounded disk growth |
| G10 | netlink/netring liveness silently dropped | `app.rs:1791-1800` | Wrong device status for new sensors |

#### C.2 Proposals

**P1 — Introduce a `Host` aggregate as the primary entity. (L, breaking)**
Re-key the dashboard on `Host { id: source }` and attach a `HashMap<Protocol, ProtocolFacet>` per host. One card per physical host shows merged identity, a composite health score, and per-facet badges (sysinfo / netlink / logs / netring). `DeviceId` stays as the *facet* key internally and in the keyspace (no wire change). This is the single highest-leverage change: it makes "one host = sysinfo+netlink+logs+netring" true on the main surface, not just topology. *Rationale:* fixes G1, unblocks G6/C.3. *Compat:* GUI state model changes; wire protocol unchanged. Topology's existing `update_from_devices` merge logic (`topology/mod.rs:80-106`) is the template.

**P2 — A unified `Incident` object spanning alert ↔ host ↔ metric ↔ flow ↔ log. (L, additive)**
Define `Incident { id, host, severity, state, started, timeline: Vec<Transition>, alerts: Vec<alert_key>, evidence: Evidence }` where `Evidence` links the offending metric series (`MetricId`), netring flows, and a log slice (by host+time window). Group alerts into incidents using the existing `alert_key` as a dedup key plus time-window grouping (PagerDuty model). The existing per-key `timelines` (`alerts.rs`) become the incident timeline. Provide a single Incident view with a timeline tab and one-click pivots to each evidence type. *Rationale:* fixes G2/G8; turns the app from a dashboard into a triage tool. *Compat:* purely additive over existing `Alert` stream; no sensor change.

**P3 — Universal trend layer: every value chartable + a typed-sample contract. (M)**
Extend the store to accept booleans (as 0/1 step series) and to retain "latest text" for log-rate series, and add a typed `Sample` projection per `TelemetryValue` arm in one place (it already lives in `telemetry_to_f64`, `store.rs:95`). Make any metric in any specialized view click-to-chart (reuse `PromoteMetricToAlert`'s plumbing). Derive a `logs/<unit>/rate` numeric series from the log VecDeque so logs get trends too. *Rationale:* fixes G3; logs/netlink booleans (`iface up`) become trendable (e.g., flap detection). *Compat:* additive; `store.rs` API extension.

**P4 — A declarative query-channel framework with prefetch. (M)**
Replace the six hand-written `query_netring_*` / `query_netlink_detail` methods with one generic `QueryChannel { key, decode, into_message }` registry and a `fetch_on_open` policy per specialized view. On device/incident open, prefetch the relevant channels concurrently. Keep the `Fetch<T>` state machine (`specialized/fetch.rs`) — it's good. *Rationale:* fixes G4; removes ~150 lines of duplication; drill-ins arrive pre-populated. *Compat:* internal refactor, no wire change.

**P5 — Topology as the real service map: all hosts, netlink neighbor edges, L7 drill. (M)**
(a) Make *any* host with telemetry a node (not just sysinfo/netlink) so logs/netring-only hosts appear. (b) Add netlink neighbor adjacency as edges (#49) alongside netring flow edges, distinguishing observed-flow vs L2-adjacency edge types. (c) Click an edge → drill to the netring flow records / netlink neighbor entry (Hubble model). (d) Replace the hard-coded node panel with a facet-driven renderer fed by P1's `Host`. *Rationale:* fixes G5; closes the gap to Hubble/Coroot. *Compat:* internal; reuses `apply_flow_edges`, `canvas::Cache`.

**P6 — Composite host health score + RED/USE fleet view. (M)**
Compute a per-host `HealthScore` from: liveness status, firing-incident max severity, sysinfo saturation (USE), netring error/anomaly rate, and log error rate (RED-ish for logs). Surface it as the dashboard card's primary signal and the topology node tint. Add a fleet "worst-first" overview banding hosts by score. *Rationale:* fixes G6; one number to triage a fleet. *Compat:* additive; pure function over existing state.

**P7 — Security view: ATT&CK mapping + evidence pivot. (S–M)**
Add an optional `attack: Option<AttackTechnique>` to detector metadata (already a `rule→meta` table in `security.rs`), group anomalies by tactic, and make each anomaly row pivot to its `Incident` (P2) / flow evidence. *Rationale:* fixes G8; competitive NDR lens with low effort since the detector table exists. *Compat:* additive label on `Alert.labels` (sensors can populate `attack=` later).

**P8 — Store retention/eviction + chart downsample on read. (S)**
Add a retention policy per tier (drop minute buckets > 30d, hour > 1y) run during the existing flush, and downsample-on-read for wide chart windows. *Rationale:* fixes G9; matches Netdata. *Compat:* internal.

**P9 — Decompose the god-struct. (M, mechanical)**
Split `ZenSight` into a `Model` (data: store, hosts, incidents, sensors) + per-view `ViewState`, and break `update()` into per-domain handlers returning `Task`. *Rationale:* fixes G7; makes P1–P8 tractable and testable. *Compat:* internal.

**P10 — Fix the liveness protocol gap. (S, bugfix)**
`handle_device_liveness` should `protocol_str.parse::<Protocol>()` instead of the hand-rolled match that drops netlink/netring (`app.rs:1791-1800`). *Compat:* none; strict fix.

#### C.3 Proposed information architecture / navigation

Reframe around **hosts and incidents**, not protocols:

```
┌─ Top bar: connection · freshness · global incident badge · Ctrl-K search ───────┐
│ Left rail (persistent shell, already exists in shell.rs):                       │
│   ▸ Overview      — fleet health bands (worst-first), RED/USE summary tiles     │
│   ▸ Hosts         — one card per physical host (P1), facet badges + health score│
│   │    └ Host detail — tabs: Overview · Metrics(charts) · Network(netlink+      │
│   │                    netring facets) · Logs · Incidents — all for ONE host    │
│   ▸ Map           — service map: all hosts, flow + neighbor edges, L7 drill (P5)│
│   ▸ Incidents     — grouped incidents, timeline, triage pivots (P2)             │
│   ▸ Security      — anomaly lens by ATT&CK tactic, pivots to incidents (P7)     │
│   ▸ Logs          — global log stream + filters (keep current)                  │
│   ▸ Sensors       — sensor health/errors (keep current)                         │
│   ▸ Settings                                                                    │
└─────────────────────────────────────────────────────────────────────────────────┘
```

Key shift: **the protocol is a facet of a host, not a top-level navigation axis.** `Expectations` folds into Host detail → Network (it's netlink-scoped). `Topology` is renamed `Map` and promoted. `Device` becomes `Host detail` with per-facet tabs, so the four sensors render as four tabs of one host rather than four cards.

#### C.4 Prioritized shortlist (top 5 highest-ROI cross-cutting changes)

1. **P1 — `Host` aggregate (one card per physical host).** The keystone: without it, every other cross-sensor feature stays bolted-on. Unblocks the map node panel, health score, and incident host-linkage. *(L)*
2. **P2 — Unified `Incident` object + timeline + pivots.** Turns ZenSight from "list of alerts" into a triage tool; reuses the existing `alert_key` dedup and per-key timelines. *(L)*
3. **P3 — Universal trend layer (everything chartable; logs→rate series).** Unlocks history for half the sensor output that is currently snapshot-only; small store extension, large UX gain. *(M)*
4. **P5 — Topology as a real service map (all hosts + neighbor edges + drill).** Closes the gap to Hubble/Coroot and makes netring/netlink data visibly valuable. *(M)*
5. **P9 + P10 — Decompose the god-struct and fix the liveness bug.** Enabling refactor that makes 1–4 safe to build, plus a strict correctness fix for the two newest sensors. *(M + S)*

P4/P6/P7/P8 are strong second-wave items (framework cleanliness, health score, ATT&CK, retention) that ride on the foundations above.

**Sources:** [Coroot overview](https://coroot.com/overview) · [Coroot RCA](https://coroot.com/blog/we-built-ai-powered-root-cause-analysis-that-actually-works/) · [Cilium Hubble UI](https://docs.cilium.io/en/stable/observability/hubble/hubble-ui/) · [Better Stack RED/USE](https://betterstack.com/community/guides/monitoring/red-use-metrics/) · [PagerDuty Incidents](https://support.pagerduty.com/main/docs/incidents) · [Corelight ATT&CK](https://corelight.com/products/overview/mitre-attack) · [Netdata tiered retention](https://www.netdata.cloud/features/dataplatform/tiered-retention/) · [Iced releases](https://github.com/iced-rs/iced/releases)

---

## 6. Market framing (why this matters)

The industry is consolidating observability and network-detection-and-response into
unified platforms: SIEM/SOAR + NDR convergence for full threat-lifecycle management,
AI-assisted alert summarization/triage, and identity-network correlation are the named
2025–26 NDR trends ([Omdia NDR market 2026](https://omdia.tech.informa.com/blogs/2026/may/network-detection-and-response-ndr-market-2026-navigating-xdr-disruption-platform-consolidation-and-ai-driven-renaissance)).
ZenSight already sits at exactly this convergence — host observability (sysinfo, logs,
netlink) **and** passive NDR (netring) in one local-first desktop tool over a Zenoh
bus. The redesign above leans into that: a `Host` that unifies obs + security facets,
an `Incident` object that fuses metric/log/flow evidence, and a service map that merges
flows + adjacency are precisely the "single triage surface" the market is consolidating
toward — differentiated by being agentless-where-possible, local-first, and
privacy-preserving (no cloud egress).

---

## 7. Consolidated sources

**Host / sysinfo:** node_exporter ([README](https://github.com/prometheus/node_exporter/blob/master/README.md), [guide](https://prometheus.io/docs/guides/node-exporter/)) · [OTel system semconv](https://opentelemetry.io/docs/specs/semconv/system/system-metrics/) · [USE method](https://www.brendangregg.com/usemethod.html) / [USE Linux](https://www.brendangregg.com/USEmethod/use-linux.html) · [eBPF observability](https://www.brendangregg.com/blog/2021-07-03/how-to-add-bpf-observability.html) · [runqlat](https://www.brendangregg.com/blog/2016-10-08/linux-bcc-runqlat.html)

**Logs:** [OTel logs data model](https://opentelemetry.io/docs/specs/otel/logs/data-model/) · [OTel general/logs](https://opentelemetry.io/docs/specs/semconv/general/logs/) · [systemd catalog](https://systemd.io/CATALOG/) · [journal-fields(7)](https://man7.org/linux/man-pages/man7/systemd.journal-fields.html) · [Drain3](https://github.com/logpai/Drain3) · [Drain paper](https://netman.aiops.org/~peidan/ANM2023/6.LogAnomalyDetection/phe_icws2017_drain.pdf) · [Loki LogQL metrics](https://grafana.com/docs/loki/latest/query/metric_queries/)

**Netlink:** [Coroot eBPF service map](https://coroot.com/blog/engineering/building-a-service-map-using-ebpf/) · [coroot-node-agent](https://github.com/coroot/coroot-node-agent) · [Cilium Hubble + BCC](https://www.cloudraft.io/blog/ebpf-based-network-observability-using-cilium-hubble) · [eBPF ecosystem 2024-25](https://eunomia.dev/blog/2025/02/12/ebpf-ecosystem-progress-in-20242025-a-technical-deep-dive/) · [Bufferbloat / CAKE](https://www.bufferbloat.net/projects/codel/wiki/Cake/) · [netdev low-latency](https://netdevconf.info/0x17/docs/netdev-0x17-paper19-talk-slides/Low%20Latency%20Life%20Lessons%20Learned.pdf) · [TCP delivery_rate](https://lists.openwall.net/netdev/2016/09/17/42)

**Netring / NDR:** [community-id-spec](https://github.com/corelight/community-id-spec) · [Security Onion Community ID](https://docs.securityonion.net/en/2.4/community-id.html) · [FoxIO JA4+](https://github.com/FoxIO-LLC/ja4) / [blog](https://blog.foxio.io/ja4+-network-fingerprinting) · [Zeek JA4](https://zeek.org/2026/01/how-to-use-ja4-network-fingerprints-in-zeek/) · [Akamai NOD](https://www.akamai.com/blog/security-research/newly-observed-domains-discovered-13-million-malicious-domains) · [Corelight lateral movement](https://corelight.com/blog/detecting-lateral-movement-and-evasion) · [Arkime](https://github.com/arkime/arkime)

**Frontend / architecture:** [Coroot](https://coroot.com/overview) · [Hubble UI](https://docs.cilium.io/en/stable/observability/hubble/hubble-ui/) · [PagerDuty incidents](https://support.pagerduty.com/main/docs/incidents) · [Corelight ATT&CK](https://corelight.com/products/overview/mitre-attack) · [Netdata tiered retention](https://www.netdata.cloud/features/dataplatform/tiered-retention/) · [Iced](https://iced.rs/) · [Omdia NDR 2026](https://omdia.tech.informa.com/blogs/2026/may/network-detection-and-response-ndr-market-2026-navigating-xdr-disruption-platform-consolidation-and-ai-driven-renaissance)
