# ZenSight Zenoh Keyspace Reference

This is the canonical reference for every Zenoh key expression ZenSight uses.
All sensors, exporters, and the frontend follow these conventions; new code MUST
build keys through the shared helpers listed in [§7](#7-key-building-helpers)
rather than ad-hoc `format!()`.

The single root is `zensight/`. Everything below it is either **telemetry**
(`zensight/<protocol>/<source>/…`), **control-plane** for one sensor instance
(host-scoped `zensight/<protocol>/<source>/@/…`) or one protocol (shared
`zensight/<protocol>/@/…`), or cross-sensor **metadata** (`zensight/_meta/…`).

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
| `parallax` | zensight-sensor-parallax (live video → `@media`, #402) | hostname |

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

> **Log lines are served on demand, never streamed** (#358). Per-line log
> events are high-cardinality detail, so — like flows/sockets/processes (P2) —
> they live in a bounded in-memory ring inside the logs sensor and are pulled
> from the `zensight/logs/@/query/events` queryable (`Vec<LogRecord>`, newest
> first; selectors `since=<epoch_ms>` inclusive, `max=<n>` default 500,
> `host=<name>`). Each record keeps the #104 identity: a unique `<uid>` =
> `<timestamp_ms><seq>` (zero-padded, time-sortable) plus the OpenTelemetry
> logs data model (`severity_number` 1–24, `severity_text`, `log.record.uid`,
> `log.record.original` when raw is kept) — facility/severity travel as fields/
> labels, not keys. Only the low-rate rollups (`logs/by_severity/*`,
> `logs/by_unit/*`, `logs/ingest/*`, …) ride the telemetry bus for charts and
> alerts. The GUI seeds its rolling buffer from the queryable on open and
> refreshes on a slow tick; fetched lines persist to its local store for
> search-back. (Pre-#358 sensors streamed each line as
> `zensight/logs/<host>/events/<uid>` — the GUI still ingests that shape from
> old sensors, and exporters already excluded it.)

> **Published with a zenoh-ext `AdvancedPublisher`** (per-key cache + miss/
> publisher detection), so it pairs with the GUI's `AdvancedSubscriber` on
> `zensight/**` (history + recovery). The control-plane below uses plain
> `put`/`delete` and a plain subscriber. See
> [Architecture → Zenoh Transport & Pub/Sub Model](ARCHITECTURE.md#zenoh-transport--pubsub-model).

---

## 3. Control-plane

Per-sensor operational channels, in **two scopes**:

- **Host-scoped state** — `zensight/<protocol>/<source>/@/…`, one subtree per
  sensor *instance*. These are last-writer-wins state keys, so they carry the
  instance's `<source>` segment (the same value as its telemetry subtree):
  two machines running sysinfo publish `zensight/sysinfo/hostA/@/health` and
  `zensight/sysinfo/hostB/@/health`, never colliding. The instance control
  prefix is `{key_prefix}/{source}` ([`sensor_control_prefix`], §7).
- **Protocol-scoped channels** — `zensight/<protocol>/@/…`, shared by every
  host running that protocol. These are *deliberately* shared: alert keys
  already disambiguate by hashing `source` in, and the query/command/artifact
  channels rely on the shared key for fan-in/fan-out (a GET on
  `zensight/netlink/@/query/sockets` collects replies from **every** host —
  exactly what the flow↔process join wants).

### Host-scoped state — `zensight/<protocol>/<source>/@/…`

| Key | Direction | Payload | Emitted by |
|-----|-----------|---------|------------|
| `@/health` | put | `HealthSnapshot` (carries `source`) | every sensor (`SensorRunner`) |
| `@/errors` | put | `ErrorReport` | every sensor (`HealthReporter`) |
| `@/status` | put | status JSON (running/offline) | every sensor (`StatusPublisher`) |
| `@/alive` | liveliness token | — | every sensor (`LivelinessManager`) |
| `@/devices/<device>/liveness` | put | `DeviceLiveness` | sensors with per-device tracking |
| `@/devices/<device>/alive` | liveliness token | — | sensors with per-device tracking |

> **Compat.** Before release 0.8 these lived directly under
> `zensight/<protocol>/@/…` with no `<source>` segment — N machines running
> the same sensor overwrote each other (last-writer-wins) and shared one
> liveliness token. Sensors now publish **only** the host-scoped shape; the
> GUI and correlator keep consuming the legacy shape for one release
> (mixed-fleet rolling upgrade), to be dropped in 0.9.

### Protocol-scoped channels — `zensight/<protocol>/@/…`

| Key | Direction | Payload | Emitted by |
|-----|-----------|---------|------------|
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

> **Multi-host targeting.** Because these channels are shared, a PUT to
> `@/commands/<topic>` or `@/artifact/request` reaches **every** host running
> that protocol. The artifact channel already targets one instance via
> `ArtifactRequest.opts.target_source` (each sensor filters requests against
> its own source), and the GUI sets it from the per-instance Sensors card.
> Commands have no such field yet — a `set_capture` or systemd `action`
> command fans out to all hosts (mitigated for systemd `action` by the
> per-host allowlist + polkit). Planned follow-up: an optional
> `target_source` on the `Command<T>` envelope, mirroring the artifact
> precedent — targeting via payload, not key, preserves the fan-in query
> pattern.

### 3.1 Control topics in use

| Sensor | `@/commands/<topic>` · `@/status/<topic>` | Purpose |
|--------|---|---|
| logs | `filter` | add/remove/clear dynamic message filters |
| netlink | `expectations` | hot-swap sentinel expectations (sockets/links/neighbors/routes/metrics/rates/delivery/route-flaps + policy `rules` #323) |
| netlink | `collection` | toggle collectors at runtime |
| netring | `detectors` | runtime detection tuning: allowlist + per-detector mute/threshold |
| netring | `capture_filter` | hot-swap the reloadable packet-tier BPF filter (capture focus) |
| netring | `threat_intel` | hot-reload IOC indicators / YARA rules (set_ioc / reload_ioc_files / clear_ioc / set_yara) — armed by `threat.reload` or startup indicators |
| netring | `capture_disk` | capture-to-disk control (#327): `capture_now {tag?}` fires the pre-trigger ring (or rotates the spool); `set_capture {mode}` hot-switches between the armed modes. Status reply: mode, ring occupancy, retention usage, last event |
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
| `Capture { duration_secs, max_bytes, filter, snaplen, compress }` | — | on-demand pcap capture off netring's live packet tap (issue #333); netring only, gated on `capture.on_demand.enabled` |
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
  dedup fleet-wide — see `../zenoh-blob/docs/router-storage.md`.

**Producers.** sensor-core owns one `ArtifactChannel` (request/status/cancel +
reaper, per-kind busy + cooldown, lazy `BlobServer`/`TreeServer`). Each supported
kind is an `ArtifactProducer` (the `Snapshot` producer advertises its allowlisted
`dirs` via `KindAdvert::Snapshot { dirs }`; the GUI hides `KindAdvert::Unknown`
kinds). The GUI surfaces available kinds/dirs for download in the Sensors view.
See `docs/design/large-data-transfer.md`.

### 3.2 On-demand detail queries — `@/query/<topic>`

High-cardinality detail is **served on request, never streamed** onto the
telemetry bus (principle: keep the bus low-cardinality). Parameters are passed
as Zenoh selector params (e.g. `?top=20`, `?state=&port=`).

| Sensor | `@/query/<topic>` | Reply |
|--------|---|---|
| logs | `events?since=<epoch_ms>;max=N;host=<name>` (#358, zenoh `;`-separated params) | `Vec<LogRecord>` (newest first, from the bounded per-line ring) |
| sysinfo | `processes?sort=cpu\|mem\|io&top=N`, `latency`² | `Vec<ProcessRecord>` / `LatencyReport` |
| netlink | `routes`, `neighbors`, `sockets?state=&port=&ip=`⁶, `addresses`, `events`, `route_changes`, `tc`, `xfrm`, `nft`, `bandwidth?top=N`⁴, `retransmits`³, `connections`³ | `Vec<…Record>` |
| netring | `flows`, `tls`, `talkers?top=N`, `matrix?top=N`, `elephant_flows`, `dns?top=N`, `http?top=N`, `quic`, `ssh`, `encrypted_dns`, `ja4h?top=N`¹, `assets`, `captures`⁵, `bandwidth?top=N`⁷ | `Vec<…Record>` |
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

⁴ `bandwidth` (per-process TCP goodput rate, #317/epic #320) is served when
`collect.bandwidth` is on (default; unprivileged). Replies `Vec<BandwidthRecord>`
(`zensight-common::bandwidth`), ranked by rate, top-N (default 50). **TCP-only,
app-goodput** — every record carries `bw.source`/`bw.semantics`/`bw.proto` so the
GUI never blends it with wire-L3 (systemd) or wire-L2 (capture) numbers.

⁷ netring's `bandwidth` (wire-level bandwidth-by-process, #318/epic #320) is the
**opt-in, best-effort** capture tier, served only when `bandwidth_attribution` is
set. netring measures per-flow wire bandwidth on the capture path; a periodic
sock_diag dump + `/proc` fd scan joins each live 5-tuple to its owning process
off the hot path. Replies `Vec<BandwidthRecord>` tagged `bw.source = netring`,
`bw.semantics = wire-l2` (full frame, undirected — the whole rate is reported as
`tx_bps`), `bw.proto = all` (TCP+UDP), ranked, top-N (default 100). Flows whose
socket isn't in the current dump fall into an explicit unattributed bucket
(`pid = -1`). The GUI Bandwidth monitor fetches this alongside the netlink
`bandwidth` key and renders both, each behind its own semantics badge.

⁵ `captures` (capture-to-disk file index, #327) is served when
`capture.to_disk.mode != off`. Replies `Vec<CaptureRecord>` (newest first):
triggered captures carry the firing detector, packet counts and — while their
serve TTL lives — the `artifact_id` to download the bytes through
`@/artifact/blob/**`; rotating spool files are metadata-only (local disk).
Companion telemetry: `capture/events` (lifecycle Text points) and
`capture/disk/*` (mode, ring occupancy, retention usage, drop/eviction/trigger
counters).

⁶ `ip=` (#309) narrows the reply to sockets whose local **or** remote endpoint
IP matches. The GUI flow↔process join queries both flow-endpoint IPs, collects
**all** replies (every netlink sensor answers the shared key; only the host
owning an endpoint can hold the matching socket), and matches the exact
5-tuple in either direction. The `community_id` on `FlowRecord` is the same
cross-tool flow key, so external Zeek/Suricata records can reuse this join.

### 3.3 Media plane — `zensight/<protocol>/<source>/@media/…` (#359)

Live video / imagery rides its own **opaque** plane. `@media` is an
`@`-verbatim chunk — a *sibling* of the `@/` control plane, a **different**
chunk — so a media key is invisible to **both** the telemetry firehose
(`zensight/**`) and the control-plane wildcard (`zensight/*/@/**`). Samples are
raw encoded bytes with a Zenoh `Encoding` (`video/h264`, `image/jpeg`) + a
**CBOR `FrameMeta` attachment** (`zensight-common/src/stream.rs`: keyframe
flag, optional pts/dts/duration ns, sequence, width, height) — **never** a
`TelemetryPoint`/`Format` envelope, and never fed to the telemetry decoder
(the exporters' `is_telemetry_key` rejects any `@`-prefixed chunk, not just
`/@/`).

| Key | Direction | Payload | Built by |
|-----|-----------|---------|----------|
| `@media/<stream>/video/<codec>/<profile>` | put (plain, per-stream publisher) | raw encoded access units + CBOR `FrameMeta` attachment | `media_video_key()` |
| `@media/<stream>/preview/jpeg` | put (plain, per-stream publisher) | encoded JPEG preview frames + CBOR `FrameMeta` attachment | `media_preview_key()` |

**Viewer subscription pattern**: a preview viewer subscribes to the exact
`@media/<stream>/preview/jpeg` key; a video viewer subscribes with the
`<profile>` chunk as a **single-chunk wildcard** —
`…/@media/<stream>/video/<codec>/*` — because the profile chunk is the
*sensor's* configuration (e.g. parallax `video.profile`, default `main`) and
the catalogue does not advertise it. The key stays scoped to exactly one
stream and one codec, so the "no wildcard firehose" rule's intent holds; the
publisher's matching listener fires for the wildcard subscriber all the same
(zenoh matching is intersection-based — pinned in the parallax sensor e2e).

The media publisher is a **plain** `zenoh::pubsub::Publisher` (NOT an
`AdvancedPublisher` — no cache/recovery/history for a superseded frame stream),
carrying `QosClass::LiveVideo` (best-effort · drop · interactive-high · express
off — a stale frame is worthless, and the encoder must never block).
`zensight-sensor-core`'s `Publisher::raw_media_publisher()` returns a
`RawMediaPublisher` whose `matching_listener()` fires when a viewer subscribes,
so the sensor can force an immediate keyframe (late joiners get a decodable
picture at once). The `keyframe` flag is a byte-level promise: the parallax
sensor derives it from the bitstream and publishes every keyframe access unit
self-contained (SPS/PPS in the same AU, prepended from cache when the encoder
or camera didn't inline them) — a fresh decoder may begin at any sample whose
attachment says `keyframe: true`.

**Stream control rides the ordinary `@/` channels** (§3), not `@media` —
and, like the media keys themselves, it is **host-scoped**: the prefix is
`zensight/<protocol>/<source>` (e.g.
`zensight/parallax/hostA/@/commands/stream`), so commands reach exactly one
host's sensor and its catalogue/status answer for that host only:

| Key (under `zensight/<proto>/<source>`) | Direction | Payload | Topic |
|-----|-----------|---------|-------|
| `@/commands/stream` | subscribe | `Command<StreamControl>` (`OpenStream`/`CloseStream`/`RequestKeyframe`) | `stream` |
| `@/query/streams` | queryable | `Vec<StreamDescriptor>` (advertised streams; late-joiner seed) | `streams` |
| `@/status/streams` | queryable **and** declared-publisher transitions | `Vec<StreamStatus>` reply / one `StreamStatus` per transition (open? · viewers · active profile) | `streams` |

**BREAKING** (#402): the `@/status/streams` *queryable reply* changed in-place
from a single `StreamStatus` to `Vec<StreamStatus>` (one entry per currently
open stream). Migration: decode the reply as a JSON array and pick your stream
by the `stream` field; the *published transitions* on the same key are
unchanged (still one `StreamStatus` per transition). A failed `open_stream`
now also publishes a definitive `open: false` transition.

Stream *stats* (fps/kbps/drops/viewers/encode_ms) ride normal telemetry under
`zensight/<proto>/<source>/<stream>/stats/<metric>`, so existing charts light up
for free. The concrete producer is **`zensight-sensor-parallax`** (#402): V4L2
cameras, RTSP cameras, and synthetic test patterns, encoded on demand
(open_stream/close_stream) to H.264 + JPEG previews — see that crate's
`docs/streams.md`; the GUI's viewer is `zensight/src/view/specialized/parallax*.rs`.

### netring detector-registry migration (#369, BREAKING)

Adopting flowscope 0.22's `DetectorRegistry` + netring 0.29's `aggregate()` /
`red()` changed three netring contracts:

| Area | Before | After |
|------|--------|-------|
| Flow-lifetime telemetry | `flow/duration_p50_ms`, `flow/duration_p95_ms` (Gauges) | `flow/red/rate`, `flow/red/error_ratio`, `flow/red/p50_ms`, `flow/red/p95_ms`, `flow/red/p99_ms` (Gauges, from netring `red()`) |
| `@/query/talkers` reply | `TalkerRecord { dst, bytes, packets, flows, names }` — per-**destination** cumulative | `TalkerRecord { src, bytes_per_sec, names }` — per-**source** rolling 60 s rate |
| `@/query/matrix` reply | `MatrixRecord { src, dst, bytes, packets, flows }` — cumulative | `MatrixRecord { src, dst, bytes_per_sec, names }` — rolling 60 s rate |
| Anomaly slug (`@/alerts` `rule`, `anomaly/<slug>/total`) | `RitaBeacon` | `BeaconRita` (flowscope upstream) |
| Anomaly slug | `DataExfiltration` | `DataExfil` (flowscope upstream) |

The `talkers?top=N` / `matrix?top=N` query keys are unchanged; only their reply
record shapes and the ranking axis (rolling bytes/sec vs cumulative bytes) change.
Detector semantics also shift with the stock detectors (e.g. connection-flood is
now source-keyed, not `(dst,port)`-keyed). Runtime detector tuning
(`@/commands/detectors`: allowlist / mute / threshold) is preserved via the
`Tuned` decorator.

---

## 4. Metadata — `zensight/_meta/…`

Cross-sensor, protocol-independent registries.

| Key | Payload | Emitted by |
|-----|---------|------------|
| `zensight/_meta/sensors/<name>/<source>` | `SensorInfo` (identity-stamped registration) | every sensor (runner, 60 s re-emit) |
| `zensight/_meta/evidence/host/<sensor>/<source>` | `HostEvidence` (identity claim) | every sensor (self-report) + observers (#307) |
| `zensight/_meta/evidence/names/<sensor>/<ip-slug>` | `NameObservation` (latest name for an IP) | netring passive DNS (#307/#308) |
| `zensight/_meta/entity/host/<entity_id>` | `HostEntity` (merged host, cached + re-emitted) | **correlator only** (single writer, #305) |
| `zensight/_meta/query/entities` | queryable → `Vec<HostEntity>` (late-joiner seed) | correlator (#305) |
| `zensight/_meta/query/names?ip=<ip>` | queryable → `Vec<NameVal>` for one IP | correlator (#305) |

### 4.1 Evidence contract (#301)

- Evidence is a **claim**, not a verdict: `observer: None` marks a sensor's
  self-report about its own host; `observer: Some(sensor)` marks a third-party
  claim about a device observed on the wire (weighted lower by the correlator).
- `host_id` is `sha256(machine-id + app salt)` hex — the raw machine-id never
  leaves the host. All ZenSight sensors on one machine derive the same value.
- **TTL**: consumers ignore evidence whose `last_updated` is older than the
  evidence TTL (correlator default 900 s). Publishers therefore refresh live
  claims at ≤ TTL/2 — the runner's 60 s re-emission satisfies this for
  self-reports — and stale claims age out instead of binding entities forever.
- Publishers use cached AdvancedPublishers (`cache(1)`), so a late joiner
  seeds the latest doc per key immediately.

The former `zensight/_meta/correlation/<ip>` keyspace (`CorrelationEntry`) is
**deleted** — it was never published by any sensor; entity resolution moves to
the correlator's `_meta/entity/**` keyspace (#305).

### 4.2 Entity keyspace — the correlator (single writer, #305)

`zensight-correlator` is the **only** publisher under `zensight/_meta/entity/**`.
It subscribes to the evidence keyspace above, merges claims into hosts with a
union-find over ranked identity rules (host_id 1.0 > MAC+IP 0.8 > FQDN 0.5 >
hostname 0.25; IP and MAC each alone are *never* a join; a host_id-conflict guard
blocks weaker bridges between two distinct machine-ids), and publishes one
`HostEntity` per resolved host.

- **Cached + re-emitted**: entities ride a cached AdvancedPublisher (late joiners
  seed the latest doc) and are re-emitted every `reemit_secs` (default 60 s),
  which doubles as correlator liveness. Consumers mark an entity stale after
  ~3× that period; a `SampleKind::Delete` tombstones a retired/merged entity.
- **Stable ids**: `entity_id = h_<12hex>` — the machine-id hash prefix when known,
  else `sha256` of the best evidence key (fqdn > mac > hostname > ip). Ids are
  order-independent and stable across restarts (the merge is a pure function of
  the live evidence set), so a restart reseeds from the evidence caches with no
  local database. On an id upgrade (a weak id later gaining a machine-id) the old
  id is carried in `aliases[]` and tombstoned.
- **Names served, not broadcast**: fleet-host names ride inline on their
  `HostEntity`; arbitrary external IPs (every CDN netring ever saw) are resolved
  on demand via the `_meta/query/names?ip=` queryable to avoid flooding the bus.
- **Single-writer guard**: on startup the correlator probes a Zenoh liveliness
  token on `zensight/_meta/correlator/@/alive`; a second instance detects the
  first and exits rather than double-publishing.
- **Telemetry keys are deliberately NOT re-keyed** on entity ids — `source` stays
  human-readable; identity travels as evidence + labels, and the entity layer
  provides the join. So no sensor or exporter depends on the correlator: if it is
  down, entities go stale and consumers fall back to per-protocol devices.

### 4.3 Historical passive-DNS tier — `zensight/@pdns/<ip>` (#310)

The evidence/entity keyspace above is **live** state (TTL-swept; a restarted
correlator rebuilds it, it holds no history). For a durable *historical* record
of what an IP resolved to over time, the correlator emits a second stream on a
dedicated plane:

| Key | Payload | Emitted by |
|-----|---------|------------|
| `zensight/@pdns/<ip-slug>` | `PdnsRecord` (IP + full accumulated `Vec<NameVal>` + `last_updated`) | **correlator** (on every name-store update, #310) |

- `@pdns` is an **`@`-verbatim chunk** — a sibling of the `@/` control plane and
  the `@media` plane (#359), but a *different* chunk — so an `@pdns` key is
  invisible to **both** the telemetry firehose (`zensight/**`) and the per-sensor
  control wildcard (`zensight/*/@/**`). The exporters' `@`-chunk reject keeps
  these off Prometheus/OTel. (Guard test: `keyexpr::pdns_tier_is_off_the_telemetry_and_control_buses`.)
- `<ip-slug>` slugifies the IP (`.`/`:` → `-`), one chunk per IP — the same
  convention as the `evidence/names/<ip-slug>` keys. The record payload keeps the
  real (un-slugged) address.
- Unlike the live wire `NameObservation` (one name per sample, last-writer-wins),
  a `PdnsRecord` carries the correlator's **full accumulated name set** for the IP
  (an A record, a PTR, a TLS SNI, …), so one key captures the complete IP↔name
  binding at that instant.
- Published with a **plain** `session.put` (not a per-IP declared publisher — the
  IP set is unbounded) carrying `QosClass::Entity` (reliable · block): a dropped
  `@pdns` PUT would be a gap in the historical record. The publish is **cheap and
  off the packet hot path** — it fires per correlator name-store update, never per
  packet.
- **Nothing consumes this on the live bus.** The tier exists to be captured by a
  router-hosted storage backend (`zenoh-backend-influxdb` into a time-series
  bucket — see [`../zensight-correlator/docs/storage.md`](../zensight-correlator/docs/storage.md) and
  [`configs/router-pdns-influxdb-storage.json5`](../configs/router-pdns-influxdb-storage.json5)),
  giving a queryable IP↔name history without loading the correlator.

---

## 5. Wildcards & subscriptions

| Wildcard | Used by | Catches |
|----------|---------|---------|
| `zensight/**` | frontend (history sub), exporters | all telemetry *and* `_meta` (but **not** `@/…` nor `@media/…`) |
| `zensight/*/*/@/**` | frontend | all **host-scoped** control-plane (health/errors/status/liveness) — never intersects `zensight/*/@/**`, telemetry, `@media`, or `@pdns` (pinned in `zensight-common` tests) |
| `zensight/*/@/**` | frontend | all **protocol-scoped** control-plane (alerts) + legacy pre-0.8 state keys |
| `zensight/<proto>/<source>/@media/<stream>/…` | media viewer | one stream's opaque samples: the exact `preview/jpeg` key, or `video/<codec>/*` (single-chunk wildcard over the sensor-configured profile — see §3.3) |
| `zensight/*/*/@/alive` | frontend | sensor liveliness tokens (host-scoped) |
| `zensight/*/*/@/devices/*/alive` | frontend | device liveliness tokens (host-scoped) |
| `zensight/*/@/alive` | frontend | legacy sensor liveliness tokens (pre-0.8 sensors) |
| `zensight/*/@/devices/*/alive` | frontend, correlator | legacy device liveliness tokens |
| `zensight/*/@/query/alerts` | frontend (GET at startup) | firing-set seed for late joiners |
| `zensight/<protocol>/@/alerts/**` | any alert consumer | one sensor's alerts (note explicit `@`) |
| `zensight/*/@/alerts/*` | exporters (`export_alerts`) | all sensors' alerts, mirrored to Prometheus/OTel |
| `zensight/_meta/sensors/**` | frontend | sensor registrations (per `<name>/<source>` instance) |
| `zensight/_meta/evidence/**` | correlator (#305) | host-identity claims + name observations |
| `zensight/_meta/entity/**` | frontend (#306) | merged `HostEntity` docs + tombstones |
| `zensight/_meta/query/entities` | frontend (GET at startup) | entity-set seed for late joiners |
| `zensight/@pdns/**` | router-hosted storage backend (#310) | historical IP↔name `PdnsRecord`s (verbatim `@pdns` — off `zensight/**` and `zensight/*/@/**`) |

Exporters (`prometheus`, `otel`) subscribe to `zensight/**` and **skip**
control/metadata by rejecting any key with an `@`-prefixed chunk (`/@/…` control
plane **and** the `@media/…` plane, #359) or starting with `zensight/_meta/` —
only true telemetry is exported. With `export_alerts` on
(the default) each exporter **additionally** declares a second subscriber on
`zensight/*/@/alerts/*` (`all_alerts_wildcard()`) to mirror firing alerts —
necessary precisely because `zensight/**` does not cross the `@` chunk. Each
exporter's telemetry subscription is narrowable via `filters.key_expr` (#357,
default `zensight/**`) to drop unwanted protocols + `_meta/**` at the wire.

## 5a. QoS & serialization (epic #352)

Every publisher is **declared** (never a one-shot `session.put`; interned key +
primed routing) and carries a fixed `zensight_common::QosClass`:

| Class | Keys | Reliability | Congestion | Priority |
|-------|------|-------------|------------|----------|
| Telemetry | `zensight/<proto>/<source>/**` | best-effort | drop | data-low |
| Health/liveness | `@/health`, `@/devices/*/liveness`, `@/errors` | best-effort | drop | data |
| Alert / Command | `@/alerts/*`, `@/commands/*`, `@/status` | **reliable** | **block** | interactive-high |
| Evidence / Entity | `_meta/evidence/**`, `_meta/entity/**` | **reliable** | **block** | data |
| LiveVideo (#359) | `@media/<stream>/**` | best-effort | drop | **interactive-high** |

Superseded streams (telemetry/health) drop under congestion; must-arrive control
(alerts/commands/evidence/entities) blocks. Payloads default to **CBOR** (#355);
consumers decode format-agnostically (`decode_auto`), so mixed JSON/CBOR fleets
interoperate during a rolling upgrade.

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

The four key-planes, visually:

```mermaid
mindmap
  root(("zensight bus"))
    Telemetry
      "zensight/&lt;protocol&gt;/&lt;source&gt;/&lt;metric&gt;"
      TelemetryPoint
    "Control-plane (@/, verbatim)"
      "zensight/&lt;protocol&gt;/&lt;source&gt;/@/** (host-scoped)"
      "health, errors, status, alive, devices/**"
      "zensight/&lt;protocol&gt;/@/** (protocol-scoped)"
      "alerts/&lt;alert_key&gt;"
      "commands, query, status, artifact/**"
    "Metadata (_meta/)"
      "sensors/&lt;name&gt;/&lt;source&gt;"
      "evidence/host, evidence/names"
      "entity/host/&lt;entity_id&gt;"
      "query/entities, query/names"
    "Verbatim media + pdns"
      "@media/** (video, preview, #359)"
      "@pdns/&lt;ip-slug&gt; (PdnsRecord, #310)"
```

And the annotated tree, chunk by chunk:

```
zensight/
├── <protocol>/
│   ├── <source>/<metric…>              # telemetry  (TelemetryPoint)
│   ├── <source>/@/                     # HOST-SCOPED state (one per instance)
│   │   ├── health                      # HealthSnapshot
│   │   ├── errors                      # ErrorReport
│   │   ├── status                      # running/offline status
│   │   ├── alive                       # liveliness token
│   │   ├── devices/<device>/liveness   # DeviceLiveness
│   │   └── devices/<device>/alive      # liveliness token
│   └── @/                              # PROTOCOL-SCOPED (shared by all hosts)
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
├── @pdns/<ip-slug>                    # PdnsRecord — historical IP↔name (correlator, #310)
└── _meta/
    ├── sensors/<name>/<source>         # SensorInfo (identity-stamped registration)
    ├── entity/host/<entity_id>        # HostEntity (correlator output, #305)
    ├── query/{entities,names}         # late-joiner queryables (#305)
    └── evidence/                       # host-identity claims (correlator input)
        ├── host/<sensor>/<source>      # HostEvidence (self-report / observed)
        └── names/<sensor>/<ip-slug>    # NameObservation (passive DNS)
```

---

## 7. Key-building helpers

Do not hand-write keys. Build them through these so the conventions stay
enforced and a single change propagates everywhere.

| Helper | Location | Produces |
|--------|----------|----------|
| `KeyExprBuilder::build(source, metric)` | `zensight-common/src/keyexpr.rs` | `zensight/<proto>/<source>/<metric>` |
| `keyexpr::sensor_control_prefix(proto, source)` | `zensight-common/src/keyexpr.rs` | `zensight/<proto>/<source>` (the instance control prefix) |
| `KeyExprBuilder::status_key(source)` | `zensight-common/src/keyexpr.rs` | `…/<source>/@/status` |
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
| `keyexpr::media_video_key(proto, source, stream, codec, profile)` | `zensight-common/src/keyexpr.rs` | `…/@media/<stream>/video/<codec>/<profile>` (#359) |
| `keyexpr::media_preview_key(proto, source, stream)` | `zensight-common/src/keyexpr.rs` | `…/@media/<stream>/preview/jpeg` (#359) |
| `keyexpr::pdns_key(ip)` | `zensight-common/src/keyexpr.rs` | `zensight/@pdns/<ip-slug>` (#310) |
| `all_*_wildcard()` (incl. `all_pdns_wildcard()`) | `zensight-common/src/keyexpr.rs` | the wildcards in §5 |

The control-plane keys for `health`, `errors`, `alive`, `devices/*`, `alerts/*`,
and `artifact/*` + `store/*` + `tree/*` are produced inside `zensight-sensor-core`
(`health.rs`, `liveliness.rs`, `alert.rs`, and the `ArtifactChannel`) so every
sensor inherits them identically by using the framework — sensors never build
these by hand.

[`TelemetryPoint`]: ../zensight-common/src/telemetry.rs
[`KeyExprBuilder::build(source, metric)`]: ../zensight-common/src/keyexpr.rs
[`Alert::alert_key`]: ../zensight-common/src/alert.rs
