# Plan 02 (enh) — netlink GUI v2: tabbed host view, charts, drill-down, topology

**Crate:** `zensight` (frontend). **Depends on:** enh-01 (telemetry + query +
command channels). **Effort:** M–L. Pure frontend + the query-channel client.

Goal: turn the flat `specialized/netlink.rs` (interfaces + sockets list) into a
rich host-network cockpit that surfaces everything enh-01 produces, fetches
detail **on demand**, shows live expectation state, and lets the user toggle
collection — all driven by the aggregate-stream + query-detail model (P2).

---

## 1. Tabbed host view
Replace the single scroll with tabs in `specialized/netlink.rs` (a small local
`NetlinkTab` enum + `NetlinkViewState` for selected tab and fetched detail):

| Tab | Source | Notes |
|---|---|---|
| **Overview** | streamed aggregates | health summary: up ifaces, socket states, gateway reachable, bottleneck score, firing expectations |
| **Interfaces** | `iface/*` | table + per-iface expand → charts (§2); USE columns (util/sat/errors) |
| **Sockets** | aggregates + `@/query/sockets` (§3) | counts streamed; full table fetched on demand, filter by state/port |
| **Routes** | `@/query/routes` | fetched on tab open |
| **Neighbors** | `@/query/neighbors` | ip/mac/dev/state; feeds topology (§5) |
| **QoS / TC** | `tc/*` | backlog/drops/overlimits per qdisc |
| **WireGuard** | `wireguard/*` + `@/query/wireguard` | per-peer handshake age, throughput |
| **Diagnostics** | `diagnostics/*` | bottleneck score gauge + issues list + recommendation |
| **Expectations** | status queryable | inline satisfied/firing per rule, link to authoring view |

Tabs that need detail fetch lazily on selection (don't fetch until opened).

## 2. Charts & sparklines
Use the existing `view/chart.rs` + the device history `VecDeque`:
- Per-interface **rx/tx rate** (derive bps from byte counters; ×8 for bits, per
  the node_exporter convention), **error/drop rate** sparklines.
- Socket-state **stacked area** over time (established/listen/time_wait).
- `retransmit_rate` and `rtt_p95_us` trend lines (connection health).
Rates are derived client-side from the streamed counters + timestamps.

## 3. On-demand detail client (the query channel)
A reusable app helper `query_json(key, selector) -> Task<Message>` (generalizes
the Plan 08 `query_expectations`): `session.get(key + selector).await` → decode →
`Message::NetlinkDetailReceived { topic, json }`. Used for sockets/routes/
neighbors/wireguard tabs. Detail is cached in `NetlinkViewState` with a manual
**Refresh** + an auto-refresh on tab open. **Nothing is streamed** — this is the
P2 payoff in the UI.

## 4. Inline expectation state
On the Interfaces/Sockets tabs, annotate each row with its expectation verdict
(✓ satisfied / ⚠ firing / – none) by cross-referencing `alerts.external` (firing
expectation alerts, matched by labels: iface, port) — so the user sees, on the
host view itself, which declared expectations are currently violated, with a
jump to the Expectations authoring view (Plan 08).

## 5. Topology enrichment — measured edges (the headline)
Finish what v1's overlay started (v1 has nodes + alert tint; edges are still the
activity heuristic):
- **Real L2/L3 edges from neighbors:** consume `@/query/neighbors` per netlink
  host; cross-reference the existing `_meta/correlation/*` map (IP↔host) to draw
  an edge host↔neighbor when the neighbor IP/MAC matches another known host.
  Edge metadata: iface, reachable state.
- **Broken-edge overlay:** a firing `route`/`neighbor` expectation (enh-01 §B)
  renders the affected edge dashed/red — "expected-but-absent path" is visible on
  the graph, not just in the alert list.
- **Edge weight = bandwidth:** size edges by netring flow rate between endpoints
  (Plan 04) when available; fall back to the v1 heuristic otherwise.
- Add a `TopologyState.adjacency` map populated from a periodic neighbor fetch (or
  a compact `@/topology` snapshot if enh-01 adds one); rewrite `generate_edges()`
  to prefer measured adjacency.

This is the "inferred → measured" upgrade the original report pitched.

## 6. Dynamic-config UI
A small **Collection** panel (on Overview or a settings sub-view): toggles for
each collector (routes/neighbors/tc/wireguard/conntrack/diagnostics/events) that
send `collection set` commands (enh-01 §C) via the existing `send_command`
helper, with a host-target dropdown (fleet vs one host). Status reflected from
`@/status/collection`. So an operator turns on conntrack/QoS collection from the
GUI without touching configs.

## 7. New protocols / messages / state
- `message.rs`: `Message::{SelectNetlinkTab, NetlinkDetailReceived{topic,json},
  RefreshNetlinkDetail, SetNetlinkCollection{...}, NetlinkCollectionStatus(json)}`.
- `NetlinkViewState` (per selected device): tab, cached detail per topic, last
  fetch time.
- Route the netlink specialized view through a `device_view_with_netlink_state`
  dispatcher (like syslog's filter-state path), since it now needs side state.

## 8. Testing (iced `Simulator`)
- Each tab renders from synthetic aggregates / fetched-detail JSON without panic.
- `query_json` builds the right key+selector (fake session capturing gets).
- Detail decode: `@/query/sockets` reply JSON → table rows.
- Topology: synthetic neighbor adjacency → expected measured edges; firing
  route-expectation → broken edge.
- Collection toggle emits the right command `Message`.

## 9. Acceptance criteria
- Opening a netlink host shows tabs; Sockets/Routes/Neighbors fetch full tables on
  demand (verified: a `@/query/*` round-trip populates the table).
- Interface rate + socket-state charts render from streamed counters.
- A firing route/neighbor expectation draws a broken topology edge between the
  real hosts; resolving clears it.
- Two hosts on a segment draw a **measured** neighbor edge (not the heuristic).
- Toggling a collector from the GUI changes what the sensor publishes (no restart).
- Simulator tests green; new protocols/messages exhaustively handled.
