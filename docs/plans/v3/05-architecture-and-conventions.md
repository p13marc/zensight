# Plan v3‑05 — Architecture & conventions (idioms · typing · async · performance)

A cross‑cutting contract every v3 feature plan (01–04) must follow. The goal: code
that is **Rust‑idiomatic, strongly typed, async‑friendly, and performance‑aware**.
Backward compatibility may be broken where it buys a cleaner type or wire model.

> Grounded in verification of the pinned crates (procfs 0.17 PSI/KernelStats are
> typed structs; nlink `events()` is a `Stream`; flowscope detectors are generic
> over the flow key) and standard tokio/Rust practice.

---

## 1. Strong typing

**Pipeline, not stringly‑typed logic.** Every collector follows:
`typed sample struct → pure map fn → TelemetryPoint`. Decode the kernel/crate
types into a plain, owned, unit‑tested sample struct; map to wire points in a pure
`map.rs` function. Never branch on metric‑path strings internally.

- **Lean on the crates' typed APIs** — they're already strong: `procfs::CpuPressure
  { some: PressureRecord { avg10, avg60, avg300, total } }`, `KernelStats { ctxt,
  processes, procs_running, procs_blocked }`, nlink's typed `LinkState`/`Rings`/
  `TcMessage`/`SecurityAssociation`, flowscope's `BeaconScore<K>`/`DnsMessage`. Do
  not re‑parse raw bytes when a typed accessor exists.
- **`enum` over magic values / bools‑that‑mean‑state.** Sample structs use enums
  for closed sets (link duplex, conntrack proto, DNS rcode, end‑reason). Pattern
  on `#[non_exhaustive]` upstream enums with an explicit `_ =>` arm (the conntrack
  `IpProtocol` lesson).
- **Newtypes where a bare scalar invites a bug**, not everywhere. Justified:
  millisecond vs second durations, byte vs packet counters mixed in one fn,
  percent‑as‑0..1 vs 0..100 (the conntrack‑utilization GUI floor bug). Use
  `std::time::Duration` for durations; don't wrap things that are already obvious.
- **Config is typed + `serde`‑derived** with `#[serde(default)]`, never ad‑hoc
  parsing. New collectors add a `collect.<x>: bool` (or a typed sub‑config) with a
  sensible default and graceful absence.
- **The wire stays generic on purpose.** `TelemetryPoint { metric: String, value:
  TelemetryValue }` is the right boundary for a protocol‑agnostic bus — keep it.
  Strong typing lives *on both sides* (sensor sample structs, GUI DTOs in
  `zensight-common`), not in the bus. The `@/query/*` channels already carry typed
  `Vec<Record>` DTOs — extend that pattern (one DTO per detail type in
  `zensight-common`).

## 2. Async

**Reads come in two flavors — treat them differently.**

- **Event streams (preferred for state).** nlink exposes `conn.events()` →
  `impl Stream<Item = Result<NetworkEvent>>`. Consume with `while let Some(ev) =
  events.next().await` inside a dedicated task, *or* fold into the collector loop
  with `tokio::select! { _ = tick.tick() => poll(), ev = events.next() => react() }`.
  This is how Plan 02‑A kills poll latency. netring/flowscope are already
  fully `async`/callback‑driven — keep handlers in their model.
- **Blocking file reads need care.** `/proc` and `/sys` reads (Plan 01) are
  *blocking* `std::fs` I/O. Small fixed reads (`/proc/pressure/*`, `/proc/stat`,
  `/proc/meminfo`) are sub‑millisecond — acceptable inline on the poll tick. But
  **large or unbounded walks** — `/proc/<pid>/*` across all processes, full
  conntrack/socket dumps, cgroup trees — must run under
  `tokio::task::spawn_blocking` so they never stall the runtime. Rule of thumb:
  *bounded constant‑size read inline; per‑entity iteration off‑thread.*
- **Backpressure: bound the hot channels.** The netring drain uses
  `mpsc::unbounded_channel` — under live capture a burst can grow it without limit
  (OOM risk). Move **telemetry** to a bounded channel with explicit
  drop‑oldest + a dropped‑count metric (lossy telemetry is honest — see capture
  self‑health). **Never** make the alert/anomaly channel lossy.
- **Don't hold a lock across `.await`.** The TLS/flow inventory `Mutex`es are
  locked, copied, dropped — keep it that way; snapshot then serialize.

## 3. Performance

- **Cardinality discipline (P2) is the #1 perf lever.** Stream bounded aggregates;
  serve high‑cardinality detail (per‑flow, per‑pid, per‑rule, top‑talkers) via
  `@/query/*`. A metric series per pid/flow/peer is the firehose that kills the bus
  and the GUI.
- **Capture‑path handlers stay allocation‑free and lock‑light.** netring `on_*`
  callbacks run on the capture thread: `AtomicU64::fetch_add`, a short‑held
  `Mutex` push at most, no formatting, no `String` building, no blocking. Defer all
  formatting/serialization to the drain task (already the pattern — preserve it).
- **Avoid per‑tick allocations on poll hot paths.** Reuse a scratch `Vec`/buffer
  across ticks where a collector re‑reads the same shape each interval; prefer
  `&str`/iterators over intermediate `Vec<String>`.
- **Metric keys: revisit only if it shows up.** Per‑point `format!()` of the metric
  path allocates. If profiling shows it hot, intern common prefixes / use
  `Arc<str>` or `Cow<'static, str>`. Do **not** pre‑optimize this — measure first.
- **Atomics over `Mutex` on per‑event counters** (already the netring pattern).
  Reserve `Mutex<VecDeque/HashMap>` for low‑frequency inventories.

## 4. Errors & observability of the sensors themselves

- **`Result`‑returning poll steps** (started in v2 for netlink) so a failed read
  records `record_device_failure` + publishes an `@/errors` report (now wired) —
  extend this to the new collectors. Degrade gracefully: a missing genl family /
  `/proc` file / privilege ⇒ skip that collector, don't crash, don't emit
  misleading zeros.
- **Structured `tracing`** with fields, not string interpolation; warn‑once for
  recurring expected failures (avoid log spam every tick).

## 5. The GUI local time‑series store (Plan 04‑A) — typed + tiered + async

Decision, from research (redb is pure‑Rust, ACID, copy‑on‑write B+trees, stable
on‑disk format, zero‑copy reads; sled is less maintained; SQLite is sync):

- **Hot tier:** a fixed‑size, fixed‑record **ring buffer** in memory (and optionally
  an mmap'd file) for per‑second points — O(1) append, no allocation, bounded.
- **Warm/cold tiers:** periodic downsample (per‑minute / per‑hour) flushed to
  **`redb`** keyed by `(metric_id, tier, bucket_ts)`. The query engine picks the
  tier by zoom range (Netdata model).
- **Async:** the store write/flush runs off the UI thread (a `Task`/dedicated
  thread); reads for charts are zero‑copy from redb or the in‑memory ring. Never
  block Iced's `update`/`view` on disk I/O — use `Task::future` + `spawn_blocking`
  for the redb path.
- **Strong typing:** intern metric paths to a `MetricId(u32)` for compact keys;
  store `Sample { ts: i64, value: f64 }` records; keep the `TelemetryValue` →
  `f64`/flag projection in one typed place.

## 6. Backward compatibility (explicitly allowed to break)

Where a cleaner type/wire model helps, take it — but only with payoff:
- **Worth it:** bounded telemetry channel + dropped‑count (correctness under load);
  `MetricId` interning in the store (perf); typed `@/query/*` DTOs in
  `zensight-common` (shared correctness); the Plan‑05 `telemetry/` vs
  `sensor/<host>/<proto>/` key split if/when picked up.
- **Not worth it:** replacing the generic `TelemetryPoint { metric: String }` bus
  with per‑protocol typed wire enums — it would couple every sensor to the
  frontend and lose the late‑joiner/exporter generality. Keep the bus generic;
  keep the types at the edges.

---

## Adoption

Each feature commit in 01–04 must: use the typed‑sample→pure‑map pipeline,
unit‑test the pure map, pick the right async flavor (stream / inline‑read /
`spawn_blocking`) per §2, respect cardinality (§3), and degrade gracefully (§4).
Wave 1's event‑driven netlink (02‑A) and the bounded‑channel change (§2) are the
two structural items to land early since later work builds on them.
