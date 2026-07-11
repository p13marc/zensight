# zensight-rerun

> **PROTOTYPE — evaluation only (epic #415).** This crate exists to evaluate
> [Rerun](https://rerun.io) 0.34 as an *optional* visualization/replay backend for ZenSight.
> It is `publish = false`, nothing depends on it, and it may be deleted wholesale if the
> evaluation concludes "do not adopt". Design notes and findings: [`docs/plans/rerun/`](../docs/plans/rerun/).

A standalone adapter that consumes the Zenoh bus exactly like the exporters do — telemetry
(`zensight/**`), alerts (`zensight/*/@/alerts/*`), health (`zensight/*/*/@/health`), and
correlated host entities (`zensight/_meta/entity/**`) — and feeds a Rerun recording stream:
live to a viewer over gRPC, into a `.rrd` file, or both.

Layering rule (grep-gated): `src/rerun_sink.rs` is the **only** module that may `use rerun`.
Everything else (classification, entity paths, counter→rate, sampling, event normalization)
is Rerun-free and unit-tested through the `VisualizationSink` seam. The gate:

```bash
grep -rn '\brerun::' zensight-rerun/src | grep -v rerun_sink.rs   # must be empty
cargo tree -p zensight-rerun | grep -Ei "re_viewer|wgpu|egui|re_renderer"  # must be empty
```

## Cookbook

```bash
# Live: stream into a Rerun viewer running on this machine
rerun --port 9876 &                      # on a GPU box; NOT possible headless
cargo run -p zensight-rerun --release -- --config configs/rerun.json5

# Record headless: write a .rrd, replay it later on any machine with a viewer
cargo run -p zensight-rerun --release -- --config configs/rerun.json5 \
    --mode record --rrd-path /tmp/zensight.rrd

# Isolated demo session (no ambient sensor traffic): adapter listens on a
# loopback endpoint, scouting off; demos/sensors connect to it explicitly
ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:7449 \
cargo run -p zensight-rerun --release -- --mode record --rrd-path /tmp/demo.rrd --isolate

# Inspect a recording without a GPU
rerun rrd stats /tmp/zensight.rrd
rerun rrd verify /tmp/zensight.rrd

# Replay on a GPU box
rerun /tmp/zensight.rrd
```

Demo scenarios (synthetic publishers, `--bin zensight-rerun-demo`) and the deterministic
correlated-incident script are documented in
[`docs/plans/rerun/06-incident.md`](../docs/plans/rerun/06-incident.md).

## Entity-path scheme

| Bus | Rerun |
|---|---|
| `zensight/<proto>/<source>/<metric>` (correlated) | `hosts/<entity_id>/<proto>/<metric>` |
| `zensight/<proto>/<source>/<metric>` (uncorrelated) | `sensors/<proto>/<source>/<metric>` |
| `zensight/<proto>/@/alerts/<key>` | `alerts/<proto>/<rule>` (+ `/state` lane) |
| health transitions | `health/<sensor>/<source>` |

Full mapping: [`docs/plans/rerun/02-mapping.md`](../docs/plans/rerun/02-mapping.md).

## Configuration

JSON5, [`configs/rerun.json5`](../configs/rerun.json5): shared `zenoh` block
(env-overridable), `rerun` sink block (`mode`, `viewer_url`, `rrd_path`, `application_id`,
`recording_id`, `counters`), `filters`, `sampling`, `isolate`, `logging`.
