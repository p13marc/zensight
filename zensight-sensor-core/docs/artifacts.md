# Artifacts — on-demand large-data transfer

Some data is too big and too rare to stream on the telemetry bus: a sosreport-style
debug bundle, a config directory snapshot, an ad-hoc packet capture. The artifact
channel serves these **on demand** — an operator asks a sensor to produce one, the
sensor builds it off the poll/capture path, and the client fetches the bytes over
`zenoh-blob`. One channel (`artifact.rs`) subsumes what used to be separate
`@/report` and `@/snapshot` surfaces and hosts new kinds as pluggable producers.

See [`../../docs/LARGE-DATA-TRANSFER.md`](../../docs/LARGE-DATA-TRANSFER.md) for the
transfer-tier rationale and [`../../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) §3
for the `@/artifact` / `@/store` / `@/tree` keys.

## The control plane

A sensor enables the channel with `SensorRunner::with_artifacts(source_id,
producers)` (a no-op if no producer is enabled in config). The channel owns:

| Key | Primitive | Purpose |
|-----|-----------|---------|
| `<prefix>/@/artifact/request` | subscriber | PUT an `ArtifactRequest` to ask for an artifact |
| `<prefix>/@/artifact/status` | queryable | GET the per-kind `ArtifactStatus` (lifecycle) |
| `<prefix>/@/artifact/cancel` | subscriber | PUT an artifact id (ULID) to abort/free early |

Delivery servers are spun up only for the tiers actually registered:

- **Tier-1 (`Blob`)** — a `zenoh-blob` `BlobServer` under `<prefix>/@/artifact/blob`.
- **Tier-2 (`Tree`)** — a `TreeServer` + in-memory chunk store over
  `<prefix>/@/store/<algo>/<hash>` (content-addressed, cacheable) and the index at
  `<prefix>/@/tree/<id>`.

Because the control plane is per-protocol (shared by every host running that
protocol), a request's `opts.target_source` disambiguates which host answers; the
channel drops a request whose `target_source` isn't its `source_id`.

## Lifecycle

A request drives a per-kind state machine, surfaced in the status queryable:

```
Generating{detail, progress}  ──▶  Ready{delivery, expires_ms}  ──▶  Expired
                              └──▶  Failed{reason}
```

- On request the channel resolves the producer by kind slug, calls
  `accepts()` for validation/authorization, then applies a **per-kind busy +
  cooldown gate** — so a long capture never blocks a quick debug bundle.
  Production runs off the select loop so status stays responsive; the producer
  streams `ProgressUpdate`s that are republished as `Generating`.
- On success the produced file/dir is finalized into a `Delivery` and held until
  its TTL; a periodic reaper expires it, and `cancel` aborts an in-flight
  production or frees a `Ready` artifact early. Only one live artifact per kind is
  kept (a new one replaces the prior).

## The ArtifactProducer trait

A producer is registered per kind; the channel drives it and the producer **never
touches Zenoh**. It only validates and produces a file or directory:

```rust
#[async_trait]
pub trait ArtifactProducer: Send + Sync + 'static {
    fn kind(&self) -> &'static str;            // slug: "report" | "snapshot" | "capture"
    fn common(&self) -> &CommonArtifactLimits; // cooldown / TTL / max_bytes / chunk_size
    fn delivery_kind(&self) -> DeliveryKind;   // Blob or Tree
    fn advert(&self) -> KindAdvert;            // what the GUI needs to render the request
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), String>; // validate + authorize
    fn tree_max_files(&self) -> Option<u64> { None }              // Tree only
    async fn produce(&self, kind: ArtifactKind, ctx: ProduceCtx) -> anyhow::Result<Produced>;
}
```

`produce` returns a `Produced::File { path, filename }` (→ `Delivery::Blob`) or
`Produced::Dir { path }` (→ `Delivery::Tree`); the variant must match the declared
`delivery_kind`. `ProduceCtx` supplies a private `workdir`, a `CancelToken` a
long-running producer must poll, and a `progress` sender.

## Built-in producers

| Producer | Slug | Delivery | Produces |
|----------|------|----------|----------|
| `ReportProducer` | `report` | `Blob` | a redacted debug bundle (config + health + counters), a single `tar.zst` |
| `SnapshotProducer` | `snapshot` | `Tree` | an allowlisted directory, content-addressed |
| *(netring)* capture | `capture` | `Blob` | a bounded on-demand `pcap[.zst]` off a live packet tap |

`SnapshotProducer` only snapshots an **allowlisted** logical directory name — never
an arbitrary path; that allowlist is the authorization boundary. The `capture`
producer is defined by the netring sensor (it needs the live tap), but its request
params live in the shared `ArtifactKind::Capture` so both ends stay typed. An
unknown kind degrades cleanly to `ArtifactKind::Unsupported` / a `Failed` state.

## Wire types

`ArtifactRequest`, `ArtifactKind`, `ArtifactStatus` / `KindStatus`, `Delivery`,
`TreeSummary` and the re-exported `zenoh-blob` `Manifest` / `TreeIndex` / `Entry`
all live in `zensight-common` (`artifact.rs`) so the GUI, sensor, and any client
share one definition. `ArtifactRequest.id` (a ULID) correlates the request,
status, and delivery.

## See also

- [Framework](framework.md) — `with_artifacts` and the runner lifecycle.
- [`../../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) — `@/artifact` key contract.
