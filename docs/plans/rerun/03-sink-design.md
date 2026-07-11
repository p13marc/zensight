# The `VisualizationSink` abstraction (#418)

Design for the seam that keeps the adapter honest: everything upstream of the sink trait is
Rerun-free, and the trait is narrow enough that a `TestSink` can assert on exactly what Rerun
would have received.

## 1. The trait

```rust
pub trait VisualizationSink: Send {
    fn publish_metric(&mut self, point: &TelemetryPoint, path: &str, value: f64) -> anyhow::Result<()>;
    fn publish_event(&mut self, event: &NormalizedEvent, path: &str) -> anyhow::Result<()>;
    fn publish_alert(&mut self, alert: &Alert, path: &str) -> anyhow::Result<()>;
    fn publish_entity(&mut self, entity: &HostEntity) -> anyhow::Result<()>;
    fn flush(&mut self) -> anyhow::Result<()>;
}
```

Deliberate choices:

- **Conversion happens *before* the trait.** Entity-path building, `EntityIndex` lookup,
  counter→rate conversion, sampling, and classification all run in the `SinkWorker` — the
  trait receives the final `path` and the final `f64`. So the `TestSink` (a `Vec` recorder)
  sees byte-for-byte what `RerunSink` sees, and every mapping rule in
  [02-mapping.md](02-mapping.md) is unit-testable without the `rerun` dependency.
- **`&mut self`, sync, `Send`**: the worker owns the sink on one task; Rerun's
  `RecordingStream` does its own internal batching/threading, so no async plumbing is needed
  at this seam.
- **Domain types in, not Rerun types**: `publish_alert(&Alert, path)` rather than
  `publish_text_log(...)` — the sink decides the archetype split (TextLog + state series),
  because that split *is* Rerun-specific.

## 2. Placement: in the adapter, not in a shared crate

The exporters (`zensight-exporter-{prometheus,otel}`) already duplicate the subscribe/decode/
filter skeleton and their `is_telemetry_key` guard by design — shared extraction was
explicitly deferred (#418 keeps that stance). This evaluation must not create a
`zensight-exporter-core` on speculation: the trait lives in `zensight-rerun/src/sink.rs`. If
the #430 decision is "adopt", extracting a shared consumer/sink skeleton becomes a scored
follow-up with three real consumers to generalize from; if "reject", nothing leaked.

## 3. Pipeline & queueing

```text
zenoh subscribers (subscriber.rs)          SinkWorker (sink.rs)                sink
  telemetry  zensight/**          ──────→ tx_telemetry (mpsc 4096, drop-newest) ─┐
  alerts     zensight/*/@/alerts/*  ──┐                                          ├→ classify/convert ──→ VisualizationSink
  health     zensight/*/*/@/health  ──┤→ tx_control   (mpsc 1024, blocking)    ──┘    (mapping.rs, events.rs)
  entities   _meta/entity/** + seed ──┘
```

- **Two bounded channels**, mirroring the bus QoS philosophy (`zensight-common/src/qos.rs`):
  - telemetry (4096): **drop-newest** on full (`try_send`; a dropped sample is superseded by
    the next anyway) with a dropped-samples counter surfaced in adapter stats/logs. The
    subscriber task must never block on a slow sink.
  - control (1024): alerts / health transitions / entities **block** (`send().await`) — they
    are rare and must-arrive, same reasoning as `QosClass::Alert`'s `CongestionControl::Block`.
- The worker drains telemetry and control with `select!` biased toward control, applies
  `classify()` → sampler → rate converter → path building, then calls the sink.
- Sink errors are counted and logged, not fatal: a wedged gRPC sink must not kill the
  recording of a `.rrd` (and vice versa in `both` mode Rerun owns that fan-out internally).

## 4. Back-pressure & sampling

Rerun's `RecordingStream` buffers internally and `connect_grpc` keeps buffering when the
viewer is away — memory grows. Two adapter-side controls:

- **`sampling.max_hz_per_series`** (+ optional per-prefix overrides): a per-series token
  check *before* conversion; sub-sampled series drop intermediate points (fine for gauges;
  the rate converter runs *after* sampling so rates stay correct over the longer window).
- The viewer side is capped by `--memory-limit` (oldest-first drop, see 01-capabilities §4);
  record mode has no cap — [07-record-replay.md](07-record-replay.md) measures growth.

## 5. Testability

- `TestSink` records `(path, value, timestamp)` / events / alerts into vecs; unit tests drive
  the worker with synthetic `TelemetryPoint`s and assert the full mapping pipeline.
- The integration test (commit 7) swaps in the real `RerunSink` in record mode — the only
  place Rerun runs in CI — and asserts on the produced `.rrd` file.
