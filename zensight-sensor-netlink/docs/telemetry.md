# netlink telemetry

All streamed telemetry is published under
`zensight/v1/<origin>/telemetry/netlink/<metric>` as a serialized
`TelemetryPoint` (JSON or CBOR per `serialization`), where `<origin>` is the
host's `h-<12hex>` id — the key carries no source chunk; the payload
`TelemetryPoint` still carries `source`. High-cardinality detail is **never
streamed** — it is served on request from the `@rpc/netlink/<topic>` procedures
(see below). Metric families are gated by the `collect.*` config toggles (see
[configuration.md](configuration.md)).

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative
key-expression contract and
[`zensight-keyspace/registry/netlink.toml`](../../zensight-keyspace/registry/netlink.toml)
for the machine-readable subject/procedure registry this page summarizes.

## Streamed telemetry

### Interfaces & counters (`collect.interfaces`)

Per-interface counters and state under `iface/<name>/...`:

| Metric | Type | Notes |
|---|---|---|
| `iface/<name>/rx_bytes`, `tx_bytes`, `rx_packets`, `tx_packets`, `rx_errors`, `tx_errors`, `rx_dropped`, `tx_dropped` | Counter | label `ifindex` |
| `iface/<name>/up` | Boolean | admin/oper up |
| `iface/<name>/carrier` | Boolean | physical carrier |
| `iface/<name>/oper_state` | Text | `up`/`down`/`lowerlayerdown`/… |
| `iface/<name>/mtu` | Gauge | |
| `iface/<name>/info` | Text | MAC address (label `mac`) |

Interface selection is governed by the `interfaces` include/exclude filter.

### TCP socket aggregates (`collect.sockets`)

Fleet-wide TCP state counts and enriched `tcp_info` roll-ups under `sockets/tcp/...`:

- state counts: `established`, `listen`, `time_wait`, `syn_sent`, `close_wait`, …
- `retransmits_total` (summed), `max_rtt_us` (worst RTT observed)
- delivery/pacing/retransmit/reorder percentiles from the enriched `tcp_info`
  (#108) — e.g. `delivery_rate_p50` (watched by the sentinel delivery floor)
- per-congestion-control fleet counts: `sockets/tcp/by_cong/<algo>`

Full per-socket rows (with congestion-control detail — BBR bottleneck bandwidth
`bbr_bw_bps` + min-RTT `cc_min_rtt_us`) come from `@rpc/netlink/sockets`, not
the bus.

### ethtool link health (`collect.ethtool`, nlink 0.23)

Per-interface link health under `ethtool/<iface>/...`: speed/duplex/autoneg, ring
sizes, pause, plus **FEC** (`fec/{modes,auto}` — silent corruption on marginal
optics) and **EEE** (`eee/{enabled,active}` — power-save that can add latency).
Best-effort per family; drivers lacking one still yield the rest.

### TC / QoS / bufferbloat (`collect.tc`)

Per-qdisc drops/overlimits/backlog plus a bufferbloat/AQM health score. Read is
unprivileged; absent where no qdiscs are configured. Per-qdisc rows on
`@rpc/netlink/tc`.

### Routes, neighbors, addresses, diagnostics

- `collect.routes` — routing-table summary + `routes/default_v4_flaps_total`
  (default-route flap counter; per-transition history on
  `@rpc/netlink/route_changes`).
- `collect.neighbors` — ARP/NDP neighbor-state summary.
- `collect.addresses` — per-family + global IP address counts (#10).
- `collect.diagnostics` — nlink's bottleneck score + issue counts.

### Control-plane change timeline (`collect.events`)

Real-time RTNETLINK changes fold into counters and a recent-events ring
(`@rpc/netlink/events`). Counter families `events/<family>/{added,removed,changed}_total`:

| Family | Source |
|---|---|
| `link`, `addr`, `route`, `neighbor` | core RTNETLINK add/del/change |
| `ipsec` | XFRM **monitor** stream — SA/policy lifecycle the periodic snapshot misses (gated on `collect.events && collect.xfrm`, nlink 0.23) |
| `rule`, `nexthop`, `mdb`, `nsid` | policy-routing rules, nexthop objects, bridge multicast-DB, netns ids (#323, nlink 0.24) |

`NewRule`/`DelRule` re-evaluates the sentinel instantly.

### nftables firewall hit-rate (`collect.nftables`, needs `CAP_NET_ADMIN`)

Ruleset shape `nft/{tables,chains,rules}_total`, monotonic `nft/{packets,bytes}_total`,
and per-table `nft/<family>/<table>/{packets,bytes}`. Per-rule counters decoded from
nlink's native `RuleInfo::counter()` (#322). Per-rule detail on `@rpc/netlink/nft`.

### conntrack (`collect.conntrack`, needs `CAP_NET_ADMIN`)

Netfilter conntrack table summary (entries/proto/utilization).

### Per-process TCP bandwidth (`collect.bandwidth`, default on)

The **unprivileged, TCP-only** bandwidth tier (#317, epic #320). A background
sampler diffs each socket's `tcp_info` goodput byte counters (`bytes_acked` = TX,
`bytes_received` = RX) **per cookie** (never the reusable inode). Served
query-only on `@rpc/netlink/bandwidth`. Limits are labelled, not hidden: **TCP only**
(`udp_diag` exposes no per-socket byte counters), **app-goodput** (below wire — no
headers/retransmits), and short flows opened+closed between samples are missed.
Records tagged `bw.source=sock_diag` / `bw.semantics=app-goodput` / `bw.proto=tcp`.

### WireGuard (`wireguard.interfaces`)

Per-peer handshake age, rx/tx, up/down. Needs the `wireguard` kernel module; full
peer data needs `CAP_NET_ADMIN`. `wg_quick_configs` enrich peer labels with
AllowedIPs/endpoint (#268).

### eBPF tier (`collect.ebpf`, off by default, `--features ebpf` build)

What `sock_diag` snapshots cannot see — connection *lifecycle* and *attribution*.
Streams connect-latency gauges `sockets/tcp/connlat_us_{p50,p95}` (through the
normal publish path, so sentinel `metric-threshold` expectations can watch them).
Also serves `@rpc/netlink/retransmits` and `@rpc/netlink/connections` (see below).
No-op unless built with `--features ebpf` **and** holding `CAP_BPF` + `CAP_NET_ADMIN`.

## On-demand detail — `@rpc/netlink/<topic>`

Served on request, never streamed — read procedures (GETs) on the `@rpc` plane:
`zensight/v1/<origin>/@rpc/netlink/<topic>`. Parameters are Zenoh selector
params (`?state=&port=`, `?top=N`). A fleet-wide caller selects
`zensight/v1/*/@rpc/netlink/<topic>` with query target `All` to fan out to
every netlink sensor on the bus. Every sensor also serves
`@rpc/netlink/introspect`, returning the registry slice this build serves.

| Topic | Reply | Notes |
|---|---|---|
| `routes` | `Vec<RouteRecord>` | |
| `neighbors` | `Vec<NeighborRecord>` | ARP/NDP entries |
| `sockets?state=&port=&ip=` | `Vec<SocketRecord>` | with process attribution + congestion-control detail; `port` compiles to kernel-side INET_DIAG bytecode (#322); `ip=` narrows to sockets whose local **or** remote IP matches (#309) |
| `addresses` | `Vec<AddressRecord>` | |
| `events` | `Vec<EventRecord>` | recent control-plane timeline |
| `route_changes` | `Vec<RouteChangeRecord>` | per-transition default-route flap history (gateway change / withdrawal / re-appearance) |
| `tc` | `Vec<TcRecord>` | per-qdisc |
| `xfrm` | `Vec<XfrmRecord>` | IPsec SA/policy |
| `nft` | `Vec<NftRecord>` | per-rule `packets`/`bytes` |
| `bandwidth?top=N` | `Vec<BandwidthRecord>` | per-process TCP goodput, ranked, top-N (default 50) |
| `retransmits` | top-K per-peer retransmit counts | eBPF only (#114) |
| `connections` | recent tcplife records (pid/comm/peer/duration) | eBPF only (#114) |

### Socket → process attribution (#304)

With `collect.socket_processes` (default on), each `SocketRecord` is annotated —
unprivileged — with `cookie` (stable socket id; prefer over the reusable inode),
`cgroup_id`/`cgroup` (v2 path), and the owning `pid`/`process`/`proc_start_time`
(the `(pid, start_time)` identity pair). Attribution runs a `/proc` fd-scan **per
query request** (in a blocking task, duration logged), skipped above
`socket_process_max_procs` (default 4096). Sockets whose owner has exited stay
unattributed (`—`). This `(pid, start_time)` pair joins to a sysinfo process, a
systemd unit's `main_pid`, and netring flow ownership.

## Alerts & identity evidence

- **Alerts** — `zensight/v1/<origin>/state/netlink/alert/<alert_key>` from
  sentinel expectation violations (`<alert_key>` = 16-hex FNV-1a of
  `rule + labels`; firing = Put, resolved = Put(Resolved) then a Delete
  tombstone). See [sentinel.md](sentinel.md).
- **Identity evidence (#307)** — with `evidence` (default on), the neighbor poll
  publishes observed-neighbor `HostEvidence` (ARP/ND table → MAC↔IP,
  `observer=netlink`) on
  `zensight/v1/<origin>/state/netlink/evidence/device/<device>` (plus the
  self-claim on `.../evidence/self`) for the correlator, with the configured
  rate-limiting and TTL aging.
