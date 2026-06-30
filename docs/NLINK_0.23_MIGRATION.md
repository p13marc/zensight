# nlink 0.23 migration + capability adoption

**Date:** 2026-07-01
**Crate touched (backend):** `zensight-sensor-netlink`
**Crate touched (frontend):** `zensight` (`view/specialized/netlink.rs`)
**Status:** done — builds, `clippy -D warnings`, `fmt --check`, and tests green
(netlink 58 / frontend 276+69).

---

## 1. Why

`zensight-sensor-netlink` depended on `nlink` through a **git** dependency
(`github.com/p13marc/nlink`), which `Cargo.lock` pinned to commit `91f6d5a` =
version **0.21.0**. `nlink` is now published on **crates.io** (latest **0.23.0**,
2026-06-30). Moving off the git source:

- **Reproducible / offline / packaged builds** — no network git fetch, a recorded
  crates.io checksum, and `cargo deny`/license auditing works against a registry
  release instead of a moving branch.
- **Pinned, semver-meaningful upgrades** — `nlink = "0.23"` instead of "whatever
  `master` is today".
- **Unlocks 0.22/0.23 features** the old pin predated (XFRM monitor events,
  ethtool FEC/EEE, sockdiag bytecode filtering).

### The change

```toml
# zensight-sensor-netlink/Cargo.toml
-nlink = { git = "https://github.com/p13marc/nlink", features = ["sockdiag"] }
+nlink = { version = "0.23", features = ["sockdiag"] }
```

`cargo update -p nlink` then dropped the `git+` source for both `nlink` and
`nlink-macros` and recorded the registry checksum:

```
name = "nlink"
version = "0.23.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "5584327ae3df35d410c01a164670a81390511095bf88741a6f303b80082c896c"
```

No other workspace crate depends on `nlink`, so the blast radius is exactly this
one sensor (+ its frontend view).

---

## 2. The 0.21 → 0.23 API delta and the one breaking fix

The only compile break was a single, mechanical one:

- **`messages::LinkStats` fields were demoted to `pub(crate)` + public accessor
  methods.** The collector read `stats.rx_bytes` … `stats.collisions` directly.

```rust
// collector.rs — interface stats
-rx_bytes: stats.map(|s| s.rx_bytes).unwrap_or(0),
+rx_bytes: stats.map(|s| s.rx_bytes()).unwrap_or(0),
//   … same for tx_bytes, rx/tx_packets, rx/tx_errors, rx/tx_dropped,
//   multicast, collisions (10 fields)
```

`Chain::new → Result` and the new `#[non_exhaustive]` nl80211 structs from the
changelog do not touch any path this sensor uses — confirmed by a clean build.

---

## 3. New capabilities adopted

### 3.1 XFRM/IPsec **monitor events** → control-plane timeline (flagship)

**Before:** IPsec was *poll-only*. Each tick `poll_xfrm` dumped SAs
(`get_security_associations`) into the `xfrm/sa/*` summary. Anything that happened
*between* ticks — a rekey, a soft/hard SA expiry, an `ACQUIRE` (tunnel
negotiation) — was structurally invisible.

**Now:** nlink 0.23 adds an XFRM monitor stream
(`Connection::<Xfrm>::subscribe_all()` → `events()` yielding `XfrmEvent`). The
sensor opens a **dedicated** `Connection<Xfrm>` (the stream holds the request lock
for its lifetime, mirroring the existing RTNETLINK event task) and folds each
event onto the **same control-plane timeline** as a fifth event family, `ipsec`.

- `events.rs`: `EventFamily::Ipsec` added (counter matrix `[..;4] → [..;5]`); new
  generic, nlink-free `observe_ipsec[_at]` record path (counter + ring) — directly
  unit-tested without constructing kernel-only payloads.
- `collector.rs`: `run_xfrm_event_stream` + a **pure** `classify_xfrm_event` that
  maps each `XfrmEvent` onto the timeline's 3 actions and a human detail string:

  | XfrmEvent | action | detail example |
  |---|---|---|
  | `NewSa` / `NewPolicy` | `added` | `SA 10.0.0.1→10.0.0.2 spi=0x… esp tunnel` |
  | `DelSa` / `DelPolicy` / `FlushSa` / `FlushPolicy` | `removed` | `SA del →… spi=0x… esp` |
  | `ExpireSa{hard:true}` / `ExpirePolicy{hard:true}` | `removed` | `SA expire(hard) … spi=0x…` |
  | `ExpireSa{hard:false}` (rekey-soon) | `changed` | `SA expire(soft) … spi=0x…` |
  | `Acquire` / `Report` / `Other` | `changed` | `acquire <sel>` |

**Wire / metrics:**
- `events/ipsec/{added,changed,removed}_total` (streamed counters, automatic via
  the existing `counter_points`).
- Per-event rows in the recent-events ring served on `@/query/events`.

**Gating & degradation:** active only when `collect.events && collect.xfrm`. A host
with no IPsec, or one that gates the XFRM socket, just logs one warning and keeps
the unprivileged baseline.

**Frontend:**
- The control-plane timeline already renders `family/action/detail` generically, so
  `ipsec` rows appear with **no view change**.
- The **IPsec / xfrm** card now also shows the `events/ipsec/*` lifecycle counters
  (added / changed / removed), tying the live signal to the panel. The card's
  visibility gate was widened to `xfrm/` **or** `events/ipsec/` so it shows even if
  an SA churned before a summary snapshot existed.

### 3.2 ethtool **FEC / EEE** link health

nlink 0.23 exposes `get_fec` / `get_eee`. Both are best-effort additions to the
existing per-interface `ethtool_sample` (a driver lacking one still yields the
rest), surfaced as new metrics:

- `ethtool/<iface>/fec/modes` (Text, joined kernel mode names e.g. `RS,BASER`) and
  `ethtool/<iface>/fec/auto` (Boolean) — **FEC catches high-speed links silently
  corrupting under marginal optics before it shows up as drops.**
- `ethtool/<iface>/eee/enabled` and `…/eee/active` (Boolean) — **EEE is a common
  culprit for added latency / micro-stalls when negotiated unexpectedly.**

**Frontend:** ethtool had **no** view before this change. A new **ethtool (link /
FEC / EEE)** card (gated on `ethtool/`) renders a per-interface table —
link/speed/duplex/autoneg + FEC + EEE.

### 3.3 sockdiag **kernel-side state filtering**

The `@/query/sockets?state=&port=` detail query previously dumped **every** TCP
socket (`all_states()`) and filtered in user space. With nlink 0.23's sockdiag
filter, when the selector names a state we now push it into the kernel as a state
bitmask so the kernel returns only matching sockets:

```rust
let builder = SocketFilter::tcp().with_tcp_info().with_mem_info().with_congestion();
let builder = match sel.state.as_deref().and_then(tcp_state_from_label) {
    Some(st) => builder.states(&[st]),   // kernel-side
    None      => builder.all_states(),
};
```

**Correctness note:** the **port** match stays in user space on purpose. The
selector's semantics are "local **OR** remote port", but sockdiag's port bytecode
only models `sport AND dport`, so pushing it kernel-side would silently change the
result. State filtering is an unambiguous, lossless reduction; the existing
`SocketSelector::matches` still runs as the authoritative final filter (incl. port).

---

## 4. Files changed

| File | Change |
|---|---|
| `zensight-sensor-netlink/Cargo.toml` | git dep → `nlink = "0.23"` |
| `Cargo.lock` | `nlink`/`nlink-macros` → crates.io 0.23.0 |
| `…/src/collector.rs` | LinkStats accessors; XFRM event task + `classify_xfrm_event`; FEC/EEE sampling |
| `…/src/events.rs` | `EventFamily::Ipsec`; `observe_ipsec[_at]`; unit test |
| `…/src/map.rs` | `EthtoolSample` FEC/EEE fields + `ethtool_points` emission + test |
| `…/src/query.rs` | kernel-side socket state filter + `tcp_state_from_label` |
| `zensight/src/view/specialized/netlink.rs` | ethtool card; ipsec lifecycle counters in xfrm card; widened gate |
| `docs/SENSORS.md`, `docs/KEYSPACE.md` | document new `events/ipsec/*` and `ethtool/<iface>/{fec,eee}/*` keys |

---

## 5. Verification

- `cargo build -p zensight-sensor-netlink` / `-p zensight` — clean.
- `cargo test -p zensight-sensor-netlink` — **58 passed** (new `ipsec_events_fold_onto_timeline` + FEC/EEE assertions).
- `cargo test -p zensight` — **276 + 69 passed**.
- `cargo clippy -p zensight-sensor-netlink -p zensight --all-targets -- -D warnings` — clean.
- `cargo fmt … --check` — clean. Design-system color guard — no new `Color::from_rgb`.
- `grep '^name = "nlink"' -A2 Cargo.lock` shows `source = "registry+…crates.io…"` (no `git+`).

**Runtime smoke (unprivileged):** on an IPsec host, add/expire an SA and confirm an
`ipsec` row in the control-plane timeline + `events/ipsec/*` counters increment;
where no xfrm/FEC/EEE exists, expect one warning and unchanged baseline. The
`classify_xfrm_event` → `observe_ipsec` path is unit-tested, so the mapping is
covered without privileged hardware.

---

## 6. Roadmap — remaining 0.23 opportunities (not in this change)

Prioritized by value/effort. None are required; listed for follow-up.

| # | Opportunity | What it buys | Effort |
|---|---|---|---|
| R1 | **Reflector / watch-cache `Store<K,V>` + `ReflectExt::reflect`** | Replace the periodic interface/neighbor/route dumps with an event-fed in-memory cache → lower latency + less kernel chatter; the poll loop becomes a publish-from-cache tick. | M–L (touches the core loop; do behind a config flag) |
| R2 | **`namespace_watcher` (netns) telemetry** | Per-network-namespace interface/socket/route telemetry — container & netns visibility, a real gap today. New `netns/<name>/…` keyspace + a frontend facet. | L |
| R3 | **nftables declarative sets + `nft reconcile`/`diff`** | Let the sentinel assert a *desired* ruleset and alert on drift (beyond today's hit-rate counters). Strong fit for the expectations model. | M |
| R4 | **sockdiag `INET_DIAG_REQ_BYTECODE` port predicates** | Push `sport`/`dport` filters kernel-side too. Needs the query selector to distinguish local-vs-remote (or emit an AND-of-known-port bytecode) to preserve semantics. | S–M |
| R5 | **ethtool RSS / module-EEPROM** | RSS queue layout + optical module (DOM) readouts — deeper NIC/optics health. | M |
| R6 | **WireGuard `from_wg_quick()` / `client()`** | Parse standard `wg-quick` configs for richer peer labelling in the WireGuard view. | S |
| R7 | **XFRM `ExpirePolicy` / `Acquire` sentinel hooks** | Treat IPsec lifecycle events as sentinel-relevant (e.g. alert on hard-expire without re-key, or repeated `Acquire` = tunnel can't establish). Builds directly on §3.1. | S |
| R8 | **`syscall_batch` feature** | Batch the per-tick ethtool/neighbor syscalls to cut poll-tick latency on hosts with many interfaces. | S |
