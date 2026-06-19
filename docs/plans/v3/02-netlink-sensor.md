# Plan v3‑02 — netlink sensor

Today: interfaces (LinkStats), TCP sockets (sockdiag aggregates + RTT
percentiles), neighbors, routes, nlink diagnostics, conntrack, WireGuard, a
sentinel (expectations → alerts, incl. the metric‑threshold keystone), and an
on‑demand `@/query/{sockets,routes,neighbors}` channel. All **polled**.

> APIs verified against the pinned nlink checkout (`netlink/connection.rs`,
> `genl/ethtool/`, `messages/tc.rs`, `events.rs`, `sockdiag/`) + examples.
> Research: event‑driven RTNETLINK, ethtool/conntrack drop counters, USE.

---

## A. Real‑time push EVENTS (replace poll latency) **[Wave 1, high‑leverage]**

**Verified API:** `conn.subscribe(&[RtnetlinkGroup::Link, Ipv4Addr, Ipv6Addr,
Ipv4Route, Ipv6Route, Neigh])` then `let mut events = conn.events().await;` →
`while let Some(Ok(ev)) = events.next().await { … }`. `events()` returns a
`Stream`, so the idiomatic integration is `tokio::select!` between the poll
`tick` and `events.next()` in the collector loop (or a dedicated stream task).
`NetworkEvent` has typed accessors (`is_new()`/`is_del()`/`as_link()`/`as_route()`/
`as_neighbor()`). **Integrate two ways:**
1. Publish `events/{link,addr,route,neighbor}/{added,removed,changed}_total`
   (Counter) + a recent‑events ring → `@/query/events`.
2. Feed the **sentinel**: a `DelLink(eth0)` / default‑route withdrawal /
   gateway‑neighbor‑FAILED event re‑evaluates the matching expectation
   *instantly* (~0s) instead of at the next poll tick (the current 2s+ latency).

**Why:** research is unanimous — bind RTNETLINK multicast for state transitions,
poll only cumulative counters. Eliminates flap‑miss + cuts latency. Unprivileged.
*Live‑verifiable* (toggle an interface / add an IP in the sandbox).

## B. ethtool: link speed/duplex + ring drops **[Wave 1]**

`Connection::<Ethtool>::new_async()` → `get_link_state()`/`get_rings()`/
`get_channels()`/`get_features()`/`get_pause()`.

| metric | type |
|---|---|
| `ethtool/<iface>/{speed_mbps,duplex,autoneg,carrier}` | Gauge/Bool |
| `ethtool/<iface>/rings/{rx,tx}` + per‑queue drops where exposed | Gauge/Counter |
| `ethtool/<iface>/features/{tso,gro,...}` | Bool |

**Why:** `-S` drop counters are NIC/driver ground truth (ring overflow = CPU
saturation / undersized rings); speed/duplex mismatch detection. Read is
unprivileged. Sentinel: `ethtool:speed` (negotiated below expected), `:duplex`
(half‑duplex). *Live‑verifiable (read‑only).*

## C. Address inventory **[Wave 1]**

`conn.get_addresses()` → per‑interface IPs. Stream low‑card summary
(`addresses/{ipv4_count,ipv6_count,global_count}`); serve detail via
`@/query/addresses` (ip/prefix/scope/label per iface). Sentinel: `addr:present`
(an expected IP/prefix is configured), `addr:count_drop` (DHCP failure / hijack).
**Why:** closes the gap between "link up" and "actually reachable". Unprivileged,
*live‑verifiable.*

## D. Richer socket data + on‑demand by‑uid/cong **[Wave 2]**

Extend the sockdiag filter with `.with_mem_info().with_congestion()`:
stream `sockets/tcp/by_cong/{bbr,cubic,reno,...}` (Gauge) +
`sockets/tcp/mem/{snd,rcv}_buf_total`; add uid/cwnd/congestion/mem to the existing
`@/query/sockets` reply. Sentinel: `socket:bufferbloat`
(`mem/rcv_buf_total > X`). **Why:** per‑app/per‑algorithm visibility; bufferbloat
detection. Unprivileged, *live‑verifiable.*

## E. TC / QoS qdisc & class stats **[Wave 2]**

`get_qdiscs()`/`get_classes()` → `TcMessage::{bytes,packets,drops,overlimits,
requeues,qlen,backlog,bps,pps}`. Stream per‑(ifindex,handle) aggregates (bounded
by the TC hierarchy): `tc/<iface>/<qdisc>/{drops,overlimits,backlog_bytes,
backlog_pkts}`; full tree → `@/query/tc`. Sentinel: `tc:congestion`
(`drops`/`backlog` over threshold). **Why:** rising drops + growing backlog =
egress congestion *before* users notice; invisible to SNMP/NetFlow. Read is
unprivileged. *Live‑verifiable where qdiscs exist.*

## F. XFRM / IPsec SA health **[Wave 3]**

`Connection::<Xfrm>` → SA/policy dump. Stream `xfrm/sa/{total,by_state/*,
by_mode/*}`, `xfrm/policy/total`; detail → `@/query/xfrm`. Sentinel:
`xfrm:tunnel_up` (expected SA present + mature), `:lifetime_expiring`. **Why:**
broken VPN/IPsec tunnel detection (companion to the existing WireGuard feature).
Read unprivileged; root‑only host. *Verify by API + unit test.*

## G. nftables rule counters **[Wave 3, root]**

`Connection::<Nftables>::list_rules()` → per‑rule `{packets,bytes}`. Stream
table/chain rule counts; detail → `@/query/nft`. Sentinel: `nft:drop_rising`
(a drop rule's counter rate spikes — silent‑drop / DoS signal). **Why:** firewall
effectiveness + policy‑drift visibility. *API‑verified; live needs the host's nft
ruleset.*

---

## Lower‑priority / specialized (catalogued, not scheduled)

bridge/VLAN FDB + bond slave health (redundancy validation), devlink health
reporters (early HW‑error warning), nl80211 (Wi‑Fi RSSI/roaming), MACsec/MPTCP,
namespace‑scoped polling (per‑pod k8s). All have clean nlink `Connection<T>` APIs;
pull into a wave when a target environment needs them.

---

## Testing & sequencing

> Follow [Plan 05](05-architecture-and-conventions.md): consume `events()` as a
> `Stream` via `tokio::select!`; decode nlink's typed messages → pure `map.rs` →
> points; `Result`‑returning poll steps that record failures + degrade gracefully
> when a genl family / privilege is absent.

- Pure aggregate/`map.rs` functions per family on synthetic nlink structs
  (the established pattern) — unit‑tested.
- Live (unprivileged in sandbox): A (events), B (ethtool read), C (addresses),
  D (socket ext), E (TC where present) — verify with a throwaway subscriber.
- Wave 1 = A+B+C (events is the highest‑leverage: kills poll latency + flap‑miss).
  Wave 2 = D+E. Wave 3 = F+G + specialized as needed.
- Every config‑gated; graceful when a genl family / privilege is absent.
- Event‑driven sentinel re‑eval (A) is the structural win — wire it before adding
  more polled families.
