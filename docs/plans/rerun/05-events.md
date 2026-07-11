# Structured event visualization (#421)

How discrete occurrences — the netlink control-plane timeline, netring detector hits, alert
transitions, health transitions — render in Rerun, and how the ergonomics compare with what
ZenSight's own frontend gives operators today.

## 1. What was built

- `events.rs` carries the full normalized catalogue (`EventKind`: `TcpOpen`, `TcpReset`,
  `TcpTimeout`, `IcmpUnreachable`, `PacketLoss`, `LinkDegraded`, `PeerUp`, `PeerDown`,
  `RouteChange`, `Restart`, `Anomaly`, `AlertFiring`, `AlertResolved`, `HealthChange`,
  `Other(tag)`) and the normalization paths:
  - `normalize_point` — `Text` telemetry and `events/…`-keyed telemetry (kind from the path
    chunk or an explicit `kind` label; `peer`/`target`, `iface`/`interface`,
    `correlation_id` labels lifted into typed fields, everything else into attributes);
  - `normalize_health` — health snapshots become events **only on status transitions**
    (severity from the target status); steady-state snapshots are suppressed;
  - alerts are normalized in the sink (`[FIRING]`/`[RESOLVED]` prefix, severity → level,
    resolved always `INFO`) plus the `…/state` severity step lane (02-mapping.md §5).
- `rerun_sink.rs` emits each event as **two log calls on the same entity path**:
  1. `TextLog::new(message).with_level(level)` — the human-readable lane;
  2. an **`AnyValues`** bundle carrying the structured fields (`kind`, `source`, `protocol`,
     `target`, `interface`, `correlation_id`, `entity_id`, plus every attribute) so the
     selection panel and dataframe queries see typed columns, not just prose.
- `zensight-rerun-demo events` — publishes a realistic mixed sequence (route changes, peer
  up/down, a TCP reset, an anomaly) at a steady pace, then a **burst** (default 50 events in
  ~1 s) to observe high-rate behavior; plus one alert firing→resolved pair and a health
  degradation, all through the real bus contract (declared publishers, `@/alerts/*` keyed
  puts).

## 2. Observed (2026-07-11, headless record mode, debug build)

`zensight-rerun --mode record --isolate` + `zensight-rerun-demo events --burst 50`:

```text
demo:    events=55 (5 scripted + 50 burst)  alerts=2 (firing→resolved)  health=2
adapter: events=57  alerts=2  sink_errors=0     # 57 = 55 + 2 health *transitions*
events.rrd: 69624 bytes
```

- Health steady-state suppression works as designed: two snapshots → two transition events
  (healthy baseline + degradation); a rerun of the same status would emit nothing.
- The alert pair produced the `[FIRING]`/`[RESOLVED]` TextLogs, the `…/state` step lane
  (2.0 → 0.0) and the structured `AnyValues` columns without sink errors.
- The 50-event burst (~20 ms apart, same entity path) recorded without error; whether
  same-millisecond `AnyValues` columns visually collide is a viewer question (below).

## 3. Honest ergonomics assessment

Headless-assessable (from the `.rrd` + API shape):

- **The two-call emission is a wart.** Rerun has no single "structured log record" archetype:
  `TextLog` carries text+level+color only, and `AnyValues` carries arbitrary fields but has no
  text-log rendering. Logging both on one path works (components merge on the entity), but it
  is producer-side convention, not a modeled concept — nothing stops the two from drifting.
  OTel's `LogRecord` (body + severity + attributes in one record) is strictly better modeled
  here; this is a per-event-visualization reject-signal worth weighing.
- **Attributes are per-timestamp state, not per-record.** Two events on the same path at the
  same millisecond overwrite each other's `AnyValues` columns (last-write-wins per timepoint
  per component). ZenSight timestamps are ms — bursts can collide. The demo's burst exists to
  measure exactly this. Mitigation options if adopted: nanosecond jitter on the timeline
  stamp, or per-kind child paths (`events/route_change`, …).
- **No retraction.** A mis-published event is permanent in the recording (append-only chunks);
  the alert `Delete` tombstone has no analogue and is deliberately ignored (02-mapping §5).
- **Severity filtering is viewer-side.** The TextLog view filters by level; there is no
  producer-side severity floor in the sink yet (config knob candidate if adopted).

Needs the viewer (**assess on GPU box** — out of scope headless):

- [ ] TextLog view rendering of level colors + the selection panel showing the `AnyValues`
      columns next to the text lane.
- [ ] Timeline scrubbing across metric lanes + event lanes: does the shared cursor make the
      "loss ramps → alert fires" story readable (this is the core value hypothesis)?
- [ ] Dataframe-view filter on `correlation_id` as the incident drill-down.
- [ ] Burst readability: 50 events/s in the TextLog view.

## 4. Comparison anchor (ZenSight frontend today)

The Iced frontend renders alerts as stateful rows (firing/resolved with auto-clear from
tombstones) and the netlink control-plane timeline as a dedicated view. Rerun's version is a
*recorded, scrubbable* history of the same — strictly better for post-hoc replay, strictly
worse as a live "what is firing right now" pane (no stateful row model, no tombstone
semantics). They are complements, not substitutes — consistent with the epic's "adapter
alongside the frontend, never replacing it" kill-switch.
