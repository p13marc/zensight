# Rerun 0.34 capabilities — research snapshot (#416)

Research date: **2026-07-11**. Target: **`rerun` 0.34.1** (crates.io). Every claim below was
verified on the research date against the linked primary source; where a claim could only be
verified by building code, it is marked *[build-verified at commit 4]* and evidence is appended
in the [Appendix](#appendix-dependency--build-evidence-commit-4). This document is the anchor
for the whole evaluation: the API names pinned here are exactly what
`zensight-rerun/src/rerun_sink.rs` codes against.

## 1. Crate & packaging facts

Source: [crates.io API for rerun 0.34.1](https://crates.io/api/v1/crates/rerun/0.34.1) and
[docs.rs feature list](https://docs.rs/crate/rerun/0.34.1/features), both checked 2026-07-11.

- **0.34.1 published 2026-07-07** (crates.io `created_at: 2026-07-07T17:28:05Z`); 0.34.0
  shipped a few days earlier (rc4 was cut 2026-07-02 per the
  [GitHub releases page](https://github.com/rerun-io/rerun/releases)).
- **`rust-version = 1.92`** (crates.io metadata). Our toolchain is 1.95.0 — fine locally, but
  this is far above the zenoh upstream's MSRV discipline (zenoh pins 1.75): Rerun's MSRV
  moves fast and would ratchet the whole workspace if the adapter ever stopped being optional.
  *Mild reject-signal for deep integration; acceptable for an optional, `publish = false` crate.*
- **Default features** = `analytics, importers, dataframe, demo, glam, image, log, map_view,
  sdk, server`. Notably:
  - `analytics` (telemetry phoning home) is **on by default** → must be opted out for any
    offline/air-gapped ZenSight deployment. With `default-features = false, features = ["sdk"]`
    the `re_analytics` crate is not even compiled (`analytics = ["dep:re_analytics", ...]`).
    *Adopt-relevant finding: sdk-only builds are analytics-free by construction.*
  - `native_viewer` is **opt-in** (`native_viewer = ["dep:re_viewer", "dep:re_crash_handler",
    "dep:re_viewer_mcp"]`) — the egui/wgpu viewer stack never enters an sdk-only build.
  - `sdk = ["dep:re_sdk", "dep:re_sdk_types"]`; `server = ["dep:re_grpc_server",
    "re_sdk/server", "tokio/signal"]` — `server` is only needed to *host* a gRPC endpoint
    (`serve_grpc`), not to connect out.
- **Pin used by the adapter**:
  `rerun = { version = "=0.34.1", default-features = false, features = ["sdk"] }`.
  `connect_grpc` (gRPC *client*) and `save()` (.rrd file sink) are both on
  `RecordingStreamBuilder` under `sdk` — the docs.rs page for the builder shows no extra
  feature gates on either
  ([docs.rs RecordingStreamBuilder](https://docs.rs/rerun/0.34.1/rerun/struct.RecordingStreamBuilder.html),
  checked 2026-07-11). *[build-verified at commit 4]*
- Dependency-weight expectation: `sdk` still pulls **Apache Arrow** (chunked columnar
  encoding is the wire/storage format — see
  [RRD format docs](https://rerun.io/docs/concepts/logging-and-ingestion/) ) and a tonic/gRPC
  client stack. Measured cost is in the [Appendix](#appendix-dependency--build-evidence-commit-4).

## 2. Rust SDK API surface pinned for this evaluation

All from [docs.rs rerun 0.34.1](https://docs.rs/rerun/0.34.1/rerun/), checked 2026-07-11.

### 2.1 `RecordingStreamBuilder`

([docs](https://docs.rs/rerun/0.34.1/rerun/struct.RecordingStreamBuilder.html))

- `RecordingStreamBuilder::new(application_id)` — application id, typically the app name.
- `.recording_id(impl Into<RecordingId>)` — explicit recording id "for multi-process logging".
- `.save(path) -> Result<RecordingStream, _>` — stream to an `.rrd` file on disk (**record mode**).
- `.connect_grpc()` / `.connect_grpc_opts(url)` — connect to a remote viewer/proxy (**live mode**).
- `.set_sinks(impl IntoMultiSink)` — multiple sinks at once, documented for
  `GrpcSink` + `FileSink` → **"both" mode (live + record simultaneously) exists in 0.34**.
  (`RecordingStream::set_sinks` also exists for swapping sinks post-construction.)
- Also available, not used by the adapter: `.buffered()`, `.memory()`, `.stdout()`,
  `.serve_grpc()`/`.serve_grpc_opts(...)` (needs `server` feature), `.spawn()` (spawns a local
  viewer process — useless headless).

### 2.2 Timelines — exact 0.34 names

([docs](https://docs.rs/rerun/0.34.1/rerun/struct.RecordingStream.html), checked 2026-07-11.)
The older `set_time_seconds` / `set_time_nanos` names from ≤0.22 are **gone**; the 0.34 surface is:

- `set_time(timeline, impl TryInto<TimeCell>)` — the general form
  (`TimeCell::from_sequence(...)`, durations, timestamps).
- `set_time_sequence(timeline, sequence)` — frame-counter style.
- `set_duration_secs(timeline, secs)` — relative time.
- **`set_timestamp_nanos_since_epoch(timeline, nanos)`** and
  `set_timestamp_secs_since_epoch(timeline, secs)` — absolute wall-clock. The adapter uses the
  nanos form: ZenSight domain timestamps are epoch **milliseconds** (`TelemetryPoint.timestamp:
  i64`), so `nanos = ms * 1_000_000` with no float rounding.
- `set_timepoint(timepoint)`, `disable_timeline(name)`, `reset_time()` — per-thread state.
- Timeline state is **per thread** and sticky: `set_*` before every `log` call from the sink
  worker thread, which is single-threaded in our design, so this is safe.
- 0.34 behavior change ([migration 0.33→0.34](https://rerun.io/docs/reference/migration/migration-0-34)):
  the SDK **no longer injects the `log_tick` timeline** by default; `log_time` (receive time)
  is still auto-injected unless `RERUN_LOG_TIME=0`. The adapter always sets its own domain
  timeline and treats `log_time` as diagnostic-only — the epic's requirement that timelines
  come from **domain timestamps, never receive time** is satisfied by construction.

### 2.3 Logging & archetypes

- `rec.log(entity_path, &archetype)` and `rec.log_static(...)`; `rec.flush_blocking()`
  ([docs](https://docs.rs/rerun/0.34.1/rerun/struct.RecordingStream.html)).
- **Scalars**: `rerun::archetypes::Scalars::new(impl IntoIterator<Item = impl Into<Scalar>>)`
  plus `Scalars::single(value)`
  ([docs](https://docs.rs/rerun/0.34.1/rerun/archetypes/struct.Scalars.html)). The singular
  `Scalar` archetype of ≤0.22 was replaced by plural `Scalars` (0.23-era change).
- **Series styling**: `SeriesLines` / `SeriesPoints` (plural; per-series colors/names/widths),
  logged `log_static` on the same entity path
  ([archetypes index](https://docs.rs/rerun/0.34.1/rerun/archetypes/index.html)).
- **TextLog**: `TextLog::new(text).with_level(TextLogLevel)` with constants
  `TRACE, DEBUG, INFO, WARN, ERROR, CRITICAL`
  ([TextLog](https://docs.rs/rerun/0.34.1/rerun/archetypes/struct.TextLog.html),
  [TextLogLevel](https://docs.rs/rerun/0.34.1/rerun/components/struct.TextLogLevel.html)).
- **AnyValues** (arbitrary key/value payloads, our alert/event attributes):
  `rerun::AnyValues` — `Default` + `with_component_from_data(...)` /
  `with_component(...)` / `with_component_override(...)`; implements `AsComponents` so it can
  be logged alongside/instead of a fixed archetype
  ([docs](https://docs.rs/rerun/0.34.1/rerun/struct.AnyValues.html)). Exact ergonomics
  *[build-verified at commit 6]*.
- **Graphs**: `GraphNodes::new(node_ids).with_positions(...).with_labels(...).with_colors(...)
  .with_radii(...)` + `GraphEdges::new([(from, to), ...]).with_directed_edges()`
  ([GraphNodes](https://docs.rs/rerun/0.34.1/rerun/archetypes/struct.GraphNodes.html)).
  Rendered by the viewer's Graph view (force-directed when positions are omitted).

### 2.4 Entity paths

[Entity path docs](https://rerun.io/docs/concepts/entity-path): hierarchical, `/`-separated;
parts with unusual characters need escaping, plain `[a-zA-Z0-9_-]`-ish parts don't. Our paths
(`hosts/<entity_id>/<proto>/<metric...>`, `sensors/<proto>/<source>/<metric...>`) reuse
ZenSight key chunks which are already key-expression-safe.

## 3. The `.rrd` format

- **Magic**: files start with FourCC **`RRF2`** at offset 0, followed by a binary-encoded
  semver (u8 major, minor, patch, meta). Ground truth in the 0.34.1 tree:
  `crates/store/re_log_encoding/src/rrd/mod.rs` —
  `pub const RRD_FOURCC: [u8; 4] = *b"RRF2";` with `OLD_RRD_FOURCC = [RRF0, RRF1]`
  ([source](https://github.com/rerun-io/rerun/blob/0.34.1/crates/store/re_log_encoding/src/rrd/mod.rs);
  header layout confirmed by
  [`rrd.hexpat`](https://github.com/rerun-io/rerun/blob/0.34.1/crates/store/re_log_encoding/rrd.hexpat):
  `#pragma magic [ 52 52 46 ?? ] @ 0x00`). The commit-7 e2e test asserts the first 4 bytes are
  `RRF2`.
- **Structure**: a linear sequence of framed messages (`SetStoreInfo`, `ArrowMsg` carrying one
  Arrow-IPC-encoded chunk each, `BlueprintActivationCommand`) with an optional footer index
  for random access ([RRD format concepts](https://rerun.io/docs/concepts/logging-and-ingestion/) —
  `rrd-format` page, checked 2026-07-11). Appended-message design means a **crash-truncated
  file loads up to the last complete message** (no single point-of-failure trailer), though
  the footer index is then missing.
- **Backward-compat policy** ([0.23 release blog](https://rerun.io/blog/release-0.23), checked
  2026-07-11): since 0.23, Rerun commits that *each release loads the previous release's
  files* — an N→N+1 guarantee only, **not** "0.34 loads 0.23". Long-horizon archives must be
  re-migrated release-by-release. The CLI ships
  **`rerun rrd migrate`** ("Migrate one or more .rrd files to the newest Rerun version") for
  exactly this ([CLI manual](https://rerun.io/docs/reference/cli)).
  *Reject-signal for using `.rrd` as a long-term evidence/archive format; acceptable for
  short-lived incident captures.*
- **`rerun rrd` CLI subcommands** ([CLI manual](https://rerun.io/docs/reference/cli), checked
  2026-07-11): `compare`, `filter`, `merge`, `migrate`, `optimize` (chunk compaction),
  `print`, `route`, `split`, `stats`, `verify`. `merge` matters for us: per-producer `.rrd`
  files sharing a `recording_id` can be merged offline. `stats`/`verify` are used in
  07-record-replay.md.

## 4. Viewer / operational facts (headless-relevant)

All from the [CLI manual](https://rerun.io/docs/reference/cli) and
[0.34.0 release notes](https://github.com/rerun-io/rerun/releases/tag/0.34.0), checked 2026-07-11.

- **gRPC proxy**: viewer listens on port **9876** by default; SDK URL scheme
  `rerun+http://127.0.0.1:9876/proxy` (`rerun://`, `rerun+http://`, `rerun+https://`
  supported). The CLI manual documents **no authentication or encryption** for the plain
  `rerun+http` proxy endpoint. *Deployment finding: a live adapter→viewer link must be
  loopback or an operator-controlled network; do not expose 9876.*
- **`--serve-web`**: hosts a web viewer over HTTP plus a gRPC proxy that forwards SDK
  connections to viewers — the natural "remote GPU-less server, browser on the operator's
  laptop" topology. Needs viewer-carrying binaries (not our sdk-only build); marked
  **assess on GPU box**.
- **`--memory-limit`**: viewer-side ring buffer — on reaching the limit "Rerun will drop the
  oldest data" (accepts `16GB`, `50%`). This is the built-in answer to unbounded live
  streaming; record mode has no such cap (file grows monotonically).
- **Headless viewer mode is new in 0.34**: the viewer can run with no OS window
  (`--headless`), driven programmatically (screenshot API / Viewer MCP)
  ([0.34.0 release notes](https://github.com/rerun-io/rerun/releases/tag/0.34.0)). A
  `--renderer` override exists (`vulkan`, `gl`, `metal` native); pure-software rendering is
  **not documented** — headless mode still exercises the GPU stack offscreen, so this
  sandbox (no GPU, no viewer binary) still cannot run it. Viewer-side claims stay
  "assess on GPU box".
- **Multi-producer semantics**
  ([apps-and-recordings](https://rerun.io/docs/concepts/apps-and-recordings), checked
  2026-07-11): recordings sharing a `recording_id` + `application_id` are "treated as a single
  logical recording" even across processes/machines; a **random recording_id is generated per
  process by default**, so distributed producers *must* coordinate an explicit id. The viewer
  groups recordings (and stores blueprints) by `application_id`. Details in
  08-multi-process.md.

## 5. Breaking-change velocity (0.23 → 0.34)

The [migration index](https://rerun.io/docs/reference/migration) (checked 2026-07-11) lists a
dedicated migration guide for **every minor release** — twelve guides between 0.22→0.23 and
0.33→0.34, over roughly 15 months. Sampled 0.33→0.34
([guide](https://rerun.io/docs/reference/migration/migration-0-34)): default-timeline behavior
changed (`log_tick` no longer injected), a deprecated Python module removed outright, gRPC
services restructured (`SaveScreenshot` moved to a new `ViewerControlService`), stricter
dataframe ingestion. Combined with the fast MSRV ratchet (1.92 today):

> **Finding**: Rerun's API churns on a ~6-week cadence with real breaking changes each minor.
> An adapter isolated to one module (`rerun_sink.rs`) contains this; any deeper coupling
> (Rerun types in shared crates, `.rrd` as an archival format) would put ZenSight on that
> treadmill. This is a first-class input to the #430 decision.

## 6. Adopt / reject signal summary (running tally)

**Adopt signals**
- sdk-only build is viewer-free and analytics-free by construction (§1) — clean for offline use.
- Time-series + text-log + graph archetypes map 1:1 onto ZenSight's domain (§2.3).
- Absolute-epoch timelines (`set_timestamp_nanos_since_epoch`) fit our domain-timestamp rule (§2.2).
- `set_sinks` gives live+record simultaneously; `rrd merge/stats/verify` is a real ops toolchain (§3).
- Multi-producer story (shared `recording_id`) matches a sensors-fleet topology (§4).

**Reject signals**
- MSRV 1.92 and ~6-week breaking-release cadence (§1, §5).
- `.rrd` N→N+1-only compatibility → unsuitable as a long-term archive without re-migration (§3).
- Arrow + tonic dependency weight (measured below) for what is, for us, scalars-and-text (§1).
- gRPC proxy unauthenticated/unencrypted (§4).
- Viewer assessment impossible headless — half the value proposition (the viewer) is
  unverifiable in this environment and defers to the GPU box (§4).

## Appendix: dependency & build evidence (commit 4)

*Appended at commit 4 (#419) after the adapter crate first builds — `cargo tree` hygiene gate
output and target-dir cost measurements land here.*
