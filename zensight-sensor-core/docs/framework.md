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
   - `.with_status_publishing()` — publishes running/offline status.
   - `.with_liveliness().await?` — declares a sensor liveliness token for instant
     online/offline detection.
   - `.with_identity(source)` — enables the identity envelope (see below).
   - `.with_artifacts(source_id, producers)` — enables the `@/artifact` channel
     ([artifacts.md](artifacts.md)).
   - `.with_format(format)` — overrides the telemetry serialization format.
3. `runner.spawn(future)` / `spawn_with_error(...)` register worker tasks (tracked
   and aborted on shutdown). `runner.publisher()`, `.health()`, `.session()`,
   `.identity()` hand workers what they need.
4. `run().await` publishes running status, starts the periodic health task,
   starts the identity task (if enabled), then waits for **SIGINT or SIGTERM** —
   catching SIGTERM matters because systemd/docker `stop` send it, and a
   Ctrl+C-only handler would be SIGKILLed after the stop timeout, skipping the
   graceful path (offline status + alert tombstones). On signal it aborts tasks,
   publishes offline status, and closes the session.

```mermaid
stateDiagram-v2
    [*] --> New : SensorRunner::new(name, config)
    New --> Configured : with_status_publishing / with_liveliness / with_identity / with_artifacts / with_format
    Configured --> Configured : spawn(future) / spawn_with_error(name, future)
    Configured --> Running : run() / run_with_metadata(meta)
    Running --> Running : publish running status, health task, identity task
    Running --> ShuttingDown : SIGINT or SIGTERM
    ShuttingDown --> [*] : abort tasks, publish offline status, close session
```

### The identity envelope (`with_identity`)

Enabling identity detects the local `HostIdentity`, stamps `host_id` onto health
snapshots, and once `run()` starts, publishes two docs every 60 s via cached
(late-joiner-seedable) publishers:

- **`SensorInfo`** on `zensight/_meta/sensors/<name>/<source>` — sensor
  registration.
- **self-report `HostEvidence`** (`observer: None`) on
  `zensight/_meta/evidence/host/<name>/<source>`.

Both are published through an `AdvancedPublisherRegistry` set to
`QosClass::Evidence` (reliable, must-arrive). Identity is re-detected every 5th
tick so DHCP address churn is eventually reflected. If `identity.cloud_metadata`
is enabled in config, a one-shot IMDS probe runs before the first emit and its
`CloudFacts` are attached (and preserved across refreshes). `source` is the
sensor's unified host id — the same value used as the telemetry `<source>`
segment and passed to `with_artifacts`.

## Publishers & the declare-all discipline

`publisher.rs` / `advanced_publisher.rs`. The framework never uses one-shot
`session.put`; every write goes through a **declared, cached** publisher so keys
are interned and routing-optimized, and telemetry matches the GUI's
`AdvancedSubscriber`.

`Publisher` has two internal paths:

- **Telemetry** — `publish` / `publish_to_key` / `publish_batch` go through an
  `AdvancedPublisherRegistry` of zenoh-ext *advanced* publishers (per-key cache +
  sample-miss / publisher detection). This pairing with the GUI's
  `AdvancedSubscriber` on `zensight/**` is what gives reliable delivery and
  late-joiner history/recovery.
- **Control plane** — `publish_raw` / `publish_json` / `delete` (for `@/…` keys the
  GUI reads with a plain subscriber) go through a plain `PublisherRegistry` of
  declared publishers. Each call takes an explicit `QosClass` — e.g. alerts use
  `QosClass::Alert` (reliable+block) so a firing/resolved event is never dropped
  on a lossy link; health uses `QosClass::HealthLiveness` (drop-friendly).

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
exception: a **plain** publisher for the opaque `@media` plane (#359) carrying raw
encoded access units with per-frame `Encoding` + attachment, no `TelemetryPoint`
envelope, `QosClass::LiveVideo`, and a `matching_listener` so the sensor can force
a keyframe when a viewer appears.

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
publishing `HealthSnapshot` JSON to `<prefix>/@/health` (the runner does this
every 5 s so the GUI's Sensors view / health bar populate). Errors feed a
**rolling one-hour window** of 60 one-minute buckets, so `errors_last_hour` is a
true sliding count that ages out old failures.

## Alert reporting

`alert.rs` — `AlertReporter` is the sensor-side counterpart to
`zensight_common::Alert`. It owns a `Publisher`, tracks which alerts are firing,
and publishes firing/resolved transitions to
`zensight/<protocol>/@/alerts/<alert_key>` (a `Put(Firing)` to raise/update, then
`Put(Resolved)` + a `Delete` tombstone to clear).

- `observe(alert, for_duration)` — call for each violation this tick. A `for:`
  **debounce** window (settable default via `with_debounce`, or per-observe) means
  an alert must be violated continuously for N before a `Firing` is actually
  published.
- `reconcile(rule, &still_firing_keys)` — after evaluating a rule, resolves any
  alert of that rule no longer in the firing set.
- `with_identity(shared)` — stamps `host.id` as an annotation label on every
  alert. Annotation labels are excluded from `alert_key()`, so stamping never
  changes alert identity (firing/resolve stay matched across identity refreshes).

## Liveness

`liveliness.rs` — `LivelinessManager` declares Zenoh liveliness tokens for
instant presence detection: a sensor token (`zensight/<protocol>/@/alive`,
declared on creation, undeclared on drop) and per-device tokens
(`declare_device_alive` / `undeclare_device` at
`zensight/<protocol>/@/devices/<id>/alive`).

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
- [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) — the full key contract.
