# ZenSight Zenoh Keyspace Reference

This is the canonical reference for every Zenoh key expression ZenSight uses.
All sensors, exporters, and the frontend follow these conventions; new code MUST
build keys through the shared helpers listed in [§7](#7-key-building-helpers)
rather than ad-hoc `format!()`.

The single root is `zensight/`. Everything below it is either **telemetry**
(`zensight/<protocol>/<source>/…`), **control-plane** for one sensor
(`zensight/<protocol>/@/…`), or cross-sensor **metadata** (`zensight/_meta/…`).

> **`@` is special in Zenoh.** A key chunk starting with `@` is matched
> *verbatim*: the wildcards `*` and `**` do **not** cross into it. So
> `zensight/snmp/**` matches telemetry but **not** `zensight/snmp/@/alerts/…`.
> Control-plane consumers must name the `@/…` keyspace explicitly (see
> [§5](#5-wildcards--subscriptions)).

---

## 1. Protocols

`<protocol>` is one of the sensor protocols. Each sensor owns the subtree
`zensight/<protocol>/`. The default `key_prefix` is `zensight/<protocol>`.

| Protocol | Sensor crate | Source identifier |
|----------|--------------|-------------------|
| `snmp`    | zensight-sensor-snmp    | device name |
| `logs`  | zensight-sensor-logs  | hostname (network or journald) |
| `netflow` | zensight-sensor-netflow | exporter name |
| `modbus`  | zensight-sensor-modbus  | device name |
| `sysinfo` | zensight-sensor-sysinfo | hostname |
| `gnmi`    | zensight-sensor-gnmi    | device name |
| `netlink` | zensight-sensor-netlink | hostname |
| `netring` | zensight-sensor-netring | sensor id |
| `systemd` | zensight-sensor-systemd | hostname |

---

## 2. Telemetry — `zensight/<protocol>/<source>/<metric>`

The universal pattern. `<metric>` may contain `/` (it is a path), so a key can
have more than four chunks.

```
zensight/snmp/router01/system/sysUpTime
zensight/logs/web01/events/0001700000000000000000042   # per-line event, metric=events/<uid>
zensight/sysinfo/server01/cpu/usage
zensight/netflow/exporter01/10.0.0.1/10.0.0.2
zensight/modbus/plc01/holding/temperature
zensight/gnmi/router01/interfaces/interface[name=eth0]/state/counters
zensight/netlink/host01/sockets/tcp/established
zensight/netlink/host01/events/ipsec/changed_total      # XFRM monitor: SA/policy lifecycle (nlink 0.23)
zensight/netlink/host01/ethtool/eth0/fec/modes          # ethtool FEC mode(s) (nlink 0.23)
zensight/netring/sensor01/flow/by_l4/tcp/bytes_total
zensight/systemd/host01/units/failed                    # unit-state aggregate
zensight/systemd/host01/boot/total_usec                 # boot-performance phase (like systemd-analyze)
zensight/systemd/host01/unit/sshd.service/active        # watched-unit telemetry (unit label carries raw name)
zensight/systemd/host01/mounts/mounted                  # opt-in mount roll-up (collect.mounts)
zensight/systemd/host01/journal/disk_usage_bytes        # opt-in journal health (collect.journal)
```

Payload: a serialized [`TelemetryPoint`] (JSON or CBOR per the sensor's
`serialization` config). Built via [`KeyExprBuilder::build(source, metric)`].

> **Logs are per-line events** (#104). The logs sensor keys every line under a
> unique `events/<uid>` metric — `<uid>` is `<timestamp_ms><seq>` (zero-padded,
> time-sortable). This replaced the old `<facility>/<severity>` metric, where
> every key was overwritten by the next line of the same severity (last-writer-
> wins lost all history). Facility/severity and the OpenTelemetry logs data model
> (`severity_number` 1–24, `severity_text`, `log.record.uid`, and
> `log.record.original` when raw is kept) now travel in **labels**, not the key.
> Because each line is unique text, these points feed the GUI's rolling log buffer
> only — they are excluded from per-metric device state, the numeric local store,
> and the Prometheus exporter (cardinality), while the OTel exporter maps them to
> log records.

> **Published with a zenoh-ext `AdvancedPublisher`** (per-key cache + miss/
> publisher detection), so it pairs with the GUI's `AdvancedSubscriber` on
> `zensight/**` (history + recovery). The control-plane below uses plain
> `put`/`delete` and a plain subscriber. See
> [Architecture → Zenoh Transport & Pub/Sub Model](ARCHITECTURE.md#zenoh-transport--pubsub-model).

---

## 3. Control-plane — `zensight/<protocol>/@/…`

Per-sensor operational channels. All are derived from the sensor's `key_prefix`.

| Key | Direction | Payload | Emitted by |
|-----|-----------|---------|------------|
| `@/health` | put | `HealthSnapshot` | every sensor (`SensorRunner`) |
| `@/errors` | put | `ErrorReport` | every sensor (`HealthReporter`) |
| `@/status` | queryable | status JSON | every sensor (`StatusPublisher`) |
| `@/alive` | liveliness token | — | every sensor (`LivelinessManager`) |
| `@/devices/<device>/liveness` | put | `DeviceLiveness` | sensors with per-device tracking |
| `@/devices/<device>/alive` | liveliness token | — | sensors with per-device tracking |
| `@/alerts/<alert_key>` | put / delete | `Alert` (firing → resolved → tombstone) | snmp, logs, netlink, netring |
| `@/query/alerts` | queryable | `Vec<Alert>` (current firing set) | sensors with alerts (late-joiner seed) |
| `@/commands/<topic>` | subscribe | topic command | sensors with runtime control |
| `@/status/<topic>` | queryable | topic status | sensors with runtime control |
| `@/query/<topic>` | queryable | topic detail (`Vec<Record>`) | netlink, netring |
| `@/artifact/request` | subscribe | `ArtifactRequest` (operator-initiated) | sensors with artifact support |
| `@/artifact/status` | queryable | `ArtifactStatus { kinds: Vec<KindStatus> }` (one per kind) | sensors with artifact support |
| `@/artifact/cancel` | subscribe | artifact id (ULID) — free an in-flight/ready artifact early | sensors with artifact support |
| `@/artifact/blob/<id>/**` | queryable | `Manifest` + chunk bytes (`zenoh-blob`) — Tier-1 `Blob` delivery | sensors with artifact support |
| `@/store/<algo>/<hash>` | queryable | content-addressed chunk bytes (`zenoh-blob` Tier-2, immutable ⇒ cacheable fleet-wide) — `Tree` delivery | sensors with artifact support |
| `@/tree/<id>` | queryable | a `TreeIndex` (depth-first `Entry` list) — Tier-2 `Tree` delivery | sensors with artifact support |

`<alert_key>` is a stable hash of `source + rule + sorted-labels`
([`Alert::alert_key`]) so the same logical alert always maps to the same key
(firing and resolving are state transitions on one key, not new keys).

### 3.1 Control topics in use

| Sensor | `@/commands/<topic>` · `@/status/<topic>` | Purpose |
|--------|---|---|
| logs | `filter` | add/remove/clear dynamic message filters |
| netlink | `expectations` | hot-swap sentinel expectations |
| netlink | `collection` | toggle collectors at runtime |
| netring | `detectors` | runtime detection tuning: allowlist + per-detector mute/threshold |
| systemd | `expectations` | hot-swap sentinel expectations (service/target/timer/restart-rate/forbid-failed) |
| systemd | `action` | gated service control `{verb,unit}` (start/stop/restart/reload) — **default OFF**, allowlist + polkit |

### 3.1a Large-data artifacts — `@/artifact/*`

One unified channel (release 0.7.0) for all on-demand large-data transfer,
provided framework-wide by `zensight-sensor-core` (like `@/health`). It replaced
the former separate `@/report/*` (Tier-1 file blob) and `@/snapshot/*` (Tier-2
directory tree) channels — see the migration note in `CHANGELOG.md`. Each sensor
opts in per **artifact kind**; every kind is disabled by default.

**Lifecycle.** An operator PUTs an `ArtifactRequest { id, kind, opts }` to
`@/artifact/request`. `ArtifactKind` (serde-tagged `"kind"`) is one of:

| Kind | Payload | Notes |
|------|---------|-------|
| `Report {}` | — | redacted `tar.zst` debug bundle (config + health + counters) |
| `Snapshot { dir }` | `dir` = an **allowlisted logical name** | `dir` is the authz boundary — never a raw path |
| `Capture { duration_secs, max_bytes, filter, snaplen, compress }` | — | **planned / reserved** for the netring pcap-capture feature (issue #333) |
| *(unknown)* | — | forward-compat `Unsupported` — a sensor replies `Failed` for kinds it doesn't implement |

The sensor generates the artifact off-thread and exposes progress via the
`@/artifact/status` queryable, which returns an `ArtifactStatus { kinds:
Vec<KindStatus> }` — **one `KindStatus` per artifact kind the sensor produces**
(`{ kind, busy, current: Option<ArtifactState>, max_bytes, cooldown_secs, advert:
KindAdvert }`). `ArtifactState` (tagged `"state"`) is `Generating { …, progress }`
→ `Ready { …, delivery, expires_ms }` → `Failed { …, reason }` / `Expired { … }`.
An operator can PUT an artifact id (ULID) to `@/artifact/cancel` to free an
in-flight or ready artifact early (the TTL is the backstop otherwise).

**`Delivery` decides Blob vs Tree.** The `Ready` state carries a `Delivery`
(tagged `"delivery"`) that tells the client how to pull the bytes:

- `Blob { manifest: Manifest, blob_prefix }` — **Tier-1** whole-file blob. The
  client pulls via `BlobClient` from a [`zenoh-blob`](../zenoh-blob) server under
  `@/artifact/blob/<id>/**` (manifest + chunk replies, with progress / SHA-256
  integrity / range-resume). The blob lives under its own `blob/` segment so its
  `…/blob/**` queryable never collides with `…/artifact/status` or
  `…/artifact/request`.
- `Tree { tree_id, store_prefix, tree_prefix, summary: TreeSummary }` — **Tier-2**
  content-addressed directory tree. The client pulls via `TreeClient`: GET the
  `TreeIndex` (depth-first `Entry` list) from `@/tree/<id>`, compute `missing =
  needed − have` against its local [`ContentStore`] (ZenSight backs this with a
  redb `chunks` table — `RedbContentStore` in `zensight/src/store.rs`), fetch only
  the missing chunks from `@/store/<algo>/<hash>` (re-hashing each on receipt),
  reconstruct the tree (mode/symlinks), and verify the root hash. Resume *is*
  "which hashes are already on disk", so it survives reconnect **and** restart for
  free. `@/store` / `@/tree` are **kind-agnostic Tier-2 delivery infra** — shared
  by any kind whose producer emits a `Tree` delivery. Chunk boundaries can be
  fixed-size or content-defined (FastCDC, for cross-version dedup); the client
  never re-chunks (it fetches by hash). Chunks/index can also be PUT into a
  **router-hosted Zenoh storage** so a producer publishes and exits and chunks
  dedup fleet-wide — see `docs/BLOB-ROUTER-STORAGE.md`.

**Producers.** sensor-core owns one `ArtifactChannel` (request/status/cancel +
reaper, per-kind busy + cooldown, lazy `BlobServer`/`TreeServer`). Each supported
kind is an `ArtifactProducer` (the `Snapshot` producer advertises its allowlisted
`dirs` via `KindAdvert::Snapshot { dirs }`; the GUI hides `KindAdvert::Unknown`
kinds). The GUI surfaces available kinds/dirs for download in the Sensors view.
See `docs/LARGE-DATA-TRANSFER.md`.

### 3.2 On-demand detail queries — `@/query/<topic>`

High-cardinality detail is **served on request, never streamed** onto the
telemetry bus (principle: keep the bus low-cardinality). Parameters are passed
as Zenoh selector params (e.g. `?top=20`, `?state=&port=`).

| Sensor | `@/query/<topic>` | Reply |
|--------|---|---|
| sysinfo | `processes?sort=cpu\|mem\|io&top=N`, `latency`² | `Vec<ProcessRecord>` / `LatencyReport` |
| netlink | `routes`, `neighbors`, `sockets?state=&port=`, `addresses`, `events`, `route_changes`, `tc`, `xfrm`, `nft`, `retransmits`³, `connections`³ | `Vec<…Record>` |
| netring | `flows`, `tls`, `talkers?top=N`, `matrix?top=N`, `elephant_flows`, `dns?top=N`, `http?top=N`, `quic`, `ssh`, `ja4h?top=N`¹, `assets` | `Vec<…Record>` |
| systemd | `units`, `failed`, `unit?name=<u>`, `timers`, `events`, `cgroups?path=<rel>` | `Vec<UnitRecord>` / `UnitDetail` / `Vec<TimerRecord>` / `Vec<EventRecord>` / `CgroupNode` |

Note: sysinfo's `@/query/*` keys carry the `<hostname>` segment
(`zensight/sysinfo/<host>/@/query/<topic>`), unlike netlink/netring which use the
host-less `command::query_key` form.

¹ `ja4h` (JA4H HTTP fingerprints, #124) is only served when the netring sensor is
built with `--features ja4plus` (FoxIO License 1.1) and `collect.http_fp` is set;
otherwise the channel is absent and the reply empty. JA4SSH is not yet available
upstream — SSH is fingerprinted via HASSH on the `ssh` channel.

² `latency` (eBPF runqlat + biolatency saturation histograms, #99) is only served
when the sysinfo sensor is built with `--features ebpf` and `collect.ebpf` is set,
and the process holds CAP_BPF/CAP_PERFMON; otherwise the reply is a
`LatencyReport` with `available: false`.

³ `retransmits` / `connections` (eBPF connlat/retransmit/tcplife, #114) are only
served when the netlink sensor is built with `--features ebpf`, `collect.ebpf` is
set, and the process holds CAP_BPF/CAP_NET_ADMIN; otherwise the channels are
absent.

---

## 4. Metadata — `zensight/_meta/…`

Cross-sensor, protocol-independent registries.

| Key | Payload | Emitted by |
|-----|---------|------------|
| `zensight/_meta/sensors/<name>` | `SensorInfo` (registration/discovery) | every sensor |
| `zensight/_meta/correlation/<ip>` | `CorrelationEntry` (which sensors see a device) | sensors with correlation |

---

## 5. Wildcards & subscriptions

| Wildcard | Used by | Catches |
|----------|---------|---------|
| `zensight/**` | frontend (history sub), exporters | all telemetry *and* `_meta` (but **not** `@/…`) |
| `zensight/*/@/**` | frontend | all control-plane (health/errors/alerts/liveness) |
| `zensight/*/@/alive` | frontend | sensor liveliness tokens |
| `zensight/*/@/devices/*/alive` | frontend | device liveliness tokens |
| `zensight/*/@/query/alerts` | frontend (GET at startup) | firing-set seed for late joiners |
| `zensight/<protocol>/@/alerts/**` | any alert consumer | one sensor's alerts (note explicit `@`) |
| `zensight/*/@/alerts/*` | exporters (`export_alerts`) | all sensors' alerts, mirrored to Prometheus/OTel |
| `zensight/_meta/sensors/*` | frontend | sensor registrations |
| `zensight/_meta/correlation/*` | frontend | device correlations |

Exporters (`prometheus`, `otel`) subscribe to `zensight/**` and **skip**
control/metadata by filtering keys containing `/@/` or starting with
`zensight/_meta/` — only true telemetry is exported. With `export_alerts` on
(the default) each exporter **additionally** declares a second subscriber on
`zensight/*/@/alerts/*` (`all_alerts_wildcard()`) to mirror firing alerts —
necessary precisely because `zensight/**` does not cross the `@` chunk.

---

## 6. Exporter semconv mapping — `zensight-common::semconv` (#100)

Wire keys stay ZenSight-internal; the **exporters** map the core sysinfo host
metrics to the OpenTelemetry host-metrics semantic conventions via **one shared
table** (`zensight_common::semconv`), so exported metrics are dashboard-portable.
State/direction/device/cpu are factored out of the name into attributes (OTel) /
labels (Prometheus). Keys without a standard equivalent keep the raw
`zensight.<protocol>.<metric>` (otel) / `<prefix>_<protocol>_<metric>` (prom) name.

| Internal key | OTel metric | Attributes |
|--------------|-------------|------------|
| `cpu/usage`, `cpu/<n>/usage` | `system.cpu.utilization` | `cpu=<n>` |
| `load/{1m,5m,15m}` | `system.cpu.load_average.{1m,5m,15m}` | — |
| `memory/{used,cached,buffers,available}` | `system.memory.usage` | `state={used,cached,buffered,free}` |
| `memory/total` | `system.memory.limit` | — |
| `memory/usage_percent` | `system.memory.utilization` | — |
| `memory/swap_used` | `system.paging.usage` | `state=used` |
| `memory/paging_{in,out}_total` | `system.paging.operations` | `direction={in,out}` |
| `memory/page_faults_major_total` | `system.paging.faults` | `type=major` |
| `network/<if>/{rx,tx}_bytes` | `system.network.io` | `device=<if>`, `direction={receive,transmit}` |
| `network/<if>/{rx,tx}_{packets,errors,dropped}` | `system.network.{packets,errors,dropped}` | `device`, `direction` |
| `disk/<dev>/io/{read,write}_bytes` | `system.disk.io` | `device=<dev>`, `direction={read,write}` |
| `disk/<dev>/io/{read,write}_ops` | `system.disk.operations` | `device`, `direction` |
| `disk/<dev>/{used,available}` | `system.filesystem.usage` | `device`, `state={used,free}` |
| `disk/<dev>/usage_percent` | `system.filesystem.utilization` | `device` |

Values pass through unchanged (utilization stays the sensor's 0–100 percent, not a
0–1 ratio) — the table maps metric *identity*, not units.

---

## 6. Full tree at a glance

```
zensight/
├── <protocol>/
│   ├── <source>/<metric…>              # telemetry  (TelemetryPoint)
│   └── @/
│       ├── health                      # HealthSnapshot
│       ├── errors                      # ErrorReport
│       ├── status                      # queryable
│       ├── alive                       # liveliness token
│       ├── devices/<device>/liveness   # DeviceLiveness
│       ├── devices/<device>/alive      # liveliness token
│       ├── alerts/<alert_key>          # Alert (firing/resolved)
│       ├── query/alerts                # firing-set seed (queryable)
│       ├── query/<topic>               # on-demand detail (queryable)
│       ├── commands/<topic>            # runtime control (sub)
│       ├── status/<topic>              # control status (queryable)
│       ├── artifact/                   # on-demand large-data artifacts (opt-in per kind)
│       │   ├── request                 # ArtifactRequest (sub)
│       │   ├── status                  # ArtifactStatus{ kinds } (queryable)
│       │   ├── cancel                  # free artifact early (sub)
│       │   └── blob/<id>/**            # Manifest + chunks — Blob delivery (zenoh-blob queryable)
│       ├── store/<algo>/<hash>         # content-addressed chunks — Tree delivery (queryable)
│       └── tree/<id>                   # TreeIndex — Tree delivery (queryable)
└── _meta/
    ├── sensors/<name>                  # SensorInfo
    └── correlation/<ip>                # CorrelationEntry
```

---

## 7. Key-building helpers

Do not hand-write keys. Build them through these so the conventions stay
enforced and a single change propagates everywhere.

| Helper | Location | Produces |
|--------|----------|----------|
| `KeyExprBuilder::build(source, metric)` | `zensight-common/src/keyexpr.rs` | `zensight/<proto>/<source>/<metric>` |
| `KeyExprBuilder::status_key()` | `zensight-common/src/keyexpr.rs` | `…/@/status` |
| `KeyExprBuilder::alert_key_expr(key)` | `zensight-common/src/keyexpr.rs` | `…/@/alerts/<key>` |
| `command::command_key(prefix, topic)` | `zensight-common/src/command.rs` | `…/@/commands/<topic>` |
| `command::status_key(prefix, topic)` | `zensight-common/src/command.rs` | `…/@/status/<topic>` |
| `command::query_key(prefix, topic)` | `zensight-common/src/command.rs` | `…/@/query/<topic>` |
| `command::artifact_request_key(prefix)` | `zensight-common/src/command.rs` | `…/@/artifact/request` |
| `command::artifact_status_key(prefix)` | `zensight-common/src/command.rs` | `…/@/artifact/status` |
| `command::artifact_cancel_key(prefix)` | `zensight-common/src/command.rs` | `…/@/artifact/cancel` |
| `command::artifact_blob_prefix(prefix)` | `zensight-common/src/command.rs` | `…/@/artifact/blob` (zenoh-blob server prefix; `Blob` delivery) |
| `command::artifact_store_prefix(prefix)` | `zensight-common/src/command.rs` | `…/@/store` (kind-agnostic Tier-2 chunk queryable prefix; `Tree` delivery) |
| `command::artifact_tree_prefix(prefix)` | `zensight-common/src/command.rs` | `…/@/tree` (kind-agnostic Tier-2 index queryable prefix; `Tree` delivery) |
| `all_*_wildcard()` | `zensight-common/src/keyexpr.rs` | the wildcards in §5 |

The control-plane keys for `health`, `errors`, `alive`, `devices/*`, `alerts/*`,
and `artifact/*` + `store/*` + `tree/*` are produced inside `zensight-sensor-core`
(`health.rs`, `liveliness.rs`, `alert.rs`, and the `ArtifactChannel`) so every
sensor inherits them identically by using the framework — sensors never build
these by hand.

[`TelemetryPoint`]: ../zensight-common/src/telemetry.rs
[`KeyExprBuilder::build(source, metric)`]: ../zensight-common/src/keyexpr.rs
[`Alert::alert_key`]: ../zensight-common/src/alert.rs
