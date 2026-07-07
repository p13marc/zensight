# Topology View Redesign

*2026-07-07 — design report. Companion audits: [`ZENOH-EFFICIENCY.md`](ZENOH-EFFICIENCY.md),
[`design/correlation.md`](design/correlation.md).*

## Executive summary

The topology view's **data plumbing is strong and its presentation is the bottleneck**. Nodes are
already entity-keyed through the correlator (one node per physical host, passive wire-only assets
included), edges are honestly derived from observed data (netring flows + netlink ARP/NDP), and
alerts tint both. But everything renders as **one flat, undirected, force-directed canvas** whose
node model reads ~8 hardcoded metrics — while the bus already carries directed traffic rates,
device roles, established-socket peers with process attribution, gateways, health snapshots, and
passive-DNS names that the graph never touches.

The redesign is therefore **not a data project — it's a presentation and interaction project**,
and it can be summarized in one sentence:

> Adopt the industry-converged recipe — *overview first, zoom & filter, details on demand*
> ([Shneiderman](https://infovis-wiki.net/wiki/Visual_Information-Seeking_Mantra)) — as
> **lenses** over a typed, directed, rate-weighted graph model, with **grouping/focus** to kill
> the hairball and a **side panel** that pivots into the data we already have.

Four phases, each independently shippable (§6). Phase 1 (typed graph model, directed rate edges,
asset roles, health) delivers the biggest visible improvement for the least risk.

---

## 1. Current state

### 1.1 What exists

| File | LOC | Role |
|---|---|---|
| `zensight/src/view/topology/mod.rs` | 1 633 | `TopologyState`, node/edge derivation, alert overlay, info panels (~480 LOC tests) |
| `zensight/src/view/topology/graph.rs` | 859 | `iced::widget::canvas` program: drawing + mouse/keyboard (~290 LOC tests) |
| `zensight/src/view/topology/layout.rs` | 367 | Force-directed layout (repulsion/attraction/centering/damping) |

What already works well — keep all of it:

- **Entity-keyed nodes.** `update_from_devices` (`mod.rs:82`) collapses per-protocol
  `DeviceState`s into one node per physical host via `EntityStore` (`zensight/src/entity.rs`),
  with a re-key migration path when correlation claims a source later, and a degraded path when
  the correlator is absent. `apply_entities` adds **passive wire-only nodes** for entities with no
  live sensor.
- **Observed-only edges** ("Hubble model", `mod.rs:167`): netring `FlowRecord`s aggregated per
  node pair (`edges_from_flows`) + netlink `NeighborRecord` adjacency (`edges_from_neighbors`,
  which also classifies `is_router` neighbors as `NodeType::Router`). Nothing is fabricated.
- **Alert overlay**: `apply_alerts` tints nodes by highest firing severity and edges by worst
  endpoint; feeds the selected-node panel.
- **Interaction basics**: drag-to-pin, pan, wheel/keyboard zoom, search highlight, 1 Hz refresh
  (`app.rs handle_tick`), canvas redraw cache.
- **Testing discipline**: layout, coordinate transforms, hit testing, and edge derivation are
  pure functions with unit tests.

### 1.2 The gap: data on the bus vs. data in the graph

| Available on the bus | Where | Used by topology today? |
|---|---|---|
| Directed traffic matrix with **bytes/sec** | netring `@/query/matrix?top=N` → `MatrixRecord{src,dst,bytes_per_sec,names}` | ✗ — edges use cumulative `FlowRecord.bytes`, undirected |
| Per-direction flow counters, `community_id`, dst names | netring `@/query/flows` → `FlowRecord` | partially (bytes only; direction, names, community_id dropped) |
| Device **roles** (router/switch/ap/phone/iot), vendor, platform, fingerprints | netring `@/query/assets` → `AssetRecord` (MAC-keyed, LLDP/CDP/mDNS-fed) | ✗ — `NodeType::Switch` exists but is never assigned |
| Established TCP peers **with owning process** | netlink `@/query/sockets` → `SocketRecord{local,remote,pid,process,rtt_us,…}` | ✗ |
| Flow↔process join (issue #309, done) | `zensight/src/view/specialized/attribution.rs` | ✗ — only the netring flow table uses it |
| Default gateways / nexthops | netlink `@/query/routes` → `RouteRecord{dst,gateway,oif}`; `routes/default_v4_gw` metric | ✗ |
| Sensor health | `@/health` → `HealthSnapshot`; `@/devices/*/liveness` | indirectly (per-device `is_healthy` bool) |
| Passive-DNS names for IPs | `HostEntity.names`, `_meta/query/names?ip=` | ✗ — labels use hostname/source only |
| Per-process/app bandwidth | netlink + netring `@/query/bandwidth?top=N` → `BandwidthRecord` | ✗ |
| Top talkers (rolling 60 s rate) | netring `@/query/talkers?top=N` → `TalkerRecord` | ✗ |
| Metric history (1 s hot ring + redb tiers) | `zensight/src/store.rs` | ✗ — no sparklines/trends in topology |
| systemd unit dependencies, per-unit egress/ingress bps | systemd `@/query/unit?name=` → `UnitDetail`; `unit/<n>/ip_*_bps` | ✗ (intra-host; drill-in material) |

### 1.3 UX problems (why "greatly redesign" is warranted)

1. **Hairball trajectory.** Every host and every passive asset is one flat node; every flow pair
   is one edge. On a real LAN, netring's asset discovery alone produces dozens–hundreds of
   passive nodes. There is no grouping, no collapse, no focus mode, no idle-edge filter — the
   exact failure mode the graph-vis literature warns about
   ([grooming the hairball](https://www.researchgate.net/publication/281050201_Grooming_the_hairball_-_how_to_tidy_up_network_visualizations)).
2. **Edges carry almost no information.** Undirected, width = cumulative bytes since sensor
   start (an ever-growing number, not a rate), one protocol label. You cannot see who is
   talking *to* whom, how fast, or whether it's happening *now*.
3. **All hosts look the same.** A router, a switch, a phone, and an IoT camera all render as the
   same circle unless netlink happens to flag a neighbor as router.
4. **Details are cramped, pivots absent.** The selected-node panel shows a few numbers; there is
   no path from a node/edge to the flow table, asset inventory, process attribution, alert
   detail, or device view (only a raw "device detail" jump).
5. **One view for three jobs.** Ops ("what's slow/down?"), security ("what's talking that
   shouldn't?"), and inventory ("what's on my network?") share a single undifferentiated render.
6. **Canvas is untestable.** `iced_test::simulator` drives widgets, not canvas geometry — the
   graph body has zero simulator coverage; only header widgets do.

---

## 2. What the reference tools do

Patterns extracted from tools that solved this exact problem, with what we should copy or skip.

### [Grafana node graph](https://grafana.com/docs/grafana/latest/visualizations/panels-visualizations/visualizations/node-graph/)
Nodes show a **main stat + secondary stat inside the circle**, name/type below, and a **colored
arc around the border** encoding a ratio (e.g. error share). Edges have hover stats and context
menus. Three layouts: **layered (default), force (500+ nodes), grid** (ranked overview of the
"most interesting" nodes). *Copy:* in-node stats, the arc ring, the grid layout as a ranked
fallback, edge hover stats. *Skip:* nothing — this is the closest match to our widget budget.

### [Kiali](https://kiali.io/docs/features/topology/) ([design doc](https://github.com/kiali/kiali-design/blob/master/service-graph/design.md))
Health as **stroke color** on nodes and edges (green/orange/red/grey-idle); an **edge-label
dropdown** (rate / latency / error %); **traffic animation** (moving dots, density ∝ rate, red
shapes for errors); **grouping with collapse** (app groups, versions); **Find and Hide**
expressions; hovering an element **dims everything unconnected**. *Copy:* edge-label selector,
health strokes, hover-dim, find/hide, grouping+collapse. *Adapt:* traffic animation is lovely but
is a polish-phase item on an iced canvas redrawn per frame.

### [Cilium Hubble UI](https://docs.cilium.io/en/stable/observability/hubble/hubble-ui/)
Service **cards** connected by **directed arrows**; cards list **access points (ports/protocols)**.
The graph shows only *observed* traffic — the same honesty rule we already follow. *Copy:*
direction arrows everywhere; listen-ports in the node side panel (we have `sockets?state=listen`).

### NDR tools ([Darktrace / ExtraHop](https://www.esecurityplanet.com/products/ndr-network-detection-response/))
Distinguish **north–south vs east–west** traffic; the map is an *investigation surface*: from an
alert straight to the entity, its connections, and the raw evidence in one or two clicks.
*Copy:* internal/external distinction (an "Internet" pseudo-node or edge badge for
off-subnet/public destinations), and alert→map→evidence pivots. We already have the evidence
chain (alerts, flows, `community_id`, artifacts).

### Network mappers ([LibreNMS](https://docs.librenms.org/Extensions/Network-Map/), [Juniper Paragon](https://www.juniper.net/documentation/us/en/software/juniper-paragon-automation2.0.0/user-guide/topics/concept/topology-visualization.html), [Selector](https://www.selector.ai/learning-center/network-visualization-tools-key-features-and-top-6-tools/))
Auto-discovery feeds the map (ours: netring assets + LLDP/CDP `seen_via`); **cluster views**
collapse devices/links into aggregates to stay navigable; metrics (utilization, loss, latency)
layer *onto* the map rather than living beside it. *Copy:* group-collapse into meta-nodes with
aggregated edges.

### Graph-vis literature
[Shneiderman's mantra](https://infovis-wiki.net/wiki/Visual_Information-Seeking_Mantra) is the
organizing principle for everything above. For hairballs: **aggregate, filter, or focus** —
sampling/coarsening and neighborhood highlighting beat cleverer layouts
([survey](https://www.researchgate.net/publication/281050201_Grooming_the_hairball_-_how_to_tidy_up_network_visualizations)).
On layout: Fruchterman–Reingold (what `layout.rs` implements) remains the standard interactive
choice ([overview](https://en.wikipedia.org/wiki/Force-directed_graph_drawing)); fancier methods
(stress majorization, t-FDP, GPU) only pay off at thousands of nodes. Rust crates
([petgraph](https://github.com/petgraph/petgraph), [fdg](https://github.com/grantshandy/fdg))
exist, but our bespoke ~230-line layout is well-tested and sufficient — **no new dependency
needed**.

**Design principles adopted:**

- **P1 — Overview first, zoom & filter, details on demand.** The default render must stay legible
  at LAN scale; depth lives behind hover/selection/panel, not on the canvas.
- **P2 — The map answers a question; different questions are different lenses,** not more ink on
  one render.
- **P3 — Show only observed data** (keep the existing rule), and show its *direction* and *rate*.
- **P4 — Every element pivots to its evidence** (flows, processes, assets, alerts, history).
- **P5 — Aggregate before you decorate.** Grouping/filtering outranks animation and icons.

---

## 3. Proposed design

### 3.1 Typed graph model (`view/topology/model.rs` — new, pure, unit-testable)

Split the data model out of `mod.rs` into a pure module (same discipline as
`edges_from_flows`/`layout_step` today — functions from records to graph, no iced types):

```rust
pub struct TopoNode {
    pub id: NodeId,                     // entity id | source (unchanged)
    pub label: String,                  // entity hostname → passive-DNS name → source
    pub role: NodeRole,                 // Host | Router | Switch | AccessPoint | Phone | Iot | Internet | Unknown
    pub provenance: Provenance,         // Monitored (has sensors) | Passive (wire-only) | External
    pub health: NodeHealth,             // Healthy | Degraded | Down | Stale  (from @/health + liveness)
    pub alert: Option<AlertSeverity>,   // unchanged
    pub stats: NodeStats,               // cpu, mem, rx/tx bps (rates!), tcp counts, sensor/metric counts
    pub group: Option<GroupId>,         // subnet | DeviceGroup | role  (per grouping mode)
    // position/velocity/pinned unchanged
}

pub struct TopoEdge {
    pub from: NodeId, pub to: NodeId,   // DIRECTED: from = initiator
    pub kind: EdgeKind,                 // Flow | L2Adjacency | Gateway
    pub rate_bps: f64,                  // from MatrixRecord.bytes_per_sec (Flow) else 0
    pub bytes: u64, pub packets: u64,   // cumulative, kept for the panel
    pub reverse_rate_bps: f64,          // responder direction (FlowRecord initiator/responder split)
    pub protocol: Option<String>,
    pub last_seen: i64,
    pub alert: Option<AlertSeverity>,
}
```

Sources per field:

- **`role`** — netring `AssetRecord.role` joined via `EntityStore.by_mac`/`by_ip` (assets are
  MAC-keyed); netlink `NeighborRecord.is_router` stays as the fallback it is today. `Internet` is
  a synthetic role for the external aggregate (§3.3).
- **`health`** — `@/health` `HealthSnapshot` + `@/devices/*/liveness`, replacing the single
  `is_healthy` bool. `Stale` when the entity is past `ENTITY_STALE_MS` — today staleness is
  invisible.
- **`stats` rates** — rx/tx **bps** from counter deltas over the hot ring (`store.rs` has
  1 s-resolution samples; a 2-point delta is enough), not raw cumulative counters.
- **Flow edges** — primary source becomes `matrix?top=N` (`MatrixRecord{src,dst,bytes_per_sec}`):
  directed, pre-rated, pre-aggregated, already top-N-capped server-side. `flows` remains the
  drill-down source for the edge panel. Neighbor and gateway edges are cheap constants.
- **`Gateway` edges** — from each monitored host to its `routes/default_v4_gw` node (creates the
  router node if only known by IP). This single edge kind makes the physical topology readable
  even on quiet networks, where flow edges are sparse.

The existing `last_flows`/`last_neighbors` rebuild pattern generalizes to
`last_matrix`/`last_assets` — same query-on-open + on-tick refresh flow through
`TopologyFlowsReceived`-style messages (`app.rs:664` block).

### 3.2 Lenses (P2)

A segmented control in the header (reuse `components/tabs.rs` `tabbed_view` or a small variant),
switching *emphasis*, not data:

| Lens | Nodes | Edges | Extra |
|---|---|---|---|
| **Traffic** (default) | role icon + health ring, rx/tx in-node stats | Flow edges, width = log(rate), arrowheads; label per edge-label mode | edge-label dropdown: **bps / pkts / protocol / none** |
| **Security** | tint = alert severity; badge = firing count; passive nodes emphasized (unknown = interesting) | edges with alerts highlighted; others dimmed | filter chips per `AlertKind`; anomaly sources ringed |
| **L2 / Physical** | label ⇒ MAC + vendor; switches/APs emphasized | `L2Adjacency` + `Gateway` only (no flow noise) | subnet grouping on by default |
| **Health** | tint = `NodeHealth`; stale ghosted | edges neutral, dimmed | sensor-count badge; liveness age in tooltip |

Lens = a small `LensSpec` struct (which edge kinds, node tint source, label mode) interpreted by
the draw code — not four draw paths. Selected lens persists in the redb local store.

### 3.3 Hairball control (P5 — the most important section)

- **Group & collapse.** Grouping modes: **none / subnet / role / DeviceGroup** (`view/groups.rs`
  already exists and is unused by topology). A collapsed group renders as one meta-node
  (member count badge, worst health/alert bubbled up); edges between groups aggregate
  (summed rates, worst alert). Double-click expands/collapses. This is the LibreNMS/Kiali
  cluster-view pattern and the single best defense at LAN scale.
- **External aggregate.** Public/off-subnet destinations collapse into one **Internet**
  pseudo-node per lens (NDR north–south pattern). An expanded mode can split it by
  `dst_names`/ASN later; the default keeps every external SaaS endpoint from becoming a node.
- **Focus mode.** Double-click a node (or panel button) → show only its N-hop neighborhood
  (N = 1 default, stepper to 3), rest hidden, breadcrumb to exit. Cheap BFS on the model.
- **Filters** (header chips, persisted): hide passive assets · hide idle edges
  (`last_seen` > 5 min) · min-rate slider · top-N edges (default 50, honest label
  "showing top 50 of 213 edges" — no silent truncation).
- **Find & hide** (Kiali): extend the existing search box with `find:` highlighting and `hide:`
  removal over simple predicates (`role:iot`, `alert:critical`, name substring).

### 3.4 Visual encoding

All colors through `theme.rs` topology tokens (the design-system CI guard already enforces this);
new tokens needed: `topology_edge_flow`, `topology_edge_l2`, `topology_edge_gateway`,
`topology_node_stale`, `topology_focus_dim`, and role icon tints.

- **Node** = circle (radius by zoom, slightly larger for routers/switches/groups) +
  **health-colored ring** (Kiali stroke pattern) + **role glyph** inside (iced `svg`/`text`
  glyph) + alert badge (count) top-right + in-node main stat when zoom ≥ 0.6 (Grafana pattern;
  stat per lens). Passive = dashed ring (as today); stale = ghosted.
- **Edge** = quadratic curve with **arrowhead** at 2/3 length; width = `2 + log10(rate_bps)`
  clamped; two-way traffic = double arrowhead (or two offset curves if cheap enough);
  `L2Adjacency` = thin dotted, `Gateway` = thin dashed; label per edge-label mode at midpoint.
- **Hover** = highlight node + its edges + neighbors, dim the rest (Kiali). Pure model
  computation + one cache clear.
- **Legend** — small collapsible widget card (design-system components), keyed to active lens.

### 3.5 Details on demand: the side panel (P4)

Replace the cramped info box with a right-side panel (~320 px, widget-based, scrollable, reusing
`kit.rs` cards + `Sparkline` + `DataTable`):

**Node panel** — sections, each with a pivot:

1. **Identity** — entity name, role, vendor/platform, IPs/MACs, passive-DNS names
   (`HostEntity.names`), member claims with rule+confidence (`members[]`) — surfacing correlator
   *evidence*, which today is invisible in the GUI.
2. **Vitals** — CPU/mem gauges + 1 h **sparklines** from the tiered store; rx/tx bps.
3. **Traffic** — top talkers to/from this node (filter `last_matrix` client-side); "Open flows ↗"
   → netring flow table pre-filtered (the `NetringAssetToTopology` pivot already exists —
   add the reverse direction).
4. **Listening** — listen sockets with process names (`sockets?state=listen`, on-demand fetch on
   selection).
5. **Alerts** — firing list (exists) + "Open alert ↗".
6. Buttons: **Focus** · **Pin/Unpin** · **Hide** · **Device detail ↗**.

**Edge panel** (today edges are selectable but nearly mute):

1. Per-direction rates + cumulative bytes/packets, protocol split, first/last seen.
2. Top flows on this pair (`flows` filtered by the two endpoints' IPs) in a mini `DataTable`.
3. **Process attribution** — reuse `specialized/attribution.rs` (#309): resolve the flow 5-tuple
   to owning processes on both monitored endpoints. This turns "these two boxes talk" into
   "*nginx* on web1 talks to *postgres* on db1" — the single highest-value pivot in the redesign.
4. `community_id` (copyable, for Zeek/Suricata cross-referencing) + "Open in flow table ↗".

On-demand fetches go through the existing async query pattern (`query_topology_flows` →
`Message::…Received`), triggered by selection, never by tick.

### 3.6 Interaction & layout

- Keep: drag-pin, pan, wheel zoom, `+`/`-`/`0`, Esc. Add: **zoom-to-fit** (button + `f`),
  double-click = focus/expand-collapse, hover-dim.
- **Persist pins & positions** per node id in the redb local store (positions are lost on
  restart today; manual arrangement must survive).
- **Layout modes**: force (default, current algorithm — keep, it's tested and adequate below
  ~500 nodes per the [literature](https://en.wikipedia.org/wiki/Force-directed_graph_drawing)) ·
  **grid** ranked by rate/alert (Grafana's "most interesting first" overview) · **circular**
  (current seeding, as a stable deterministic option). Seed force layout by group so clusters
  don't start interleaved.
- Layout stays on the 1 Hz tick with the `layout_stable` gate (unchanged); grouping shrinks the
  node count, which is the real performance lever. No Barnes–Hut/quadtree until profiling says
  otherwise.

### 3.7 Testability

- **Model tests** — `model.rs` is pure: matrix→edges, role join, health mapping, grouping
  aggregation, focus BFS, filter predicates all unit-testable like `edges_from_flows` today.
- **Simulator coverage** — everything new *outside* the canvas is widgets: lens selector, filter
  chips, edge-label dropdown, legend, side panel, layout switcher. Each gets
  `iced_test::simulator` tests (`ui_tests.rs` pattern: `ui.click("Security")` →
  `Message::TopologySetLens(Lens::Security)`).
- **Canvas stays thin** — draw code consumes a fully-resolved `RenderPlan` (positions, colors,
  labels) computed in testable code; `graph.rs` keeps only geometry + hit tests (already
  unit-tested free functions).

---

## 4. What this looks like

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ⟵ Back   Topology     [Traffic][Security][L2][Health]    edge: bps ▾   ⌕ find │
│ group: subnet ▾ │ ☑ hide idle │ ☐ passive │ top 50 ▾ │ layout: force ▾ │ ⤢ fit│
├──────────────────────────────────────────────────────────────┬───────────────┤
│                                                              │ web1 · Host   │
│        ┌─────────────┐                ╭─────╮                │ ● healthy  ⚠1 │
│        │ 10.0.2.0/24 │◀━━ 2.1 MB/s ━━▶│ gw  │──▶(Internet)   │───────────────│
│        │  (12 hosts) │                ╰──┬──╯                │ Identity      │
│        └─────────────┘                   ┆ gateway           │  fqdn, 2 IPs  │
│                        ╭──────╮       ╭──┴──╮                │  3 sensors    │
│          143 kB/s ──▶  │ web1 │ ━━━▶  │ db1 │                │ Vitals        │
│                        ╰──────╯ 890kB ╰─────╯                │  cpu ▂▃▅▂ 34% │
│                            ┆ l2         ⚠                    │ Traffic       │
│                        ╭┄┄┄┄┄┄╮                              │  → db1 890kB/s│
│                        ┊ cam? ┊  (passive, dashed)           │ Listening     │
│                        ╰┄┄┄┄┄┄╯                              │  :443 nginx   │
│  legend ▾                                                    │ [Focus][Pin]  │
└──────────────────────────────────────────────────────────────┴───────────────┘
```

---

## 5. Non-goals / deferred

- **Geo/site map** — no geo data on the bus; revisit if multi-site becomes real.
- **Time scrubbing** ("topology as of yesterday") — the tiered store could support it, but it's
  a separate project; the panel sparklines cover the common "when did this start" question.
- **Intra-host service graph** (systemd unit deps as sub-topology) — belongs in the device
  detail's systemd view; topology links to it, doesn't render it.
- **Per-port access-point cards** (full Hubble style) — listen ports live in the side panel
  instead; card-style nodes don't fit the canvas budget.
- **New layout/graph dependencies** (petgraph/fdg) — current bespoke layout is tested and
  sufficient; re-evaluate only if profiling shows layout cost at scale.
- **Traffic-dot animation at high fidelity** — Phase 4 dashed-line animation only; per-request
  dots (Kiali) don't map to sampled flow data.

## 6. Phased roadmap (GH-epic-ready)

Each phase is independently shippable and demoable; later phases don't rework earlier ones.

**Phase 1 — Typed graph model & honest edges** *(highest value / lowest risk)*
- Extract `view/topology/model.rs`; introduce `EdgeKind`, `NodeRole`, `Provenance`, `NodeHealth`.
- Edges from `matrix?top=N` (directed, bps-weighted) + arrowheads + log-width; keep `flows` for
  drill-down; add `Gateway` edges from `routes/default_v4_gw`.
- Node roles from `AssetRecord` (join via `EntityStore.by_mac/by_ip`); health from `@/health` +
  liveness incl. `Stale`; rx/tx as rates from the hot ring.
- New theme tokens; update `zensight/docs/views.md`.
- Files: `view/topology/{model.rs (new), mod.rs, graph.rs}`, `app.rs` (queries),
  `theme.rs`, tests inline + `ui_tests.rs`.

**Phase 2 — Lenses, filters, grouping** *(the "greatly redesign" payoff)*
- Lens selector (Traffic/Security/L2/Health) + edge-label dropdown; `LensSpec` interpretation.
- Grouping modes (subnet/role/DeviceGroup) with collapse into meta-nodes + aggregated edges;
  Internet pseudo-node; focus mode (N-hop BFS); filter chips + top-N with honest count;
  find/hide predicates.
- Persist lens/filters/grouping in redb local store.
- Files: `model.rs`, `mod.rs`, `graph.rs`, header widgets, `store.rs` (prefs), simulator tests.

**Phase 3 — Details on demand**
- Side panel (node + edge variants) with identity/evidence, sparklines, top talkers, listen
  sockets, alerts; on-demand `sockets`/`flows` fetches on selection.
- Process attribution on edges (reuse `specialized/attribution.rs`); pivots to flow table /
  device detail / alerts; `community_id` copy.
- Files: `view/topology/panel.rs` (new), `app.rs` (fetch messages), reuse
  `components/{kit,sparkline,data_table}.rs`.

**Phase 4 — Polish**
- Hover-dim neighborhood; dashed-flow animation on active edges (rate-gated); legend card;
  grid + circular layout modes; zoom-to-fit; persist pins/positions; group-seeded layout.

## 7. Sources

- [Shneiderman — Visual Information-Seeking Mantra](https://infovis-wiki.net/wiki/Visual_Information-Seeking_Mantra)
- [Grafana node graph docs](https://grafana.com/docs/grafana/latest/visualizations/panels-visualizations/visualizations/node-graph/)
- [Kiali topology features](https://kiali.io/docs/features/topology/) · [Kiali service-graph design doc](https://github.com/kiali/kiali-design/blob/master/service-graph/design.md)
- [Cilium Hubble Service Map & UI](https://docs.cilium.io/en/stable/observability/hubble/hubble-ui/)
- [Skydive — real-time network analyzer](https://skydive.network/)
- [eSecurity Planet — NDR solutions overview](https://www.esecurityplanet.com/products/ndr-network-detection-response/) · [Exabeam — NDR key features](https://www.exabeam.com/explainers/network-detection-and-response/ndr-solutions-key-features-and-7-tools-to-know-in-2025/)
- [Selector — network visualization tools & key features](https://www.selector.ai/learning-center/network-visualization-tools-key-features-and-top-6-tools/)
- [Juniper Paragon — topology visualization (cluster view)](https://www.juniper.net/documentation/us/en/software/juniper-paragon-automation2.0.0/user-guide/topics/concept/topology-visualization.html) · [LibreNMS network map](https://docs.librenms.org/Extensions/Network-Map/)
- [Grooming the hairball — tidying network visualizations](https://www.researchgate.net/publication/281050201_Grooming_the_hairball_-_how_to_tidy_up_network_visualizations) · [Force-directed graph drawing](https://en.wikipedia.org/wiki/Force-directed_graph_drawing)
- Rust ecosystem: [petgraph](https://github.com/petgraph/petgraph) · [fdg](https://github.com/grantshandy/fdg) (evaluated, not adopted)
