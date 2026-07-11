# Topology → GraphNodes/GraphEdges (#425)

Mapping ZenSight's host topology onto Rerun's graph archetypes
([GraphNodes/GraphEdges](https://docs.rs/rerun/0.34.1/rerun/archetypes/struct.GraphNodes.html),
pinned in [01-capabilities.md](01-capabilities.md) §2.3), and an honest statement of what a
headless environment can and cannot conclude about it.

> Scope guard: this does **not** replace the Iced topology view (epic #395 — render-graph
> architecture, lenses, grouping). It evaluates whether Rerun could carry a *time-scrubbable*
> topology alongside the recorded telemetry — something the live frontend deliberately
> doesn't do.

## 1. Model (`topology.rs`, Rerun-free)

- `TopoNode { id, label, kind (Host|Peer), status (Up|Degraded|Down|Unknown) }`,
  `TopoEdge { from, to, up }`, materialized by a `TopologyBuilder` that folds the inputs the
  adapter already consumes:
  - a `HostEntity` → one `Host` node (id = `entity_id`, label = hostname|fqdn|id, status from
    the entity's rolled-up status);
  - link/peer events (`PeerUp`/`PeerDown`/`LinkDegraded` with a `target`) → host→peer edges
    flipping `up`; unknown endpoints auto-materialize as `Peer` nodes (uncorrelated sources
    as `src:<source>` standalone nodes).
  - (Per-interface nodes from `iface/<if>/oper_state` were considered and dropped for this
    phase — they multiply node count without changing what the evaluation can conclude.)
- `upsert_entity`/`apply_event` report *change*, and only changes re-publish the graph.
- Emission (in `rerun_sink.rs`): the whole graph is re-logged on `topology/hosts` at the
  domain timestamp whenever it changes — `GraphNodes::new(ids).with_labels(...).with_colors(...)`
  + `GraphEdges::new(pairs).with_directed_edges()` in one log call. Rerun treats each
  timepoint's log as the graph state at that time, which is exactly what makes the graph
  **scrubbable**: drag the timeline over a link-down event and the graph updates.
- Node colors encode status (up/degraded/down/unknown) with the same severity palette as the
  alert lanes.

## 2. Demo + observed (2026-07-11, headless record mode)

`zensight-rerun-demo incident` (see [06-incident.md](06-incident.md)) publishes the demo
entity (graph gets its host node) and drives a synthetic **gateway link down (t+26) → up
(t+55)** around the failover; record mode captures the graph transitions in the same `.rrd`
as the metric ramps and events:

```text
adapter: topology_published=3  sink_errors=0
         # 1 entity upsert + linkdown flip + linkup flip; the route-change
         # event correctly does NOT re-publish the graph
incident-topo.rrd: 116071 bytes (vs 95424 without the topology lane)
```

**Expressiveness gap found while implementing:** `GraphEdges` in 0.34 has **no per-edge
styling** (the archetype carries only the edge list + directedness — no colors, no widths,
no labels). A downed link therefore renders by *omitting* the edge (the peer node stays,
colored Down/red) — scrubbing over the link-down event makes the edge disappear rather than
turn red. Honest but lossy; per-edge state is exactly what a network topology wants.

## 3. Headless-assessable vs needs-viewer

Assessable now (and asserted in tests):

- The fold from entities/events to nodes+edges (pure, unit-tested).
- Graph emission API shape: node ids are strings, edges are (from, to) pairs; per-node
  labels/colors exist; `with_directed_edges()` vs undirected. Positions are optional —
  omitted, the viewer force-layouts.
- Chunk cost of re-logging the full graph per change (visible in `rerun rrd stats`).

Needs the viewer (**assess on GPU box**):

- [ ] Force-layout stability across time scrubbing (does the layout jump frame-to-frame?).
- [ ] Readability at fleet scale (ZenSight labs run 3–10 hosts; the Iced view's lenses/
      grouping have no Rerun analogue — no hierarchical grouping in the Graph view).
- [ ] Interaction: selecting a node → does the selection panel show anything useful next to
      the host's metric lanes? (No cross-view "focus this host's series" linkage exists —
      expected gap.)
- [ ] Whether per-timepoint full-graph re-log (vs component-level partial updates) scrubs
      smoothly.

## 4. Early verdict input for #430

The graph mapping is cheap (one small pure module) and the time-scrub property is genuinely
novel vs. the Iced view. But the Iced topology's operator features (lenses, grouping, alert
tinting, drill-down) have no path into Rerun's Graph view — this lane is a *replay
supplement*, not a topology tool. If the viewer assessment shows unstable layouts at scrub
time, drop the lane entirely rather than invest further.
