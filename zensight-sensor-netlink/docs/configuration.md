# netlink configuration

JSON5, loaded with `--config`. The full annotated example is
[`../../configs/netlink.json5`](../../configs/netlink.json5). This page documents
each block; defaults are as parsed by `src/config.rs`.

## Top level

```json5
{
  zenoh: { mode: "peer" },        // "client" | "peer" | "router"
  serialization: "json",          // "json" | "cbor"  (default json)
  artifacts: { report: { ... } }, // on-demand artifact procedures (disabled by default)
  netlink: { ... },
  logging: { level: "info" },
}
```

## `netlink`

> There is no `key_prefix` knob — it was retired in #465. The producer chunk
> (`netlink`) is a constant of this crate and the origin is derived from the host's
> machine-id, so keys land under `zensight/v1/<origin>/…/netlink/…` with nothing to
> configure. A `key_prefix:` line left over from 0.7.0 is **silently ignored**.

| Key | Default | Meaning |
|---|---|---|
| `source` | `"auto"` | telemetry `source` id (payload field — not part of the key); `"auto"` detects the hostname |
| `poll_interval_secs` | `5` | poll cadence (config example uses `2`) |
| `collect` | see below | per-collector toggles |
| `events` | `{ ring_capacity: 256 }` | recent-events ring size for `@rpc/netlink/events` |
| `interfaces` | see below | interface include/exclude filter |
| `wireguard` | see below | WireGuard peer monitoring |
| `expectations` | none | embedded sentinel — see [sentinel.md](sentinel.md) |
| `ebpf` | see below | opt-in eBPF module tuning (only when `collect.ebpf`) |
| `evidence` | see below | identity-evidence feed to the correlator |

### `netlink.collect`

| Toggle | Default | Privilege | What |
|---|---|---|---|
| `interfaces` | `true` | none | per-interface counters + state |
| `sockets` | `true` | none | TCP socket-state aggregates (sock_diag) |
| `neighbors` | `true` | none | ARP/NDP neighbor summary (also drives evidence) |
| `routes` | `true` | none | routing-table summary + default-route flaps |
| `diagnostics` | `true` | none | nlink bottleneck score + issue counts |
| `events` | `true` | none | real-time RTNETLINK event stream + timeline |
| `ethtool` | `true` | none | link speed/duplex/autoneg, rings, FEC, EEE |
| `addresses` | `true` | none | IP address inventory (per-family + global) |
| `tc` | `true` | none | TC/QoS qdisc drops/overlimits/backlog |
| `xfrm` | `true`* | none | IPsec/XFRM SA + policy health + monitor events |
| `nftables` | `false` | `CAP_NET_ADMIN` | nftables table/chain/rule + hit-rate counters |
| `conntrack` | `false` | `CAP_NET_ADMIN` | conntrack table summary |
| `ebpf` | `false` | `CAP_BPF`+`CAP_NET_ADMIN` | connect-latency + retransmit/tcplife (needs `--features ebpf` build) |
| `socket_processes` | `true` | none | socket→process attribution on `@rpc/netlink/sockets` (#304) |
| `socket_process_max_procs` | `4096` | — | skip the `/proc` fd-walk above this many processes |
| `bandwidth` | `true` | none | per-process TCP goodput on `@rpc/netlink/bandwidth` (#317) |

\* The struct default for `xfrm` is `true`, but the shipped
`configs/netlink.json5` sets it **`false`** — nlink's XFRM dump trips a cosmetic
ratelimited kernel warning on every poll and the family is empty without IPsec
anyway (nlink issue #160). Re-enable it (and `collect.events`) to get the XFRM
monitor `events/ipsec/*` stream.

### `netlink.interfaces`

```json5
interfaces: {
  include: [],            // only these (empty = all)
  exclude: [],            // exclude these names
  exclude_loopback: false,
  exclude_virtual: false, // docker*, veth*, br-*, virbr*, vnet*, tap*
}
```

### `netlink.wireguard`

```json5
wireguard: {
  interfaces: [],          // WG interfaces to poll, e.g. ["wg0"]  (empty = off)
  stale_after_secs: 180,   // a peer is "up" if last handshake within this
  wg_quick_configs: [],    // *.conf paths to enrich peer labels (AllowedIPs/endpoint, #268)
}
```

### `netlink.ebpf`

Only read when `collect.ebpf` is set on a `--features ebpf` build.

```json5
ebpf: {
  conn_ring_capacity: 256, // recent-connections ring (@rpc/netlink/connections)
  retransmit_top_k: 20,    // top peers returned by @rpc/netlink/retransmits
}
```

### `netlink.evidence` (#307)

Republishes observed ARP/NDP neighbors as third-party identity evidence on
`zensight/v1/<origin>/state/netlink/evidence/device/<device>` for the
correlator. Change-driven with a periodic liveness refresh.

```json5
evidence: {
  enabled: true,
  min_interval_secs: 60,   // floor between evidence feed runs (per source)
  refresh_secs: 420,       // re-emit live claims at least this often (≤ ttl/2)
  max_per_tick: 200,       // hard cap on records emitted per run
}
```

### `netlink.expectations`

The embedded sentinel. Every kind is documented in
[sentinel.md](sentinel.md); the top-level knobs are `eval_interval_secs`
(default 3) and `default_for_secs` (debounce, default 3).

## `artifacts`

The on-demand artifact surface (framework-wide, `zensight-sensor-core`):
`@rpc/netlink/artifact/request` + `artifact/cancel` procedures, a lifecycle
status doc at `state/netlink/artifact/<kind>`, and bulk bytes on the `@blob`
plane. Every kind is **disabled by default**; enable `report` to allow
downloading a redacted `tar.zst` debug bundle from the GUI.

```json5
artifacts: {
  report: {
    enabled: false,
    max_bytes: 67108864,   // 64 MiB cap (uncompressed)
    cooldown_secs: 30,     // min gap between generations
    ttl_secs: 600,         // how long a bundle stays downloadable
    chunk_size: 524288,    // transfer chunk (clamped 256 KiB–1 MiB)
    // redact_extra: ["my_custom_secret_field"],
  },
}
```

## eBPF build

The eBPF tier is out of the default `cargo build --workspace` and stable CI. To
build it:

```bash
cargo build -p zensight-sensor-netlink --release --features ebpf
# needs nightly + rust-src + bpf-linker to compile; CAP_BPF + CAP_NET_ADMIN to run
```

Off / missing caps / unsupported kernel → one warning and the unprivileged
baseline is unchanged.
