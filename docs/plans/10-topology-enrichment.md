# Plan 10 — Topology enrichment (measured edges + alert overlay)

**Goal:** turn the topology view from *inferred* into *measured*. Today
`TopologyState::update_from_devices` only makes nodes for `Protocol::Sysinfo` and
**guesses** edges from "network activity". With netlink neighbor/route data the
edges become real L2/L3 adjacency; with netring flow data they're sized by actual
bandwidth; with the alert overlay, broken/violated links are visible.

**Depends on:** 03 (netlink neighbor/route telemetry), 05 (netring flows), 07
(`external` alerts). **Effort:** M. Mostly frontend + a small sensor addition.

Pattern reference (verified): `view/topology/mod.rs::update_from_devices(&HashMap<DeviceId,
DeviceState>)` builds `Node`s (hardcoded `Protocol::Sysinfo`), then
`generate_edges()` synthesizes edges; force-directed layout in `layout.rs`,
canvas render in `graph.rs`.

---

## 1. Real adjacency from netlink (replace the guess)

The netlink sensor already publishes neighbor state
(`zensight/netlink/<host>/neighbor/<ip>/state`) and route/event telemetry (Plan
03 §3.2/§4). Add a compact, purpose-built adjacency channel so the frontend
doesn't have to reassemble it from scattered metrics:

- **Sensor side (small add to Plan 03):** publish an `AdjacencySnapshot` to
  `zensight/netlink/<host>/@/topology` periodically:
  ```rust
  struct AdjacencySnapshot {
      host: String,
      // neighbor entries: (ip, mac, iface, reachable)
      neighbors: Vec<NeighborLink>,
      // l3: default gw + notable routes (next-hop ip)
      gateways: Vec<String>,
      ts: i64,
  }
  ```
  (Bridge FDB / VLAN can extend `NeighborLink` later.)
- **Frontend:** subscribe (covered by `zensight/**`); decode in `subscription.rs`
  (`@/topology` arm) → `Message::AdjacencyReceived(AdjacencySnapshot)`; store in a
  new `TopologyState.adjacency: HashMap<host, AdjacencySnapshot>`.

## 2. Edge construction

Rewrite `generate_edges()` to prefer measured data, falling back to the current
heuristic when absent:

1. **L2/L3 edges:** for each host's neighbors, if the neighbor IP/MAC matches
   another known host (cross-reference `correlations` — the existing
   `zensight/_meta/correlation/*` map ties IP↔host across sensors), draw a real
   edge host↔neighbor. Edge metadata: iface, reachable.
2. **Edge weight = bandwidth:** if netring flow telemetry exists between the two
   endpoints (`flow/<src>-<dst>/bytes` rate), set edge thickness from it (the
   layout already wants bandwidth-based thickness). Otherwise thin/neutral.
3. **Fallback:** keep the existing activity-based synthesis for hosts with no
   neighbor data, so the view degrades gracefully.

## 3. Nodes for all relevant protocols

`update_from_devices` should create nodes for `Sysinfo` **and** `Netlink` hosts
(merge by hostname — a host running both sensors is one node, enriched from both).
netring sensors attach flow data to existing nodes rather than creating their own.

## 4. Alert overlay (the payoff with Pillar B)

Read `alerts.external` (Plan 07) in the topology update:

- A host with a firing **expectation** alert (interface down, gateway FAILED,
  route withdrawn) → node tinted by severity; a tooltip lists the violations.
- An **expected-but-absent edge** (a `route`/`neighbor` expectation that's firing)
  → render a dashed/red "broken link" between the endpoints, so a missing path is
  visible on the graph, not just in the alert list.
- A netring **anomaly** alert with a source IP matching a node → a warning badge
  on that node (and optionally a transient edge to the scanned target).

This is the single most compelling combined feature: the graph shows what *should*
connect, what *does*, and what's *wrong*, in one picture.

## 5. Interaction
- Click a node → existing info panel, now with: interfaces summary (netlink),
  top flows (netring), firing expectations, "View Details" → specialized view
  (Plan 09).
- Click a broken edge → the expectation alert that explains it.
- Existing zoom/pan/search/pin behavior unchanged.

## 6. Tests
- `generate_edges` unit tests: given synthetic adjacency + flow maps, assert the
  expected edge set + weights; assert fallback when adjacency is empty.
- Alert overlay: a firing route-expectation produces a broken edge; a resolved one
  removes it.
- Node merge: a host with both sysinfo + netlink telemetry is one node.
- Simulator: topology renders with measured edges + a tinted node for a firing
  expectation.

## 7. Acceptance criteria
- With two hosts running netlink sensors on the same segment, the graph draws a
  real edge between them from neighbor tables (not the heuristic).
- Running netring on those hosts sizes the edge by actual bandwidth.
- Taking an interface down (or a `default_via` expectation firing) tints the node
  and/or draws a broken edge within ~1s; recovery clears it.
- Edge/overlay unit + simulator tests green; topology degrades gracefully when
  only sysinfo data is present (no regression).
