# 09 — Operations Cookbook

**Status: Draft** · informative chapter

Worked recipes for the infrastructure concerns the grammar was shaped
around: session setup, subscriptions, storage, ACL, and constrained links.
Base = `zensight` throughout; substitute your deployment's base.

---

## 0. Session configuration

Every participant sets the base as its session **namespace** (stable
config; [03-grammar.md §1.1](03-grammar.md)) and enables timestamping
(required by state LWW, storages, and publisher caches):

```json5
{
  namespace: "zensight",                 // = <base>; app code never spells it
  timestamping: { enabled: true },       // peers/clients default to false — turn it on
  // mode/connect/listen as the deployment requires
}
```

With the namespace set, everything the application declares is
base-relative (`@v1/…`), and ingress from outside the namespace is
filtered — the base is an isolation boundary, not a string convention.
Remember the two scope rules: router-side config (storages, ACL,
interceptors — everything below) is written with the **full** key, and a
namespaced session cannot reach the router admin space (§5).

## 1. Selector cookbook

| Consumer | Declares | Notes |
|---|---|---|
| UI, full fleet | `zensight/@v1/*/telemetry/**` + `zensight/@v1/*/state/**` + `zensight/@v1/*/events/**` + `zensight/@v1/@catalog/state/entity/*` + `zensight/@v1/@catalog/state/alias/*` | three class subs replace firehose-plus-filtering; catalog (and its alias records — the origin→entity re-pointing on merges) named explicitly (D4) |
| UI, one host drill-down | `zensight/@v1/h-xxx/**` | complete data plane of one host; cannot pull media/blob/rpc (D2) |
| UI, presence | liveliness subs `zensight/@v1/*/state/*/alive` + `zensight/@v1/*/state/*/device/*/alive` + `zensight/@v1/@catalog/state/alive` | token keys are the identity; zero payload; the catalog token named explicitly (D4 — `*` never matches it), else "catalog dead" is indistinguishable from "no entities" |
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
requirement; a verbatim chunk inside it is fine — validation is
string-prefix + no-wildcards). Sketch (storage-manager JSON5):

```json5
plugins: {
  storage_manager: {
    volumes: {                                // REQUIRED for non-memory volumes;
      fs: {},                                 // each needs its backend plugin installed:
      influxdb: { url: "http://localhost:8086" },  // zenoh-backend-{filesystem,influxdb},
    },                                        // out-of-tree, version-matched to the router
    storages: {
      latest: {                                   // current truth of the fleet
        key_expr: "zensight/@v1/*/state/**",
        strip_prefix: "zensight/@v1",
        volume: { id: "fs", dir: "latest" },      // any LWW-honouring backend
      },
      timeseries: {                               // charts and history
        key_expr: "zensight/@v1/*/telemetry/**",
        strip_prefix: "zensight/@v1",
        volume: { id: "influxdb", db: "telemetry" },
      },
      events: {                                   // immutable record; retention is the
        key_expr: "zensight/@v1/*/events/**",     // DATABASE's policy (InfluxDB RP) —
        strip_prefix: "zensight/@v1",             // zenoh's garbage_collection GCs metadata
        volume: { id: "influxdb", db: "events" },
      },
      catalog: {                                  // explicit: '*' never matches @catalog (D4)
        key_expr: "zensight/@v1/@catalog/state/**",
        strip_prefix: "zensight/@v1/@catalog",
        volume: { id: "fs", dir: "catalog" },
      },
      pdns_history: {                             // history of LWW state = a storage choice (04 §4)
        key_expr: "zensight/@v1/@catalog/state/pdns/**",
        strip_prefix: "zensight/@v1/@catalog/state/pdns",
        volume: { id: "influxdb", db: "pdns" },
      },
    },
  },
}
```

Notes:

- The `latest` storage doubles as the fleet-wide late-joiner seed: a GET on
  any state selector is answered by the router even when producers sleep.
  (It is what makes plain-GET seeds work at all —
  [05-control-rpc.md §4](05-control-rpc.md).) Timestamping must be enabled
  on the publishing side for LWW to be meaningful
  ([04-planes.md §4](04-planes.md)).
- `catalog` and `pdns_history` **overlap**: a GET under `…/state/pdns/**`
  is answered by both (duplicate, possibly divergent replies). Either
  accept it (subscribers are unaffected; GET consumers consolidate) or
  carve `pdns/**` out of the `catalog` storage's selector.
- Media is never stored (recording is a deliberate consumer, not a storage
  rule); blob chunks MAY be stored to make the router a content cache
  ([07-bulk-planes.md §2](07-bulk-planes.md)).

### 2.1 Choosing volumes

A backend advertises a capability pair — *persistence* (volatile/durable)
× *history* (latest/all) — and the class semantics pick it:

| Volume | Capability | Use for | Caveats |
|---|---|---|---|
| `memory` (bundled) | volatile · latest | seed-only deployments, testing | gone on router restart — late joiners lose their seed until state refreshes |
| `fs` / `rocksdb` | durable · latest | `latest`, `catalog` — the LWW truth stores | out-of-tree plugins, version-matched to the router |
| `influxdb` | durable · all | `timeseries`, `events`, `pdns_history` — anything whose value is the *sequence* | retention lives in the database (RP), not zenoh config |

Storages are read-write by construction (there is no `read_only` field);
restrict who can write *into* a storage's selector with ACL, not storage
config. Out-of-order and wildcard writes are safe: the storage applies
updates by timestamp (an outdated sample is discarded, a wildcard delete
still masks a slower concrete put).

### 2.2 Replication (HA for the seed store)

The `latest` storage is a single point of seed failure; Zenoh storage
**replication** (anti-entropy digest alignment between storages on
different routers) fixes that for latest-value storages:

```json5
latest: {
  key_expr: "zensight/@v1/*/state/**",
  strip_prefix: "zensight/@v1",
  volume: { id: "fs", dir: "latest" },
  replication: {
    interval: 10.0,          // digest period, seconds
    sub_intervals: 5,
    hot: 6, warm: 30,        // eras of decreasing digest resolution
    propagation_delay: 250,  // ms; MUST be < interval/2
  },
},
```

Rules: every replica MUST use the **identical** `key_expr`,
`strip_prefix`, and replication parameters (divergent parameters cause
digest storms, not errors); replication requires timestamps and works
**only on latest-value backends** — the influx history storages do not
replicate at the Zenoh layer, their availability is the database's
concern (cluster the database, or accept a history gap on router loss).

A **replicated, fully-covering** `latest` storage is also the one place
`complete: true` is *right*: it lets the router answer any state GET from
the nearest replica without fanning out. Keep the round-1 caveat in mind —
never mark a storage complete whose selector intersects `@rpc` fan-in
paths ([05-control-rpc.md §2.1](05-control-rpc.md)); the class-scoped
selectors above cannot (verbatim `@rpc` is unreachable from `state/**`).

### 2.3 Garbage collection = tombstone lifetime

`garbage_collection: { period, lifespan }` on a storage prunes *metadata*
— including deletion tombstones — older than `lifespan` (default 24 h).
This is the knob behind the convention's tombstone rule
([04-planes.md §1.2](04-planes.md)): set `lifespan` ≥ the longest
`ttl_s` in the registry, or a retired key's tombstone can be GC'd while
consumers are still entitled to see it, and a slow replica may resurrect
the key.

## 3. ACL recipes

Four facts of Zenoh ACL shape every recipe, and the first two follow from
the convention's own algebra:

1. **Matching is keyexpr *inclusion*** (rule ⊇ message key), and `**` never
   crosses a verbatim chunk in inclusion either. So a host's
   `…/h-xxx/**` rule does **not** cover its `@rpc` replies, `@media`
   frames, or `@blob` keys — the hermeticity that protects selectors cuts
   ACL prefixes identically. Per-principal ACL is therefore a **fixed set
   of literal-prefix rules, one per plane** (~4 per host), not one rule.
2. A rule with `*` in the origin position never covers `@catalog` (D4
   applies to ACL keyexprs too); catalog access is always its own rule.
3. The config requires **three lists** — `rules`, `subjects`, and
   `policies` binding them; rules alone are rejected at router startup. The
   cert-CN ↔ origin enrollment ([03-grammar.md §4 D6](03-grammar.md))
   lives in `subjects`.
4. Under `default_permission: "deny"`, *declarations* need allowing too:
   a consumer that may not `declare_subscriber` receives nothing, a
   producer that may not `declare_queryable` serves nothing, and queries
   must be allowed **egress** toward the responder's face as well as
   ingress from the caller's.

Sketch (structure verified against the Zenoh 1.9 schema; validate against
a live `zenohd` before deploying):

```json5
access_control: {
  enabled: true, default_permission: "deny",

  rules: [
    // ---- sensor host (template: one set per enrolled host) ----
    { id: "host-data",  permission: "allow", flows: ["ingress"],
      messages: ["put", "delete", "liveliness_token"],
      key_exprs: ["zensight/@v1/h-3fa9c2d41b7e/**"] },          // data classes + alive tokens
    { id: "host-media", permission: "allow", flows: ["ingress"],
      messages: ["put"],
      key_exprs: ["zensight/@v1/h-3fa9c2d41b7e/@media/**"] },   // plane needs its own rule (fact 1)
    { id: "host-serve", permission: "allow",
      messages: ["declare_queryable", "reply", "query"],        // query egress = router forwards calls to it
      key_exprs: ["zensight/@v1/h-3fa9c2d41b7e/@rpc/**",
                  "zensight/@v1/h-3fa9c2d41b7e/@blob/**"] },
    // hosts that seed the router @blob content store use the sanctioned
    // one-shot PUT path (04-planes §3) — grant it explicitly, or omit this
    // rule in deployments without a router content store:
    { id: "host-blob-seed", permission: "allow", flows: ["ingress"],
      messages: ["put"],
      key_exprs: ["zensight/@v1/h-3fa9c2d41b7e/@blob/store/**",
                  "zensight/@v1/h-3fa9c2d41b7e/@blob/tree/**"] },
    // ONLY for hosts on the advanced tier (04-planes §3.3): the sidecars
    // (cache queryable, liveliness token, heartbeat publisher at
    // <key>/@adv/pub/<zid>/…) live under a verbatim @adv suffix the
    // host-data '**' rule cannot reach. Omitting this rule when the tier
    // is in use fails SILENTLY — empty seeds, dead recovery:
    { id: "host-adv", permission: "allow",
      messages: ["put", "liveliness_token",
                 "declare_queryable", "reply", "query"],
      key_exprs: ["zensight/@v1/h-3fa9c2d41b7e/**/@adv/**"] },

    // ---- catalog service ----
    { id: "catalog-own", permission: "allow",
      messages: ["put", "delete", "liveliness_token",
                 "declare_queryable", "reply", "query"],
      key_exprs: ["zensight/@v1/@catalog/**",
                  "zensight/@v1/@catalog/@rpc/**"] },
    // intake is split by flow: the catalog DECLARES interest (ingress) and
    // RECEIVES data/tokens (egress) — a flowless rule here would let the
    // catalog principal ingress-publish and tombstone ANY host's keys,
    // defeating the per-host enrollment story:
    { id: "catalog-intake-declare", permission: "allow", flows: ["ingress"],
      messages: ["declare_subscriber", "declare_liveliness_subscriber",
                 "liveliness_query", "query"],
      key_exprs: ["zensight/@v1/**"] },
    { id: "catalog-intake-recv", permission: "allow", flows: ["egress"],
      messages: ["put", "delete", "liveliness_token", "reply"],
      key_exprs: ["zensight/@v1/**"] },

    // ---- operator console: read everything, write nothing but RPC ----
    // (the **/@adv/** entries carry AdvancedSubscriber traffic: history/
    //  recovery GETs to publisher caches, late-publisher liveliness
    //  detection, and the console's own subscriber-detection token)
    { id: "ops-sub", permission: "allow", flows: ["ingress"],
      messages: ["declare_subscriber", "declare_liveliness_subscriber",
                 "liveliness_query", "query"],
      key_exprs: ["zensight/@v1/**", "zensight/@v1/@catalog/**",
                  "zensight/@v1/*/@rpc/**", "zensight/@v1/@catalog/@rpc/**",
                  "zensight/@v1/*/@blob/**", "zensight/@v1/*/@media/**",
                  "zensight/@v1/**/@adv/**"] },
    // the console's OWN token (advanced-tier subscriber detection) is
    // confined to @adv — a broad ingress liveliness_token allow would let
    // the console forge any host's `state/*/alive` roster entry:
    { id: "ops-own-token", permission: "allow", flows: ["ingress"],
      messages: ["liveliness_token"],
      key_exprs: ["zensight/@v1/**/@adv/**"] },
    { id: "ops-recv", permission: "allow", flows: ["egress"],
      messages: ["put", "delete", "reply", "liveliness_token"],
      key_exprs: ["zensight/@v1/**", "zensight/@v1/@catalog/**",
                  "zensight/@v1/*/@rpc/**", "zensight/@v1/@catalog/@rpc/**",
                  "zensight/@v1/*/@blob/**", "zensight/@v1/*/@media/**",
                  "zensight/@v1/**/@adv/**"] },

    // dangerous procedures deniable per-key, because the key IS the target
    // (deny wins; sound under default-deny — an origin-`**` query that would
    // sidestep this literal also matches no allow rule):
    { id: "no-remote-actions", permission: "deny",
      messages: ["query"],
      key_exprs: ["zensight/@v1/*/@rpc/systemd/action"] },
  ],

  subjects: [
    // enrollment: transport identity ↔ origin (03-grammar §4 D6)
    { id: "host-3fa9", cert_common_names: ["h-3fa9c2d41b7e"] },
    { id: "catalog",   cert_common_names: ["zensight-catalog"] },
    { id: "console",   cert_common_names: ["zensight-console"] },
  ],

  policies: [
    { rules: ["host-data", "host-media", "host-serve",
              "host-blob-seed", "host-adv"],                 subjects: ["host-3fa9"] },
    { rules: ["catalog-own", "catalog-intake-declare",
              "catalog-intake-recv"],                        subjects: ["catalog"] },
    { rules: ["ops-sub", "ops-recv", "ops-own-token"],       subjects: ["console"] },
    { rules: ["no-remote-actions"],                          subjects: ["console"] },
  ],
}
```

The property to notice survives the plane-per-rule tax: *"host X may act
only as itself"* and *"nobody but the console may invoke actions"* are
still **static literal-prefix rules pinned to enrolled identities** —
inexpressible in a keyspace where the host discriminator is a mutable name
at varying positions. And one more rule of thumb: a rule's `key_exprs`
must **include** (⊇) the consumer's declared selector, not merely
intersect it — allow `zensight/@v1/**` does not admit a `zensight/**`
subscriber.

## 4. Constrained links

A bandwidth-limited leaf (radio, cell, tactical link) is provisioned by
prefix policy on its router — the keyspace *is* the bandwidth policy (the
Indy-Autonomous-Challenge pattern, [10-prior-art.md §1](10-prior-art.md)).
Zenoh has no dedicated "bandwidth allowlist"; the policy is expressed with
two real mechanisms, both selecting on the same class prefixes:

- **`access_control` scoped to the link's interface**
  (`subjects: [{ id: "radio", interfaces: ["wlan0"] }]`), with the §3
  pattern: allow `…/h-xxx/state/**` (truth, small, must flow),
  `…/h-xxx/telemetry/sysinfo/**` (chosen rollups), `…/h-xxx/events/**`
  (rare by budget); the un-allowed `telemetry/netring/**` firehose then
  stays local by default-deny. The §3 caveats apply verbatim — in
  particular, "@rpc/@blob on demand" is not free under default-deny: the
  query/reply/declare legs must be explicitly allowed per plane, which is
  five more literal-prefix rules, not zero.
- **`downsampling`** on the link's interface for rate-limits softer than
  allow/deny: `[{ interfaces: ["wlan0"], rules: [{ key_expr:
  "zensight/@v1/*/telemetry/**", freq: 0.1 }] }]` — telemetry crosses at
  ≤ 0.1 Hz, state and events untouched.
- **`qos` overwrite interceptor** to *enforce* the class QoS profiles
  ([04-planes.md §3](04-planes.md)) at the router regardless of what
  publishers set: one rule per class prefix, e.g. force
  `zensight/@v1/*/telemetry/**` to `priority: "data_low"` and
  `zensight/@v1/*/state/*/alert/*` to
  `{ priority: "interactive_high", congestion_control: "block" }`. The
  interceptor ignores API-level QoS — deployment policy wins.

Advanced-tier traffic deserves a thought on constrained links: per-key
miss-detection heartbeats and declare-time history bursts are real bytes
(the cost box in [04-planes.md §3.3](04-planes.md)). The baseline
([04 §3.2](04-planes.md)) creates none of it — no per-key entities, no
heartbeats — which is why it is the default; the tier is opt-in per
subject, and a constrained leaf simply doesn't opt in: a leaf consumer
runs a plain subscriber + local store instead of `history()` (the
reference GUI's constrained profile does exactly that).

Complementary conventions already assumed by the classes: superseded
streams drop under congestion, must-arrive state blocks
([04-planes.md §3](04-planes.md)); payloads are compact self-describing
binary (CBOR in the reference application); high-cardinality detail stays
pull-only; `@media` crosses only for an explicitly subscribed stream (and
can simply not be allowed on the link at all).

## 5. Debugging etiquette

- `z_sub 'zensight/@v1/*/state/**'` shows fleet truth; add
  `…/@catalog/state/entity/*` to see conclusions. (Debug tools run
  *without* the namespace and spell full keys — which is also the honest
  view of what is on the wire.)
- Router administration (`GET @/<zid>/**`, admin space) needs an
  **un-namespaced** session: a namespaced session's selector is rewritten
  to `zensight/@/<zid>/**` and matches nothing
  ([03-grammar.md §1.1](03-grammar.md)).
- Reading a raw key aloud is the parse: *base, version, origin, class,
  producer, subject* — the chunk after `@v1` is always who, the next is
  always what kind (positions are base-relative: multi-chunk bases are
  legal, [03-grammar.md §1.1](03-grammar.md)). No lookup table required;
  that property is worth defending in review.
- If a needed selector is awkward to write, that is registry feedback —
  file it against the subject layout before inventing a client-side filter
  ([08-registry.md §5](08-registry.md)).
