# netlink sentinel

The netlink sensor embeds a **sentinel**: it evaluates declared **expectations**
about *this* host and emits alerts on deviation, auto-resolving when the
expectation is satisfied again. Expectations are configured under
`netlink.expectations` and can be hot-swapped at runtime from the GUI.

Each expectation carries a `severity` (`info`/`warning`/`critical`, default
`warning`) and an optional per-expectation `for_secs` debounce (fall back to
`default_for_secs`). Alerts are published on
`zensight/@v1/<origin>/state/netlink/alert/<alert_key>` as a lifecycle
(firing → resolved → Delete tombstone), where `<alert_key>` is a stable 16-hex
FNV-1a hash of `rule + labels` (the origin chunk already identifies the host,
so `source` is not hashed). The rule slug is `<kind>:<name>`.

## Expectation kinds

| Config field | Rule slug | What it asserts |
|---|---|---|
| `sockets` | `socket:<name>` | a port must be `listen`ing, or have ≥ `min` ESTABLISHED connections to `established_to` (`host:port`), or a port must **not** be listening (`forbid_listen`) |
| `links` | `link:<name>` | interface `iface` must be `up` |
| `neighbors` | `neighbor:<name>` | IP `ip` must be a `reachable` ARP/NDP neighbor (gateway/peer reachability) |
| `routes` | `route:<name>` | a default route must be present (`default_present`), optionally `default_via` a specific gateway |
| `metrics` | `metric:<name>` | a metric path satisfies `op value` (generic threshold — promotes a GUI threshold rule to a headless expectation; shares `ComparisonOp` with the frontend) |
| `rates` | `rate:<name>` | a metric must not increase by more than `max_increase_per_min` (measured between sentinel sweeps) |
| `delivery` | `delivery:<name>` | a socket-group delivery-rate percentile stays at/above `floor` bytes/sec (default metric `sockets/tcp/delivery_rate_p50`, from the enriched `tcp_info`, #113) |
| `route_flaps` | `route_flap:<name>` | the default route flaps no more than `max_flaps` times within `window_secs` (watches a cumulative counter, default `events/route/removed_total`) |
| `rules` | `rules:<name>` | policy-routing (`ip rule`) forbid/require — see below (#323) |

### Socket expectation fields

```json5
{ name: "sshd",     listen: 22,        severity: "critical" }          // must LISTEN on 22
{ name: "no-telnet", forbid_listen: 23, severity: "critical" }          // must NOT listen on 23
{ name: "db-conn",  established_to: "10.0.0.5:5432", min: 1 }           // ≥1 ESTABLISHED to db
```

### Policy-routing rule expectations (`rules`, #323)

An `ip rule` change is the classic "why is routing weird" incident *and* a
traffic-redirect primitive, so the sentinel gains a dedicated kind. Match by
`priority` and/or `table` (an unset field matches any):

- `sense: "forbid"` (default) — fire on **any** non-baseline rule matching the
  selectors. The kernel's three baseline lookup rules (priority 0 / 32766 /
  32767) never count. This is the **traffic-diversion guard** ("table main not
  bypassed").
- `sense: "require"` — fire when **no** matching rule exists (pin a VPN/mark rule
  that must stay installed).

```json5
rules: [
  { name: "no-diversion", sense: "forbid",  severity: "critical" },
  { name: "vpn-rule",     sense: "require", priority: 100, table: 51820 },
]
```

A `NewRule`/`DelRule` event re-evaluates the sentinel instantly (via the event
wake path — no wait for the next sweep). Rule violations are tagged **MITRE
ATT&CK T1599 (Network Boundary Bridging)** and surface in the GUI Security
view's tactic lens.

## Evaluation cadence

- `eval_interval_secs` — how often the sentinel sweeps (default 3).
- `default_for_secs` — global debounce: an expectation must be violated
  continuously for this many seconds before its alert fires (default 3). Any
  expectation may override with its own `for_secs`.

Rate and route-flap checks retain the previous sample / sliding window inside the
evaluator, so they need two sweeps before they can fire.

## Runtime hot-swap — `@rpc/netlink/expectations` + `.../set`

Expectations can be replaced live without a restart. Both legs are request/reply
GETs on the `@rpc` plane (there is no pub-sub command channel):

- **Write** (GET with payload): `zensight/@v1/<origin>/@rpc/netlink/expectations/set`
  — a `SetExpectations(ExpectationsConfig)` command payload **replaces the set
  wholesale**; failures ride `reply_err` with a namespaced `error/...` name.
  Expectations authored in the GUI (Expectations view) are merged with the
  config-declared ones.
- **Read** (plain GET): `zensight/@v1/<origin>/@rpc/netlink/expectations` —
  returns the currently active `ExpectationsConfig`.

A separate `@rpc/netlink/collection` (+ `.../collection/set`) procedure pair
toggles the `collect.*` collectors at runtime.
