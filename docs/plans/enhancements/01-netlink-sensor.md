# Plan 01 (enh) — netlink sensor v2: many more metrics, alerts, dynamic config

**Crate:** `zensight-sensor-netlink`. **Depends on:** v1 sensor + `AlertReporter`
+ command channel (shipped). **Effort:** L (split into independently-shippable
sub-plans A–D).

Goal: go from "interface counters + socket aggregates + 4 expectations" to a
full host-network sensor — routes, neighbors, QoS, ethtool, WireGuard,
conntrack, per-socket health, diagnostics, push events — all
**runtime-configurable**, with detail served **on demand** (§D, principle P2).

`nlink` API used below is verified against the pinned checkout's `examples/`
(route/{routes,neighbors,addresses,stats}, route/tc/stats, genl/{wireguard,
ethtool_*}, netfilter/conntrack, events/monitor, diagnostics/health_check).
Re-confirm exact method names against the pinned version at implementation time.

---

## A. More metrics (telemetry + the on-demand query channel)

New `collector.rs` modules, each behind a `collect.*` toggle (§C). Metrics follow
the **USE** taxonomy (P3). Pure `map.rs` functions per family, unit-tested.

### A.1 Interface depth (USE: full saturation + errors)
v1 has bytes/packets/errors/dropped/up/carrier/mtu. Add, from `LinkStats64`
(already available on `Link::stats()`):
| metric | type | USE class |
|---|---|---|
| `iface/<n>/rx_fifo`, `tx_fifo` | Counter | saturation |
| `iface/<n>/rx_frame`, `rx_crc`, `rx_length`, `rx_missed` | Counter | errors |
| `iface/<n>/tx_carrier`, `collisions` | Counter | errors |
| `iface/<n>/multicast` | Counter | utilization |
Plus **ethtool** (genl `ethtool_*`): `iface/<n>/speed_mbps`, `duplex`,
`link_flaps`, and per-queue `iface/<n>/queue/<q>/rx_drops` / ring stats →
`iface/<n>/ethtool/<stat>` (the NIC-level drop data SNMP can't see).

### A.2 Routes (low-cardinality summary + detail-on-demand)
Stream a summary only (cardinality!):
| metric | type |
|---|---|
| `routes/ipv4_count`, `routes/ipv6_count` | Gauge |
| `routes/default_gw_present` (v4/v6) | Boolean |
| `routes/changes_total` (from events) | Counter |
Full route table → `@/query/routes` (§D). Source: `conn.get_routes()`
(verify name) / route dump.

### A.3 Neighbors / ARP-NDP (the L2 truth)
Summary streamed:
| metric | type |
|---|---|
| `neighbors/by_state/<reachable\|stale\|failed\|...>` | Gauge |
| `neighbors/gateway_reachable` | Boolean |
Per-neighbor `(ip, mac, iface, state)` → `@/query/neighbors` (§D) — this is the
data that drives topology edges (Plan 02 §5). Source: neighbor dump
(route/neighbors example).

### A.4 TC / QoS (USE saturation for shaped links)
Per qdisc/class (these are bounded per interface, so streamable):
| metric | type | USE |
|---|---|---|
| `tc/<iface>/<qdisc>/backlog_bytes`, `backlog_pkts` | Gauge | saturation |
| `tc/<iface>/<qdisc>/drops`, `overlimits`, `requeues` | Counter | saturation/errors |
| `tc/<iface>/<class>/rate_bps` | Gauge | utilization |
Source: TC qdisc/class stats (route/tc/stats example). Answers "where is traffic
being shaped/dropped" — invisible to SNMP/NetFlow.

### A.5 Socket health (aggregates streamed; per-socket on demand)
Extend v1's by-state counts with **health distributions** (RED for connections):
| metric | type |
|---|---|
| `sockets/tcp/retransmit_rate` (Δretrans/Δpackets, windowed) | Gauge |
| `sockets/tcp/rtt_p50_us`, `rtt_p95_us` | Gauge |
| `sockets/tcp/listen_backlog_max` (max recv-Q on listeners) | Gauge |
| `sockets/tcp/by_uid/<uid>` (count, optional) | Gauge |
Full socket list (local/remote/state/rtt/cwnd/retrans/uid) → `@/query/sockets`
(§D, filterable by state/port). Source: sockdiag with `with_tcp_info().with_mem_info()`.

### A.6 WireGuard / tunnels
Per configured/discovered peer (bounded): `wireguard/<peer>/last_handshake_age_s`
(Gauge), `rx_bytes`/`tx_bytes` (Counter), `endpoint` (Text label), `up` (Boolean
= handshake fresh). Source: genl WireGuard dump (genl/wireguard example). XFRM/
IPsec SA health as a later add.

### A.7 Conntrack (NAT/flow table health)
`conntrack/entries` (Gauge), `conntrack/by_proto/<tcp\|udp\|...>` (Gauge),
`conntrack/max` (Gauge), `conntrack/utilization` (Gauge = entries/max),
`conntrack/{new,destroy}_total` (Counter, from conntrack events). Source:
netfilter conntrack (netfilter/conntrack example). `utilization` near 1.0 is a
classic outage cause.

### A.8 Diagnostics (nlink's built-in scorer)
`diagnostics/bottleneck_score` (Gauge 0..1), `diagnostics/issues_<severity>`
(Gauge counts), worst-bottleneck `location`/`recommendation` as labels. Source:
`Diagnostics::new(conn).scan()` + `find_bottleneck()` (health_check example).

### A.9 Push events (the SNMP-beating differentiator)
Already partially in v1's event forwarder. Expand: publish link/addr/route/
neighbor add/del/change as `*/event` Text telemetry **and** feed the sentinel for
instant re-eval (§B). Source: `conn.subscribe(&[RtnetlinkGroup::…]) + conn.events()`.

---

## B. More alerts (new expectation families + USE/diagnostics-derived)

All via the sentinel (`SentinelHandle`/`Evaluator`), unit-tested pure checks, and
integrated with `AlertReporter` (firing/resolved/debounce). New families:

| family | expectation | example alert |
|---|---|---|
| `route` | default route present/via `<gw>`; prefix `<cidr>` present | "default route withdrawn / gw changed" |
| `neighbor` | gateway/peer `<ip>` REACHABLE | "gateway 10.0.0.1 FAILED (ARP)" |
| `wireguard` | peer `<name>` handshake < `<t>`; tx/rx advancing | "wg peer gw2 no handshake 5m" |
| `diagnostics` | bottleneck score < `<x>`; issues ≤ `<severity>` | "bottleneck score 0.82 on eth0: <recommendation>" |
| `interface-health` | rx/tx error|drop rate < `<r>`; no carrier flaps; MTU == `<m>` | "eth0 drop rate 3%/s; carrier flapped 5× in 1m" |
| `conntrack` | utilization < `<x>` | "conntrack table 94% full" |
| `metric-threshold` | **generic**: any netlink metric `<op> <value>` | enables GUI rule-promotion (P4) |

`metric-threshold` is the keystone: it lets a GUI threshold rule (e.g.
`sockets/tcp/retransmit_rate > 0.05`) be **promoted** to a headless expectation.
Reuse `ComparisonOp` (move to `zensight-common`). Event-driven families (route/
neighbor) re-evaluate on the push event (§A.9) for ~1s latency, not the poll
interval.

---

## C. Dynamic configuration (runtime, no restart)

Generalize Plan 08's command channel. New topics on
`zensight/netlink/@/commands/<topic>`:

- **`collection`** — toggle any collector at runtime:
  `{ "type": "set", "collect": { "routes": true, "tc": false, ... } }`.
  Status queryable `@/status/collection` returns the live toggles.
- **`expectations`** — already shipped; extend with the §B families.
- **Per-host targeting (fleet):** the command key is shared by all netlink
  sensors. Add an optional `host` field; a sensor applies a command only if it
  matches its hostname (or `host` omitted = all). Lets the GUI target one host or
  the fleet.

`collector.rs` reads its toggles from an `Arc<RwLock<CollectConfig>>` (same
hot-swap pattern as the sentinel), so `set` takes effect on the next poll tick.

---

## D. On-demand detail — the query channel (principle P2)

A new `query.rs` module declares Zenoh **queryables** the GUI calls when a user
drills in. Replies are JSON; nothing is streamed.

| queryable key | reply |
|---|---|
| `zensight/netlink/@/query/sockets?state=&port=` | full TCP socket list (local, remote, state, rtt, cwnd, retrans, uid) |
| `zensight/netlink/@/query/routes` | route table (dst, gw, dev, proto, metric) |
| `zensight/netlink/@/query/neighbors` | neighbor table (ip, mac, dev, state) |
| `zensight/netlink/@/query/wireguard` | per-peer detail |

This is how the GUI (Plan 02) shows a host's full socket/route/neighbor tables
without those ever hitting the telemetry bus. Selector parameters filter
server-side. (Reuse the queryable pattern from the sentinel status channel.)

---

## E. Config schema (additions)

```json5
netlink: {
  collect: {
    interfaces: true, sockets: true,          // v1
    ethtool: true, routes: true, neighbors: true,
    tc: false, wireguard: false, conntrack: false,
    diagnostics: true, events: true,
  },
  socket_detail: { rtt_percentiles: true, by_uid: false },
  expectations: { /* v1 + new families: routes[], neighbors[], wireguard[],
                     diagnostics{}, interface_health[], conntrack{}, metrics[] */ },
}
```

## F. Testing
- Pure `map.rs` per family on synthetic nlink structs (no kernel): route summary,
  neighbor state counts, TC stats, socket health percentiles, wireguard age,
  conntrack utilization, diagnostics mapping.
- Pure checks per new expectation family (route/neighbor/wireguard/diagnostics/
  interface-health/conntrack/metric-threshold).
- Live (unprivileged): `get_routes`/`get_neighbours`/sockdiag/diagnostics against
  the real kernel behind a `live`-gated test; verify the query channel replies.
- Command channel: `collection set` toggles a collector live (integration test
  over an in-proc scouting-disabled peer, like the v1 AlertReporter test).

## G. Acceptance criteria
- All §A families publishable + each toggleable at runtime via §C with no restart.
- Drilling a host in the GUI fetches its full socket/route/neighbor tables via §D
  (verified: a `session.get(@/query/sockets)` returns the live table).
- Each §B expectation fires + auto-resolves; route/neighbor families fire within
  ~1s of the kernel event (not the poll interval).
- `metric-threshold` expectation makes a GUI-authored threshold fire headlessly.
- Pure-function unit tests for every family; example config + README updated.

## H. Sequencing
A.1–A.3 + A.5 + D first (highest value: richer interfaces/sockets + drill-down),
then A.4/A.6/A.7/A.8 (QoS/wg/conntrack/diag), then A.9 event expansion, then B
(alerts), then C (dynamic config). Ship each family independently.

## I. Risks
- **Cardinality** (P2): never stream per-socket/route/neighbor; the query channel
  is mandatory, not optional. Lint for it in review.
- Some collectors (conntrack, full sockdiag) are heavier — keep them **off by
  default** and document poll-interval cost.
- nlink genl families (wireguard/ethtool) and conntrack may need feature flags on
  the `nlink` dep — confirm and gate.
