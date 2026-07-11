# Rerun evaluation (epic #415)

Working notes for evaluating [Rerun](https://rerun.io) 0.34 as an **optional** visualization /
replay backend for ZenSight, fed by a standalone adapter (`zensight-rerun/`) that consumes the
Zenoh bus exactly like the exporters do. This is an *evaluation*, not an adoption: every
document below records reject-signals as diligently as adopt-signals, and "do not adopt" is a
fully acceptable outcome.

Scope guards (kill-switches from the epic):

- The adapter is **evaluation-only**: `publish = false`, no other crate depends on it, no
  existing crate's source is modified.
- All Rerun types are confined to `zensight-rerun/src/rerun_sink.rs`; everything else in the
  adapter is Rerun-free and testable without the dependency.
- The Iced frontend (including the topology view, epic #395) is **not** replaced or changed.
- This environment is headless: anything that needs the Rerun viewer on a GPU box is marked
  "assess on GPU box" rather than silently assumed.

## Documents

| Doc | Issue | What it covers |
|-----|-------|----------------|
| [01-capabilities.md](01-capabilities.md) | #416 | Rerun 0.34 capabilities research snapshot — API pins, packaging, format, ops facts (primary-sourced, dated) |
| [02-mapping.md](02-mapping.md) | #417 | ZenSight → Rerun data mapping (telemetry, alerts, entities, events, timelines) |
| [03-sink-design.md](03-sink-design.md) | #418 | The `VisualizationSink` abstraction: trait, queueing, drop policy, testability |
| [04-live-metrics.md](04-live-metrics.md) | #420 | Live metric mapping, counter→rate policy, sampling |
| [05-events.md](05-events.md) | #421 | Structured event visualization (TextLog + attributes) |
| [06-incident.md](06-incident.md) | #422 | Deterministic correlated-incident demo |
| [07-record-replay.md](07-record-replay.md) | #424 | Record mode, headless `.rrd` verification, storage cost |
| [08-multi-process.md](08-multi-process.md) | #423 | Multi-process producer evaluation notes |
| [09-topology.md](09-topology.md) | #425 | Topology → GraphNodes/GraphEdges mapping |

The final go/no-go write-up (**#430**, `DECISION.md`) is intentionally **out of scope for this
PR** — it is written after the GPU-box viewer assessment that this headless phase cannot
perform.
