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
zensight/@v1/<origin>/<class>/<producer>/<subject...>     data planes
zensight/@v1/<origin>/@rpc/<producer>/<procedure...>      request/reply
zensight/@v1/<origin>/@media/<producer>/<stream>/…        opaque video
zensight/@v1/<origin>/@blob/{artifact,tree,store}/…       bulk content
zensight/@v1/@catalog/…                                   the identity catalog
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
  `zensight/@v1/*/@rpc/…` with query target `All`.
- Late joiners seed with a plain GET on the same state selectors (state is
  its own seed; storage-shaped queryables answer one reply per concrete key).

## Where the machine-readable truth lives

- **Registry** (per-producer subjects/procedures, QoS entitlements, lints):
  [`zensight-keyspace/registry/*.toml`](../zensight-keyspace/registry/) —
  compiled by `zensight-keyspace`'s `build.rs` into typed builders/parsers;
  registry violations are build errors. Sensors serve their compiled slice at
  `…/@rpc/<producer>/introspect`.
- **Key builders**: `zensight-keyspace::V1Context` (producer-side),
  `zensight_common::keyexpr` (consumer-side selectors + fleet/origin RPC
  keys), `zensight_common::command` (topic/artifact procedure keys). New code
  MUST build keys through these — never ad-hoc `format!`.
- **Guard tests**: `zensight-keyspace/tests/guard.rs` pins the D1–D6
  disjointness algebra; consumer crates pin their own selector shapes.

## Operations

Session config, storage recipes (latest/catalog/timeseries/pdns), ACL, and
constrained-link profiles: RFC [09](rfcs/keyspace-v2/09-operations.md).
Shipped router configs: [`configs/router-evidence-storage.json5`](../configs/router-evidence-storage.json5)
(state seed store), [`configs/router-blob-storage.json5`](../configs/router-blob-storage.json5)
(@blob tiers), [`configs/router-pdns-influxdb-storage.json5`](../configs/router-pdns-influxdb-storage.json5)
(pdns history).
