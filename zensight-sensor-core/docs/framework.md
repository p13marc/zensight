# The sensor framework

Everything a protocol sensor needs except the protocol itself. A sensor's `main`
loads config, builds a `SensorRunner`, spawns protocol workers that publish
`TelemetryPoint`s, and calls `run()`.

## SensorRunner

`runner.rs` — owns the sensor lifecycle:

1. `SensorRunner::new(name, config)` (or `new_with_args`) initializes logging
   from `config.logging()` (with an optional CLI level override), connects to
   Zenoh, creates the telemetry `Publisher`, and sets up the shared
   `SensorHealth` tracker.
2. Optional builders layer on capabilities:
   - `.with_liveliness().await?` — declares the sensor liveliness token *early*.
     `run()` declares it automatically, so this is only needed to reach the
     `LivelinessManager` before `run()` (e.g. for device-level tokens).
   - `.with_identity()` — enables the identity envelope (see below).
   - `.with_artifacts(producers)` — enables the artifact channel
     ([artifacts.md](artifacts.md)).
   - `.with_format(format)` — overrides the telemetry serialization format.
3. `runner.spawn(future)` / `spawn_with_error(...)` register worker tasks (tracked
   and aborted on shutdown). `runner.publisher()`, `.health()`, `.session()`,
   `.identity()` hand workers what they need.
4. `run().await` serves the `@rpc/<producer>/introspect` procedure (this build's
   compiled registry slice), declares the liveliness token *after* the RPC
   queryables so "alive ⇒ callable" holds, starts the periodic health task and
   the identity task (if enabled), then waits for **SIGINT or SIGTERM** —
   catching SIGTERM matters because systemd/docker `stop` send it, and a
   Ctrl+C-only handler would be SIGKILLed after the stop timeout, skipping the
   graceful path (alert tombstones + a clean liveliness close). On signal it
   aborts tasks, **retracts and tombstones every alert still firing**, and only
   then closes the session. `run_with_metadata(meta)` additionally carries
   free-form metadata on the registration doc (`SensorInfo.metadata`).

```mermaid
stateDiagram-v2
    [*] --> New : SensorRunner::new(name, config)
    New --> Configured : with_liveliness / with_identity / with_artifacts / with_format
    Configured --> Configured : spawn(future) / spawn_with_error(name, future)
    Configured --> Running : run() / run_with_metadata(meta)
    Running --> Running : serve introspect, declare alive token, health task, identity task
    Running --> ShuttingDown : SIGINT or SIGTERM
    ShuttingDown --> [*] : abort tasks, close session (alive token undeclared)
```

### The identity envelope (`with_identity`)

Enabling identity detects the local `HostIdentity`, stamps `host_id` onto health
snapshots, and once `run()` starts, publishes two docs every 60 s via cached
(late-joiner-seedable) publishers:

- **`SensorInfo`** on `zensight/v1/<origin>/state/<producer>/sensor` — sensor
  registration. Free-form metadata (from `run_with_metadata`) rides its
  `metadata` field; the retired `@/status` document's running flag is absorbed
  by the health doc.
- **self-report `HostEvidence`** (`observer: None`) on
  `zensight/v1/<origin>/state/<producer>/evidence/self`.

Both are published through an `AdvancedPublisherRegistry` set to
`QosClass::Evidence` (reliable, must-arrive). Identity is re-detected every 5th
tick so DHCP address churn is eventually reflected. If `identity.cloud_metadata`
is enabled in config, a one-shot IMDS probe runs before the first emit and its
`CloudFacts` are attached (and preserved across refreshes). The `<origin>` chunk
is the same `h-<12hex>` host id the evidence claims carry.

## Publishers & the declare-all discipline

`publisher.rs` / `advanced_publisher.rs`. The framework never uses one-shot
`session.put`; every write goes through a **declared, cached** publisher so keys
are interned and routing-optimized, and telemetry matches the GUI's
`AdvancedSubscriber`.

`Publisher` has two internal paths:

- **Telemetry** — `publish` / `publish_to_key` / `publish_batch` go through an
  `AdvancedPublisherRegistry` of zenoh-ext *advanced* publishers (per-key cache +
  sample-miss / publisher detection), keyed under the runner's
  `V1Context::telemetry_prefix()` (`zensight/v1/<origin>/telemetry/<producer>`).
  This pairing with the GUI's `AdvancedSubscriber` on
  `zensight/v1/*/telemetry/**` is what gives reliable delivery and late-joiner
  history/recovery.
- **State plane** — `publish_raw` / `publish_json` / `delete` (for
  `state/<producer>/…` documents the GUI reads with a plain subscriber) go
  through a plain `PublisherRegistry` of declared publishers. Each call takes an
  explicit `QosClass` — e.g. alerts use `QosClass::Alert` (reliable+block) so a
  firing/resolved event is never dropped on a lossy link; health uses
  `QosClass::HealthLiveness` (drop-friendly).

`AdvancedPublisherRegistry`:

- Declares each publisher lazily on first publish to a key and caches it (shared
  across `Publisher` clones).
- `AdvancedPublisherConfig` controls cache size, miss detection + heartbeat, and
  publisher detection. `cache_only(n)` disables miss/publisher detection — cache
  only — so cache-only feeds (identity/evidence) do **not** emit a per-key
  heartbeat, which a low-bandwidth link cannot shed. The default heartbeat is a
  relaxed 5 s (periodic telemetry is superseded by the next sample anyway).
- `with_qos(class)` overrides the class applied to declared publishers (default
  `Telemetry`; the identity task sets `Evidence`).
- `publish_serializable(key, &T)` publishes any serializable control-plane doc
  (e.g. `SensorInfo`, `HostEvidence`) with the same cached late-joiner semantics.

`RawMediaPublisher` (via `Publisher::raw_media_publisher`) is a deliberate
exception: a **plain** publisher for the opaque, verbatim `@media` plane
(`zensight/v1/<origin>/@media/<producer>/<stream>/…`, #359) carrying raw
encoded access units with per-frame `Encoding` + attachment, no `TelemetryPoint`
envelope, `QosClass::LiveVideo`, and a `matching_listener` so the sensor can force
a keyframe when a viewer appears.

Runtime control is not published at all: sensors declare **queryables** on the
`@rpc` plane (`rpc.rs` — `serve` / `serve_topic`, plus `serve_introspect` for the
registry slice). A topic's read is `…/@rpc/<producer>/<topic>`, its write is
`…/@rpc/<producer>/<topic>/set`; failures reply `reply_err` with namespaced
`error/...` names (`RpcError`). RPC queryables are declared before the liveliness
token — alive ⇒ callable. How many queries a producer handles at once, and the
bounds that keep that safe, are below: *Serving `@rpc` — one query at a time*.

## Serving `@rpc` — one query at a time

Every `@rpc` queryable in this workspace handles **one query at a time**. That
is a decision, not an oversight, and it is written down here because from the
outside it is indistinguishable from an accident — which means it could be
"fixed" wrongly, or relied on without anyone knowing it is load-bearing.

### Two shapes, and the second is the sharper one

```rust
// 1. One task per queryable. Serializes queries within one procedure.
while let Ok(query) = queryable.recv_async().await {
    let records = tokio::task::spawn_blocking(move || collect(sel)).await?;
    reply_json(&query, &key, &records).await;
}

// 2. N queryables multiplexed on ONE task. Serializes across *procedures*.
loop {
    tokio::select! {
        q = routes_q.recv_async()  => { … }
        q = sockets_q.recv_async() => { … }
        // …eight more arms
    }
}
```

Shape 2 is the one to keep in mind: `zensight-sensor-netlink/src/query.rs` has
**ten** arms on one task, `zensight-sensor-systemd/src/query.rs` seven, plus
netring's command channels, the correlator and the artifact channel. So on
netlink a socket drill-down does not merely delay the next socket drill-down —
it delays `routes`, `neighbors`, `addresses`, `events`, `route_changes`, `tc`,
`xfrm` and `nft` too.

### Why serial is the right default

A sensor is a guest on a host it is supposed to be *measuring*, not perturbing.
Unbounded concurrency turns a cheap RPC into an amplification vector: N
simultaneous `/proc` walks cost the monitored host more than the telemetry is
worth, and a client storm should not be able to buy that. Serial handling is
natural backpressure with no configuration and no failure mode of its own.

Nothing needs more, either. The GUI issues one drill-down per procedure at a
time, and fleet fan-in is parallel across *hosts* — one query each — which
per-host serial handlers do not impede at all.

### What a caller actually observes

Queries **queue, they do not drop**. `zensight_common::served::serve_queryable`
returns a queryable over Zenoh's default FIFO channel with no bound set, so a
slow handler costs head-of-line *latency*, not lost calls, with the caller's
query timeout as the backstop.

### Rules for handler authors

1. **Bound the cost of one query.** This is the lever that exists, and the
   precedents to copy are `MAX_SEARCH_SCAN` (logs: at most 500 000 redb rows
   scanned per search), `socket_process_max_procs` (netlink: skip attribution
   on a host with too many processes) and logs' early `reply_err` on a bad
   regex — reject cheaply, before the expensive path.
2. **`spawn_blocking` anything blocking.** It protects the runtime's worker
   threads. It does **not** make the loop concurrent — the handler still awaits
   the join before the next query is dequeued, which is the thing most likely
   to be misread here.
3. **Reject bad input before doing the work**, not after.

### The three handlers where this is visible

Most handlers snapshot a lock and copy, so serial costs microseconds. Three do
real work, and their numbers are recorded here so the head-of-line delay is
documented rather than discovered:

| Handler | Cost of one query |
|---|---|
| `sensor-sysinfo/src/query.rs` `processes` | two full `/proc` walks *plus* a `std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL)` — a floor of ~200 ms, by construction |
| `sensor-logs/src/query.rs` `events` (durable path) | a redb range walk, up to `MAX_SEARCH_SCAN` = 500 000 rows |
| `sensor-netlink/src/query.rs` `sockets`/`bandwidth` | a `/proc` fd + cgroupfs walk, which also blocks the nine sibling arms |

> **Deferred: bounded concurrency (#652).** Making handlers concurrent means a
> `tokio::spawn` per query behind a `tokio::sync::Semaphore`, applied first to
> the three handlers above. It is deferred until a real workload complains —
> concretely: an operator drill-down that times out because an *unrelated*
> drill-down on the same host was in flight.
>
> It is not a drop-in, which is the other half of the reason. A `Semaphore`
> only bounds concurrency that already exists; the thing that creates it is the
> spawn, and for the `select!` shape that means every handler's captured state
> (`&conn`, `&route`, `Arc<Manager>`, the eBPF handle) must become
> `Clone + 'static`, reply ordering stops being arrival ordering, and task
> lifetime unbinds from the loop. Nor could the policy be *enforced* where it
> belongs: `serve_queryable` hands back the concrete
> `Queryable<FifoChannelHandler<Query>>`, so callers own their loops, and making
> it policy-carrying would touch all 67 call sites.
>
> Until then the contract above is the contract: **bound the cost of one query,
> not the number in flight.**

## Reconciling `@desired` (#816)

`desired::reconcile_topic` is the consumer half of fleet desired-state: a
controller publishes per-host policy under
`v1/@desired/state/<this-host>/<producer>/<topic>` (see `docs/KEYSPACE.md`
for the full contract), and the sensor converges on it.

```rust
let (marker, _task) = zensight_sensor_core::desired::reconcile_topic(
    session.clone(),
    runner.publisher(),
    DesiredTopic { topic: "expectations", desired_key },   // registry-built key
    config.desired.clone(),                                 // DesiredConfig (kill switch + cadence)
    file_baseline,                                          // what a Delete reverts to
    move |cfg| { let h = handle.clone(); async move {
        validate(&cfg).map_err(|e| e)?;                     // the SAME gate the RPC path runs
        h.replace(cfg).await;
        Ok(())
    }},
);
```

Discipline (each of these is load-bearing):

- **The storage GET is the primary path** — a seed GET at startup plus a
  periodic re-GET (`refresh_secs`) is level-triggered and survives missed
  samples, reconnects, router restarts. The AdvancedSubscriber (history +
  recovery) is the latency accelerator only.
- **LWW by sample timestamp**; replays and re-seeds are idempotent; an
  unstamped sample is refused (it cannot be ordered).
- **Rejection keeps the previous good config** and rides the
  `state/<producer>/applied/<topic>` marker (`AppliedConfig.last_rejected`)
  — on the bus, not only in a log. The marker also says which of the two
  writers (file | desired | rpc) won last; hand the returned `AppliedMarker`
  to your `serve_topic` apply closure so RPC applies stamp `source: rpc`.
- **Kill switch first**: `desired.enabled = false` in file config declares
  nothing and applies nothing — but still publishes the marker
  (`source: file`), because "disabled" must never read as "silent".
- **The marker is seeded, not only published** (#1034): `reconcile_topic`
  declares a queryable on the marker's own state key and answers a GET with the
  last published record, the same RFC 05 §4 shape as the alert seed. Without it
  the marker is a fire-and-forget `put` — read by whoever happened to be
  listening and by nobody else — and every consumer that GETs it (the GUI does,
  in two views) sees silence on a healthy fleet. The seed answers under the
  kill switch too, for the same reason the marker is published under it.
- **The structural never-list**: this module deserializes only your `Doc`
  type and calls only your `apply` — session endpoints/TLS/namespace have no
  writer here.

## Host identity

`identity.rs` — `HostIdentity::detect()` reads the local system:

- `host_id` = `hex(sha256(machine_id + "zensight-host-id-v1"))`. The salt is
  fixed (not configurable) so every ZenSight sensor on a host derives the same
  id; the raw machine-id (confidential per systemd) never leaves the host. `None`
  if `/etc/machine-id` is unreadable.
- `boot_id` from `/proc/sys/kernel/random/boot_id`, `hostname` (+ a dot-heuristic
  `fqdn`), non-loopback/non-link-local `ips` (getifaddrs) and `macs`
  (`/sys/class/net`, `lo` and all-zero skipped), and `container_id` from
  `/proc/self/cgroup`.

`SharedIdentity` is a cheap-to-clone `Arc<RwLock<HostIdentity>>`: `get()` snapshots,
`refresh()` re-detects from files (preserving probed `cloud` facts), `set_cloud()`
attaches the async IMDS result. Detection is fixture-testable via injectable
roots, and the hash is pinned by a test so a scheme change (which would silently
re-identify every host) fails loudly.

## Health reporting

`health.rs` — `SensorHealth` tracks device counts, poll durations, and errors,
publishing `HealthSnapshot` JSON to `zensight/v1/<origin>/state/<producer>/health`
(the runner does this every 5 s so the GUI's Sensors view / health bar populate;
the health doc also absorbs the retired `@/status` running flag). Errors feed a
**rolling one-hour window** of 60 one-minute buckets, so `errors_last_hour` is a
true sliding count that ages out old failures.

**`status` reads the errors, not only the device census** (#1080). The
census — devices responding versus failed — is the verdict for a proxy sensor;
a host sensor with no devices took its final `else` and was `Healthy` with any
number of errors. Now an error not yet followed by a success is `Degraded`,
three in a row with no success between is `Error`, and a success recovers —
the same rule the census applies to one device. `publish_error` counts as an
error (it did not); a device-less collector calls `record_success()` /
`record_error()` directly (netflow does; the others are #1082's task
supervision). The snapshot carries `last_success_unix_ms` and `last_error`, so
"the process is up" and "the process is collecting" are two facts on the wire.

### Self-telemetry (#811)

The health tick's snapshot carries `self_stats` — the fields that let the
platform notice its *own* growth, after a sensor bundle grew 110→355 MB RSS,
was OOM-killed, and reported `Healthy` throughout:

- **rss/vsz/cpu** — self-measured from `/proc/self/{status,stat}` on the 5 s
  tick, never per sample. CPU is a diff against the previous tick (`None` on
  the first — absent is *not measured*, never zero).
- **publish accounting** — every baseline-tier put is counted (messages +
  payload bytes) in the `PublishCounters` shared between the
  `PublisherRegistry` and the health doc; a sensor feeds its own
  `dropped`/`evicted` totals into the same `Arc` (`publisher.counters()`).
  Advanced-tier publications are not counted — understating is permitted,
  the fields are optional.
- **table providers** — `health.register_table_stats(Box::new(|| ...))`
  registers a pull callback reporting `{name, entries, bytes?, capacity_*?}`
  per bounded structure; providers run only on the health tick and **must
  not call back into `SensorHealth`**. This is the field that turns "the
  sensor is big" into "the flow table is 280 MB of it". netring is the
  wired exemplar (flow ring, TLS inventory, asset inventory).
- **budget** — `SensorConfig::budget_bytes()` (e.g. netring's
  `resources.budget_rss_mb`) is carried into `self_stats.budget_bytes`.
  **Declared, not enforced** — the shed ladder is #812.
- **cgroup context** — `memory.{current,max,high}` + OOM counters from the
  sensor's own cgroup-v2, when it runs in one (`max` = unlimited = absent).

When a budget is declared, the runner grades the **`sensor-budget`** rule on
the same measurement it publishes: Warning ≥ 80 %, Critical ≥ 95 %, releasing
under 75 % (hysteresis), with a message that names the largest table. These
alerts ride a runner-owned reporter, so a sensor's own `serve_alerts_query`
seed does not include them (unifying the reporters is #812's business).

Every `self_stats` field is optional and serde-defaulted: an older sensor's
health doc simply has no `self_stats`, and absent always reads as *not
measured*.

### The memory governor and its shed ladder (#812)

#811 taught a sensor to see itself; the governor (`governor.rs`) teaches it
to act. A sensor over its budget **sheds instead of dying** — an agent that
exits under resource pressure removes the evidence at the exact moment it
becomes interesting. The runner drives one ladder step per health tick, on
the same measurement it publishes:

| Step | Meaning |
|---|---|
| 0 | nominal |
| 1 | **Evict** — LRU from the largest registered table first, down to 75 % of budget, then `malloc_trim` so RSS actually returns to the kernel |
| 2 | **Degrade** — registered optional work stopped (`apply(true)`), entered at ≥ 95 % or after 3 stuck ticks of eviction |
| 3 | **Saturated** — everything shed, still over: the loudest possible report |

There is no step 4. Thresholds agree with `budget_level` (80/95/75), so the
`sensor-budget` alert and the ladder describe one condition. Recovery walks
down with hysteresis (< 75 % for 6 ticks per step, restore fanned on leaving
step 2), with a 2-tick cool-down between transitions.

**Futility guard (#864)**: an eviction round that frees under 1 % of its
target (and the target is ≥ 1 MiB — allocator jitter is not evidence) proves
the over-budget RSS is not in the evictable tables (capture rings, allocator
baseline, a mis-sized budget). The ladder then latches *futile*: eviction
stops — the tables keep their data instead of being LRU-wiped every tick —
the ladder climbs to Saturated and holds there, and
`self_stats.ladder.futile` plus a "raise budget_rss_mb" `reason` say why. A
round containing a "cannot say" outcome (entries freed, bytes unreported)
never latches. The latch clears when the budget changes (operator resized —
eviction gets a fresh chance) or RSS drops below the 75 % clear line; normal
hysteresis then walks the ladder down. Thrashing, like dying, is not on the
ladder.

**Sensor wiring** (netring is the exemplar):

- `runner.governor().register_table(TableHandle { name, stats, evict })` —
  `stats` is a cheap occupancy read (joins the health doc's `tables`);
  `evict` frees ~N bytes LRU-first and returns what was *actually* freed.
  A table registers here **or** via `register_table_stats`, never both:
  governor handles may take hot-path mutexes, which the health-lock
  providers' contract forbids.
- `runner.governor().register_degradable(name, apply)` — idempotent
  stop/restore of optional work.
- `runner.with_alert_reporter(reporter)` — hand the runner the sensor's own
  reporter so `sensor-budget` alerts land in the `serve_alerts_query` seed.

**Budget**: config (`SensorConfig::budget_bytes()`) always wins; with none
declared, the first tick discovers `cgroup memory.max ×
CGROUP_BUDGET_FRACTION` (0.75) — the container default, so the ladder cannot
disagree with the operator's drop-in. No budget and no cgroup limit = the
ladder never arms and nothing changes.

**Reporting**: the ladder's state (`self_stats.ladder`: step, the `futile`
latch, cumulative per-table evictions, degraded list, a human `reason`)
publishes every tick —
a silently degraded sensor is a lying sensor. From step 2 the health
`status` itself upgrades to `Degraded` (`ladder_status`; step 1 is normal
operation and deliberately does not recolor the fleet card).

## Alert reporting

`alert.rs` — `AlertReporter` is the sensor-side counterpart to
`zensight_common::Alert`. It owns a `Publisher`, tracks which alerts are firing,
and publishes firing/resolved transitions as LWW state to
`zensight/v1/<origin>/state/<producer>/alert/<alert_key>` (a `Put(Firing)` to
raise/update, then `Put(Resolved)` + a `Delete` tombstone to clear).

- `observe(alert, for_duration)` — call for each violation this tick. A `for:`
  **debounce** window (settable default via `with_debounce`, or per-observe) means
  an alert must be violated continuously for N before a `Firing` is actually
  published.
- `reconcile(rule, &still_firing_keys)` — after evaluating a rule, resolves any
  alert of that rule no longer in the firing set.
- `Hysteresis::{Level, Edge}` — **how a rule spends its `for:`** (#1084), and
  the two halves of that decision (`for_duration()` for `observe`,
  `reconcile_opts()` for `reconcile_opts`) come from one value so they cannot
  be paired wrongly.

  A **level** rule's condition persists while it is true — RSS over a budget, a
  unit failed — so `for` debounces it. An **edge** rule's condition is a
  per-tick counter delta (`oom_kill_delta > 0`, `media_errors_delta > 0`) and is
  true for exactly one tick; `observe` sets `first_seen` on the very call that
  evaluates `now - first_seen >= dur`, so at any non-zero `for` the test is
  `0 >= dur` and the next `reconcile` drops the entry unpublished. An operator
  who set `for_secs: 60` to stop pressure alerts flapping had silently turned
  OOM alerting off. For an edge rule `for` is a **hold** instead: raised on the
  first observation, held firing that long after the last one.

  Nothing new sits underneath it — `retire`'s recovery window already *is* hold
  semantics. One rounding to know: `retire` is sweep-driven and the reporter
  owns no timer, so an edge alert resolves at the first reconcile **after** the
  hold elapses.
- **A firing alert's content is refreshed** (#1081), rate-limited by
  `with_content_refresh` (30 s default). A summary that drifts inside one
  severity band is republished on the same key, with `timestamp` — the
  *transition* clock an ack and the historian's timeline uid both read —
  carried over unchanged and the fresh reading in `observed_at_ms`.
- `with_identity(shared)` — stamps `host.id` as an annotation label on every
  alert. Annotation labels are excluded from `alert_key()`, so stamping never
  changes alert identity (firing/resolve stay matched across identity refreshes).
- `serve_alerts_query(reporter)` — the late-joiner seed: a queryable on the alert
  **state selector** (`state/<producer>/alert/*`) that answers a plain GET with
  one reply per firing alert on its concrete key — exactly the storage-shaped
  answer a router latest-value store would give, so seeding works with or
  without one.

### The firing set outlives the process (#882)

A firing alert is a claim this producer is making, at a key only this producer
writes. `reconcile` retracts it when the condition clears — but only while the
process that raised it is still running. A restart begins with an empty set, so
an alert that was firing beforehand and is no longer true is never fired again
*and therefore never resolved*: the `Firing` document is abandoned. Without a
storage nobody notices; with a `latest` storage on `v1/*/state/**` it is
durable and served to every late joiner forever.

So the reporter owns both ends of its own lifetime, and the runner drives both
for any reporter handed to it with **`with_alert_reporter`** — which is also
what declares `serve_alerts_query`, so one registration replaces three rituals:

| End | What happens | Covers |
|---|---|---|
| start | `adopt_persisted` GETs this producer's own alert selector and takes ownership of what it finds | SIGKILL, OOM, panic, and a restart whose config no longer defines the target |
| stop | `resolve_all` retracts and tombstones everything still firing, before the session closes | `systemctl stop`, `docker stop`, Ctrl+C |

Adopted alerts enter the active set **already published**, which is the truth —
they *are* published, by us, at that key. From there the ordinary sweep finishes
the job: the first `reconcile` of each rule retracts what is no longer violated,
and re-`observe`ing what still is publishes nothing, because key and severity are
unchanged. There is no new lifecycle state, and no storage means an empty answer
and today's behaviour exactly.

Two shapes are retired on the spot rather than adopted, because no sweep can
ever reach them: a `Resolved` document whose `Delete` was lost, and a document
whose key does not match the `alert_key` its own payload derives — the #737
re-key stranding, which a producer can now clear for itself.

## Rates from counters — `CounterTracker` (#1152)

`rate.rs` — `CounterTracker<K>::observe(key, value, Instant) -> Option<Rate>`.
One derivation, for every sensor that turns a cumulative counter into a rate.

It exists because five crates kept their own previous-value map and their own
delta arithmetic, and no copy had both halves:

| Copy | Measured elapsed | Reset | Wrap |
|---|---|---|---|
| `netlink/bandwidth.rs` | yes | yes | n/a |
| `systemd` IPAccounting | yes | yes | n/a |
| `snmp/rate.rs` | yes | re-baseline | yes |
| `sysinfo/collector.rs` (×4) | **no** | **no** | n/a |

The sysinfo row is the bug (#1069): the poll loop runs
`collect_and_publish().await` and *then* sleeps the configured interval, so the
true period is `interval + collection_time` — and every rate divided by the
nominal one. Under load a 5 s tick takes 12 s and `rx_rate` reads 2.4× the
truth. The sensor already *measured* the error (`record_poll_duration`) and
published it without using it.

Three rules, each with a failure behind it:

- **The instant travels with the sample.** A rate divided by what the scheduler
  was *asked* for is not wrong by a little under load; it is wrong by exactly
  the amount that makes the load interesting.
- **A backwards step yields no rate, and re-baselines.** The sample is stored
  either way, so the *next* observation has a baseline — a reset that silently
  kept the old one publishes one enormous rate and then looks correct forever,
  which is the failure hardest to notice.
- **A wrap is only decodable if the width is declared.** `CounterWidth::Bits32`
  decodes a backwards step as one modular wrap; nothing in the arithmetic can
  tell one wrap from three, so `max_plausible_rate(elapsed)` gives the caller
  the ceiling and leaves the judgement to whoever knows the link speed.

## Liveness

`liveliness.rs` — `LivelinessManager` declares Zenoh liveliness tokens for
instant presence detection: a sensor token
(`zensight/v1/<origin>/state/<producer>/alive`, declared on creation, undeclared
on drop) and per-device tokens (`declare_device_alive` / `undeclare_device` at
`zensight/v1/<origin>/state/<producer>/device/<id>/alive`).

The sensor token is **not optional**: `run()` declares it automatically if no
builder did, because the frontend flips the sensor's card to **Offline** when
the token disappears (clean shutdown deletes it; a crash drops the session and
the DELETE propagates on transport loss or lease expiry). Without a token a
dead sensor would keep its last reported health forever.

## Process identity & scrubbing

- `procutil.rs` — the shared `/proc/<pid>/*` parsers. Process identity across
  ZenSight is the `(pid, start_time)` pair (bare PIDs get reused);
  `proc_start_time_ticks(pid)` reads `/proc/<pid>/stat` field 22 (robust against a
  `comm` containing spaces/parens by resuming after the last `)`), matching
  nlink's `start_time` byte-for-byte so cross-sensor joins need no conversion.
  `proc_cgroup_v2(pid)` reads the `0::<path>` unified cgroup — the join key to
  systemd units.
- `scrub.rs` — `ArgScrubber` redacts secret **values** in process argv before a
  cmdline leaves the host (both `--key value` and `key=value` shapes), matching a
  default sensitive-key list plus user `*`-glob words. `CMDLINE_CAP_BYTES` bounds
  published cmdlines. Complements `redact` (the JSON-config-key equivalent).

## See also

- [Artifacts](artifacts.md) — on-demand large-data transfer.
- [`zensight-common` data model](../../zensight-common/docs/data-model.md) — the
  wire types published here (`TelemetryPoint`, `Alert`, `QosClass`).
- [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) — the deployed key contract
  (normative spec: [`../docs/rfcs/keyspace-v2/`](../../docs/rfcs/keyspace-v2/00-index.md)).
