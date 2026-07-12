# 09 — Operations Cookbook

**Status: Draft** · informative chapter

Worked recipes for the four infrastructure concerns the grammar was shaped
around: subscriptions, storage, ACL, and constrained links. Base =
`zensight` throughout; substitute your deployment's base.

---

## 1. Selector cookbook

| Consumer | Declares | Notes |
|---|---|---|
| UI, full fleet | `zensight/@v1/*/telemetry/**` + `zensight/@v1/*/state/**` + `zensight/@v1/*/events/**` + `zensight/@v1/@catalog/state/entity/*` | three class subs replace firehose-plus-filtering; catalog named explicitly (D4) |
| UI, one host drill-down | `zensight/@v1/h-xxx/**` | complete data plane of one host; cannot pull media/blob/rpc (D2) |
| UI, presence | liveliness sub `zensight/@v1/*/state/*/alive` + `zensight/@v1/*/state/*/device/*/alive` | token keys are the identity; zero payload |
| Exporter (metrics) | `zensight/@v1/*/telemetry/**` | nothing to discard client-side |
| Exporter (alerts) | `zensight/@v1/*/state/*/alert/*` | |
| Protocol-specialist view | `zensight/@v1/*/telemetry/netring/**` | one `*`, protocol-first ergonomics preserved |
| Catalog (evidence intake) | `zensight/@v1/*/state/*/evidence/**` | |
| Late-joiner seeds | `GET` the same state selectors | state is its own seed ([05-control-rpc.md §4](05-control-rpc.md)) |
| Media viewer | exact `…/@media/<producer>/<stream>/preview/jpeg`, or `…/video/<codec>/*` | single-stream only, by construction |

Anti-patterns:

- `zensight/@v1/**` — legal, but you almost never mean it; it spans every
  host's every class. Subscribe per class or per origin.
- Any selector containing `$*` — forbidden ([02-principles.md P6](02-principles.md)).
- Subscribing a data selector to "watch" RPC or media — structurally
  impossible; if you think you need it, the data you want is `state`.

## 2. Router storage configuration

Class-driven storages, all with a literal `strip_prefix` (Zenoh
requirement). Sketch (storage-manager JSON5):

```json5
storages: {
  latest: {                                   // current truth of the fleet
    key_expr: "zensight/@v1/*/state/**",
    strip_prefix: "zensight/@v1",
    volume: "fs",                             // any LWW-honouring backend
  },
  timeseries: {                               // charts and history
    key_expr: "zensight/@v1/*/telemetry/**",
    strip_prefix: "zensight/@v1",
    volume: { id: "influxdb", db: "telemetry" },
  },
  events: {                                   // immutable record, retention-windowed
    key_expr: "zensight/@v1/*/events/**",
    strip_prefix: "zensight/@v1",
    volume: { id: "influxdb", db: "events" },
  },
  catalog: {                                  // explicit: '*' never matches @catalog (D4)
    key_expr: "zensight/@v1/@catalog/state/**",
    strip_prefix: "zensight/@v1/@catalog",
    volume: "fs",
  },
  pdns_history: {                             // history of LWW state = a storage choice (04 §4)
    key_expr: "zensight/@v1/@catalog/state/pdns/**",
    strip_prefix: "zensight/@v1/@catalog/state/pdns",
    volume: { id: "influxdb", db: "pdns" },
  },
}
```

Notes:

- The `latest` storage doubles as the fleet-wide late-joiner seed: a GET on
  any state selector is answered by the router even when producers sleep.
- Media is never stored (recording is a deliberate consumer, not a storage
  rule); blob chunks MAY be stored to make the router a content cache
  ([07-bulk-planes.md §2](07-bulk-planes.md)).

## 3. ACL recipes

All rules are literal prefixes + one trailing `**` — the fast path — and
every boundary is a fixed position, so rules survive vocabulary growth
untouched. Sketch:

```json5
access_control: {
  enabled: true, default_permission: "deny",
  rules: [
    // a sensor host may publish only its own subtree…
    { id: "host-pub", permission: "allow", flows: ["ingress"],
      messages: ["put", "delete", "liveliness_token", "reply"],
      key_exprs: ["zensight/@v1/h-3fa9c2d41b7e/**"] },     // one rule per enrolled host (cert CN ↔ origin)
    // …and may not touch the catalog
    // (implicit: deny by default; @catalog needs its own allow for the catalog's identity)
    { id: "catalog-pub", permission: "allow", flows: ["ingress"],
      messages: ["put", "delete", "liveliness_token", "reply"],
      key_exprs: ["zensight/@v1/@catalog/**"] },
    // operator console: read everything, write nothing but RPC
    { id: "ops-read", permission: "allow", flows: ["egress"],
      messages: ["put", "delete", "reply"],
      key_exprs: ["zensight/@v1/**"] },
    { id: "ops-rpc", permission: "allow", flows: ["ingress"],
      messages: ["query"],
      key_exprs: ["zensight/@v1/*/@rpc/**"] },
    // dangerous procedures deniable per-key, because the key IS the target:
    { id: "no-remote-actions", permission: "deny", flows: ["ingress"],
      messages: ["query"],
      key_exprs: ["zensight/@v1/*/@rpc/systemd/action"] },
  ],
}
```

The property to notice: *"host X may publish only as itself"* and *"nobody
but the console may invoke actions"* are single literal-prefix rules —
inexpressible in a keyspace where the host discriminator is a mutable name
at varying positions.

## 4. Constrained links

A bandwidth-limited leaf (radio, cell, tactical link) is provisioned by
prefix allowlist on its router — the keyspace *is* the bandwidth policy
(the Indy-Autonomous-Challenge pattern, [10-prior-art.md §1](10-prior-art.md)):

```
allow  zensight/@v1/h-xxx/state/**                       # truth, small, must flow
allow  zensight/@v1/h-xxx/telemetry/sysinfo/**           # chosen rollups only
allow  zensight/@v1/h-xxx/events/**                      # rare by budget
deny   zensight/@v1/h-xxx/telemetry/netring/**           # firehose stays local
# @rpc/@blob pass on demand — pull-only costs nothing unasked (P9)
# @media never crosses unless a viewer explicitly subscribes a stream
```

Complementary conventions already assumed by the classes: superseded
streams drop under congestion, must-arrive state blocks
([04-planes.md §3](04-planes.md)); payloads are compact self-describing
binary (CBOR in the reference application); high-cardinality detail stays
pull-only.

## 5. Debugging etiquette

- `z_sub 'zensight/@v1/*/state/**'` shows fleet truth; add
  `…/@catalog/state/entity/*` to see conclusions.
- Reading a raw key aloud is the parse: *base, version, origin, class,
  producer, subject* — position 3 is always who, position 4 is always what
  kind. No lookup table required; that property is worth defending in
  review.
- If a needed selector is awkward to write, that is registry feedback —
  file it against the subject layout before inventing a client-side filter
  ([08-registry.md §5](08-registry.md)).
