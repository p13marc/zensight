# Plan 03 — `zensight-sensor-netlink` (built on `nlink`)

**Goal:** a new sensor that streams **kernel ground truth** — interface counters
& state, link/addr/route/neighbor events, socket diagnostics, TC and WireGuard —
as `TelemetryPoint`s under `zensight/netlink/<host>/...`. This is the flagship
telemetry source *and* the data substrate the sentinel (Plan 04) asserts against.

**Depends on:** 01 (sensor-core), 02 (for `AlertReporter`, used by Plan 04).
**Effort:** M–L. **Platform:** Linux-only.

---

## 1. Crate scaffolding

`zensight-sensor-netlink/` mirrors `zensight-sensor-sysinfo` (the closest template
— single-host, poll + event loop):

```
zensight-sensor-netlink/
├── Cargo.toml
├── src/
│   ├── main.rs        # SensorArgs → config → SensorRunner → spawn collectors → run
│   ├── lib.rs         # module exports + key-expr docs
│   ├── config.rs      # NetlinkSensorConfig (impl SensorConfig)
│   ├── collector.rs   # poll loop: interface stats, sockdiag aggregates, TC, wireguard
│   ├── events.rs      # nlink event stream → TelemetryPoint (push)
│   └── map.rs         # nlink types → TelemetryPoint helpers
```

`Cargo.toml` deps: `zensight-common`, `zensight-sensor-core`, `nlink`
(pin exact version, features for `sockdiag`, `genl`/wireguard, `diagnostics`),
`tokio`, `tokio-stream`, `serde`/`serde_json`/`json5`, `clap`, `tracing`,
`hostname`. `[[bin]] name = "zensight-sensor-netlink"`.

Workspace member added under a Linux cfg gate (see §7).

## 2. Config (`config.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetlinkSensorConfig {
    pub zenoh: ZenohConfig,
    #[serde(default)] pub logging: LoggingConfig,
    pub netlink: NetlinkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetlinkConfig {
    #[serde(default = "default_key_prefix")] pub key_prefix: String, // "zensight/netlink"
    #[serde(default = "default_hostname")]   pub hostname: String,   // "auto"
    #[serde(default = "default_poll")]       pub poll_interval_secs: u64, // 5
    #[serde(default)] pub collect: CollectConfig,
    #[serde(default)] pub interfaces: IfaceFilter,   // include/exclude/exclude_virtual
    #[serde(default)] pub events: bool,              // subscribe to push events (default true)
    // Pillar B expectations live here too (Plan 04 adds `expectations`).
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    #[serde(default = "t")] pub interfaces: bool,   // counters + state + ethtool
    #[serde(default = "t")] pub sockets: bool,      // sockdiag aggregates
    #[serde(default)]       pub tc: bool,           // qdisc/class stats
    #[serde(default)]       pub wireguard: bool,    // per-peer
    #[serde(default)]       pub diagnostics: bool,  // Diagnostics::scan summary
}
```

`impl SensorConfig` (`zenoh`/`logging`/`key_prefix`/`validate`). Example in
`configs/netlink.json5`.

## 3. Telemetry mapping (`map.rs`, `collector.rs`)

`source = hostname`, `protocol = Protocol::Netlink`. Keys are
`zensight/netlink/<host>/<metric>`.

### 3.1 Interfaces (poll, `conn.get_links()` + ethtool)
For each non-filtered link:
| metric | value | labels |
|---|---|---|
| `iface/<name>/rx_bytes` / `tx_bytes` | Counter | `ifindex` |
| `iface/<name>/rx_packets` / `tx_packets` / `rx_errors` / `tx_errors` / `rx_dropped` / `tx_dropped` | Counter | |
| `iface/<name>/oper_state` | Text (`up`/`down`) | |
| `iface/<name>/carrier` | Boolean | |
| `iface/<name>/mtu` | Gauge | |
| `iface/<name>/speed_mbps` (ethtool) | Gauge | |
| `iface/<name>/ethtool/<stat>` (per-queue drops etc.) | Counter | `queue` |

Link stats come from the `RTM_GETLINK` stats (nlink `route/stats.rs`); ethtool
via nlink genl (`genl/ethtool_*`). MAC/oper from `Link::{address,is_up}`.

### 3.2 Socket aggregates (poll, `Connection::<SockDiag>`)
Count sockets by state; emit gauges:
| metric | value |
|---|---|
| `sockets/tcp/established` / `listen` / `time_wait` / `syn_sent` / `close_wait` | Gauge |
| `sockets/tcp/retransmits_total` (sum of `tcp_info.retransmits`) | Counter |
| `sockets/tcp/max_rtt_us` | Gauge |

Per-listener detail is consumed by the sentinel (Plan 04) but we publish the
*aggregates* here.

### 3.3 TC / QoS (poll, optional) → `tc/<iface>/<qdisc>/{drops,backlog,overlimits,requeues}` Counter/Gauge.
### 3.4 WireGuard (poll, optional) → `wireguard/<peer>/{last_handshake_age_s (Gauge), rx_bytes, tx_bytes (Counter)}`, label `endpoint`.
### 3.5 Diagnostics (poll, optional) → `diagnostics/bottleneck_score` Gauge, `diagnostics/issues` Gauge; emit the worst bottleneck's `location`/`recommendation` as labels.

`Publisher::publish` (or raw `session.put` like sysinfo) with `Format::Json`.

## 4. Event stream (`events.rs`, push)

```rust
let conn = Connection::<Route>::new()?;
conn.subscribe(&[RtnetlinkGroup::Link, Ipv4Addr, Ipv6Addr, Ipv4Route, Ipv6Route, Neigh])?;
let mut events = conn.events().await;
while let Some(ev) = events.next().await {
    match ev? {
        NetworkEvent::NewLink(l) | DelLink(l) => publish iface/<name>/event = Text("up"/"down"/"removed"),
        NetworkEvent::NewAddress(a) | DelAddress(a) => publish addr event,
        NetworkEvent::NewRoute(r) | DelRoute(r) => publish route/event,
        NetworkEvent::NewNeigh(n) | DelNeigh(n) => publish neighbor/<ip>/state = Text(REACHABLE/STALE/FAILED),
    }
}
```

Events are published as `TelemetryPoint`s (Text values + labels) so they flow
through the existing pipeline, **and** are forwarded to the sentinel's evaluator
so an expectation (e.g. "gateway REACHABLE") re-evaluates instantly on the event
rather than waiting for the next poll. Wire the event channel into Plan 04's
`Evaluator::on_event`.

This is the SNMP-beating differentiator: millisecond link/route/neighbor change
latency, no poll cycle.

## 5. main.rs flow (mirror sysinfo)

```rust
let args = SensorArgs::parse_with_default("netlink.json5");
let config = NetlinkSensorConfig::load(&args.config)?;
let hostname = config.resolved_hostname();
let runner = SensorRunner::new_with_args("netlink", config, Some(&args)).await?
    .with_status_publishing().with_format(Format::Json);
let session = runner.session().clone();
// spawn collector poll loop
runner.spawn(Collector::new(hostname, cfg, session.clone(), Format::Json).run());
// spawn event loop (if cfg.events)
runner.spawn(EventForwarder::new(hostname, session, ...).run());
// (Plan 04) spawn sentinel evaluator if cfg.expectations present
runner.run_with_metadata(Some(metadata)).await
```

## 6. Liveliness / health
- `SensorRunner` already publishes sensor liveliness (`@/alive`) and status.
- Treat each *interface* (or the host) as a "device" for `DeviceLiveness`?
  Recommend: the host is the device (`declare_device_alive(hostname)`); per-iface
  health is conveyed via telemetry + (optionally) expectation alerts, not the
  liveness channel. Keep it simple.
- Map nlink read errors → `SensorHealth.record_device_failure` and an
  `ErrorReport` to `@/errors`.

## 7. Linux-only gating

`nlink` is Linux-only. Options:
- Add `zensight-sensor-netlink` to workspace `members`, but in CI build it only
  on the Linux job. Cleanest: keep it a normal member and let it fail fast on
  non-Linux with a clear `compile_error!` in `main.rs` under
  `#[cfg(not(target_os = "linux"))]`.
- Document in README: "Linux only (netlink)."

## 8. Tests
- `map.rs`: pure functions `link_to_points(&Link) -> Vec<TelemetryPoint>`,
  `socket_counts(&[SocketInfo]) -> Vec<TelemetryPoint>` — unit-tested on
  hand-built nlink structs / fixtures (no root needed).
- Event mapping: `event_to_point(NetworkEvent) -> Option<TelemetryPoint>`.
- Gate any live-netlink test (`get_links` against the real kernel) behind a
  `#[ignore]` / `live` feature so CI without privileges still passes (reads are
  unprivileged but CI sandboxes vary).

## 9. Acceptance criteria
- `zensight-sensor-netlink --config configs/netlink.json5` publishes interface
  counters visible in the frontend dashboard as a `netlink` "device".
- Bringing an interface down (`ip link set X down` in a netns) produces an event
  telemetry point within ~1s (not a poll cycle).
- `Protocol::Netlink` added to `zensight_common` (+ `as_str`/`FromStr`/frontend
  `protocol_icon`), and the frontend renders the new protocol.
- Unit tests for `map.rs` pass; docs + example config added.
