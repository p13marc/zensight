# zensight-rerun

> **Decided: an optional debugging backend (#430).** The evaluation (epic #415)
> concluded on 2026-08-26. This crate stays in-tree, `publish = false`, out of
> the release train, and off unless you run the binary. It is **supported for
> bounded incident capture and replay** and for nothing else. The reasoning,
> the costs, and what was never measured:
> [`docs/plans/rerun/DECISION.md`](../docs/plans/rerun/DECISION.md).

## What this is for, and what it is not

**Supported**: capturing a bounded incident, replaying it offline, handing a
`.rrd` to a colleague, and scrubbing backwards across metrics, alerts, events
and topology on one time axis — which the Iced frontend cannot do.

**Not supported**: continuous recording, always-on live monitoring, `.rrd` as an
archive across Rerun versions, or anything an operator *acts* on (alert
acknowledgement, configuration, artifacts — all of those are the frontend's).

**This adapter is a visualization, not a system of record.** The frontend and
its redb store are. If the adapter is down, the viewer has a gap: the bus does
not buffer for late consumers, so alert *state* can be missed entirely and
resyncs only at the next transition.

## Three rules, every time

1. **`--bind 127.0.0.1`.** `rerun --serve-web` binds the web viewer **and** the
   gRPC proxy to `0.0.0.0` by default, and that transport is unauthenticated and
   unencrypted while the payload is hostnames, IPs, MACs, flow matrices and log
   lines. Remote viewing is an SSH tunnel, not a bind address.
2. **`rerun rrd optimize` before you store or share a recording.** Live writes
   cost ~1.3 KiB per scalar point; compaction takes that to ~100 B — 13x on our
   own data — and it doubles as the repair tool for a `kill -9`-truncated file.
3. **`--memory-limit`** on any session that outlives a demo. The viewer keeps
   its store in RAM.

Treat a `.rrd` as you would treat a packet capture: it is bulk telemetry, and
nobody has yet audited which fields reach it (see DECISION.md §7).

A standalone adapter that consumes the Zenoh bus exactly like the exporters do — telemetry
(`zensight/v1/*/telemetry/**`), alerts (`zensight/v1/*/state/*/alert/*`), health
(`zensight/v1/*/state/*/health`), and correlated host entities
(`zensight/v1/@catalog/state/entity/*`, plus a one-shot storage-shaped GET seed on the same
selector) — and feeds a Rerun recording stream:
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
| `zensight/v1/<origin>/telemetry/<proto>/…` (source correlated) | `hosts/<entity_id>/<proto>/<metric>` |
| `zensight/v1/<origin>/telemetry/<proto>/…` (uncorrelated) | `sensors/<proto>/<source>/<metric>` |
| `zensight/v1/<origin>/state/<proto>/alert/<key>` | `alerts/<proto>/<source>/<alert_key>` (+ `/state` lane) |
| health transitions | `health/<sensor>/<source>` |

Full mapping: [`docs/plans/rerun/02-mapping.md`](../docs/plans/rerun/02-mapping.md).

## Configuration

JSON5, [`configs/rerun.json5`](../configs/rerun.json5): shared `zenoh` block
(env-overridable), `rerun` sink block (`mode`, `viewer_url`, `rrd_path`, `application_id`,
`recording_id`, `counters`), `filters`, `sampling`, `isolate`, `logging`.
