# ZenSight Zenoh Keyspace Reference

**The deployed keyspace is the keyspace-v2 convention, v1.0 (ratified).**
The normative reference is the RFC set in
[`docs/rfcs/keyspace-v2/`](rfcs/keyspace-v2/00-index.md); ZenSight's concrete
profile (constants, per-sensor worked examples, the mapping of every shipped
key family) is [chapter 11](rfcs/keyspace-v2/11-zensight-profile.md). The
migration was executed under epic
[#453](https://github.com/p13marc/zensight/issues/453); the pre-v1 keyspace
this file used to document is retired — no shipped component publishes or
subscribes to it.

## The deployed profile in one screen

Grammar (RFC [03](rfcs/keyspace-v2/03-grammar.md)):

```
zensight/v1/<origin>/<class>/<producer>/<subject...>     data planes
zensight/v1/<origin>/@rpc/<producer>/<procedure...>      request/reply
zensight/v1/<origin>/@media/<producer>/<stream>/…        opaque video
zensight/v1/<origin>/@blob/{artifact,tree,store}/…       bulk content
zensight/v1/@catalog/…                                   the identity catalog
```

- `<origin>` = `h-<12hex>` (sha256 of machine-id + salt, RFC
  [06](rfcs/keyspace-v2/06-identity.md)); the catalog service publishes under
  the verbatim `@catalog` origin.
- `<class>` = `telemetry` (periodic samples) · `state` (LWW documents:
  health, errors, alerts, evidence, expectations, stream/artifact docs) ·
  `events` (append-only records). Classes are disjoint by construction; the
  planes (`@rpc`/`@media`/`@blob`) are verbatim chunks no data selector can
  reach (RFC [04](rfcs/keyspace-v2/04-planes.md),
  [07](rfcs/keyspace-v2/07-bulk-planes.md)).
- Presence = liveliness tokens at `…/state/<producer>/alive` (+
  `…/state/<producer>/device/<device>/alive`,
  `…/@catalog/state/alive`). Alive ⇒ callable: RPC queryables are declared
  before the token.
- Commands do not exist: writes are GETs on `…/@rpc/<producer>/<topic>/set`,
  reads on `…/@rpc/<producer>/<topic>` (RFC
  [05](rfcs/keyspace-v2/05-control-rpc.md)). Fleet callers select
  `zensight/v1/*/@rpc/…` with query target `All`.
- Late joiners seed with a plain GET on the same state selectors (state is
  its own seed; storage-shaped queryables answer one reply per concrete key).

## Where the machine-readable truth lives

- **Registry** (per-producer subjects/procedures, QoS entitlements, lints):
  [`zensight-keyspace/registry/*.toml`](../zensight-keyspace/registry/) —
  compiled by `zensight-keyspace`'s `build.rs` into typed builders/parsers;
  registry violations are build errors. Sensors serve their compiled slice at
  `…/@rpc/<producer>/introspect` — and the GUI's **Fleet** view calls it, parsing
  the reply into a `zensight_keyspace::RegistrySlice` and diffing it against the
  slice it compiled in. RFC 08 §6: a disagreement is a *finding*, not an ambiguity.
- **The registry is load-bearing.** Publishing a telemetry subject that is not
  registered panics in debug builds and warns once per name in release
  (`zensight_common::metric_guard`). This is only meaningful because the six host
  producers (sysinfo, netlink, netring, systemd, logs, parallax) register their
  telemetry as real subject families rather than a `{metric...}` catch-all — a
  catch-all makes the lint vacuously true (issue #468). `snmp`/`modbus`/`gnmi`/
  `netflow` keep a rest-var by design: their metric tree belongs to the polled
  device, not to us.
- **Type table** (RFC 08 §5): [`zensight_common::payload`](../zensight-common/src/payload.rs)
  maps each registry `type` name to its Rust definition, and `decode_payload()`
  turns a name into a decoder. This is what makes `type = "TelemetryPoint"`
  resolvable outside the producer's own crate — a consumer can now go wire key →
  subject → type → value with nothing producer-specific compiled in. The
  `types_are_total` test is the CI enforcement the RFC asks for ("a `type` name
  not present in the type table fails CI"): it walks every subject of every
  registry file and fails if a declared type does not resolve. It found one that
  did not — `parallax` declared `StreamDoc` for a subject whose payload is a
  `StreamStatus`, a name that existed nowhere else in the workspace.
- **Bus explorer**: [`zenctl`](../zenctl/README.md) is the `busctl`/`d-feet`
  equivalent RFC 08 §6 exists to enable — `topic list/info/echo`, `node list`,
  `service list/call`, and `doctor` (fan `introspect` fleet-wide, diff each reply
  against this build's slice, print the findings).
- **Key builders**: `zensight-keyspace::V1Context` (producer-side),
  `zensight_common::keyexpr` (consumer-side selectors + fleet/origin RPC
  keys), `zensight_common::command` (topic/artifact procedure keys). New code
  MUST build keys through these — never ad-hoc `format!`.
- **Guard tests**: `zensight-keyspace/tests/guard.rs` pins the D1–D6
  disjointness algebra; consumer crates pin their own selector shapes.

## The version chunk is plain (`v1`), not verbatim

Everything `@`-prefixed in the grammar is **verbatim** — invisible to `*` and
`**`. That is what keeps the planes out of data selectors (D2) and `@catalog`
out of a `*` origin (D4). The version chunk is deliberately **not** one of them.

It was `@v1` through the migration. Zenoh's advanced pub/sub parks a
publisher-detection liveliness token at `<key>/@adv/pub/<zid>/<eid>/…` and parses
it with `${remaining:**}/@adv/…` — and since `**` cannot cross an `@`,
`remaining` could not span a key containing `@v1`. **Every** token was
unparseable by the only code that reads them: `detect_late_publishers()` was
silently dead, and every subscriber logged *"malformed liveliness token key
expression"* once per publisher. No upstream fix was possible — the
`@`-exclusion is a Zenoh matching rule.

The `@` bought invisibility to an **un-versioned** selector, i.e. coexistence
with the pre-v1 keyspace — a migration property, and the migration is done.
Cross-major isolation (a `v1` selector never matches a `v2` key) never needed
it: they are different literal chunks.

**Consequence:** `zensight/**` now *does* match v1 keys. `cutover_e2e` and
`v1_probe` therefore check that nothing appears **outside** `zensight/v1/`,
rather than relying on key algebra to hide us. Pinned by
`zensight-keyspace/tests/adv_token.rs` (the token must parse),
`guard.rs::d1_version_isolation`, and
`zensight-sensor-core/tests/adv_publisher_detection.rs` (no warning, end to end).
RFC: [03 §1.2](rfcs/keyspace-v2/03-grammar.md), [12 §7](rfcs/keyspace-v2/12-open-questions.md).

## The base is the session namespace, not a chunk anyone types

Application code **never spells `zensight`**. The base is set once, as the Zenoh
session `namespace` (`zenoh.namespace`, default `zensight`, override
`ZENSIGHT_ZENOH_NAMESPACE`), and the runtime prefixes it onto every keyexpr the
session emits, strips it on delivery, and **filters ingress from outside it**
(RFC 03 §1.1, 09 §0 — issue #466).

So there are two views of every key, and which one you are in is a property of
the *session*, not the key:

| | sees | builds keys with |
|---|---|---|
| **applications** (sensors, GUI, correlator, exporters) — namespaced | `v1/h-…/telemetry/sysinfo/cpu/usage` | `V1Context`, `zensight_common::keyexpr` — all base-relative |
| **routers / storages / ACL** — no namespace | `zensight/v1/h-…/…` | full keys, written by hand in `configs/router-*.json5` |
| **debug tools** (`zenctl`, `v1_probe`) — un-namespaced *on purpose* (RFC 09 §5) | `zensight/v1/h-…/…` | `grammar::with_base(base, …)`, `keyexpr::parse_full_key(base, …)` |

The middle and bottom rows are why `with_base`/`strip_base` exist and why
`zenctl` takes a `--base`: an explorer that ran inside the namespace could not
see a key from *outside* it, and spotting exactly that is what an explorer is
for.

Two CI guards keep this true: application source may not contain a `"zensight/`
literal at all, and only `zensight_common::session` may call `zenoh::open` —
because the namespace is per-session, and a component that hand-rolls its own
`zenoh::Config` would silently publish at the bus root and go deaf with no error.
(There were five such builders before #466. There is one now.)

The wire is unchanged by all of this: `zensight-sensor-core/tests/cutover_e2e.rs`
pins it with a namespaced sensor and an **un-namespaced** observer, so the same
key is asserted in both spellings at once.

## Operations

Isolated verification: `cargo run -p zensight-common --example v1_probe` opens
a multicast-scouting-off listener, watches the bus, exercises the @rpc plane,
and fails if the retired legacy bus (`zensight/**`) carries anything. Point
sensors at it with `ZENSIGHT_ZENOH_CONNECT=tcp/127.0.0.1:17471
ZENSIGHT_ZENOH_SCOUTING=false` (the `zenoh.scouting` config knob / env
override disables multicast discovery so a session can never join a mesh
beyond its explicit endpoints; gossip stays on — it only propagates within
the connected graph).

Session config, storage recipes (latest/catalog/timeseries/pdns), ACL, and
constrained-link profiles: RFC [09](rfcs/keyspace-v2/09-operations.md).
Shipped router configs: [`configs/router-evidence-storage.json5`](../configs/router-evidence-storage.json5)
(state seed store), [`configs/router-blob-storage.json5`](../configs/router-blob-storage.json5)
(@blob tiers), [`configs/router-pdns-influxdb-storage.json5`](../configs/router-pdns-influxdb-storage.json5)
(pdns history).
