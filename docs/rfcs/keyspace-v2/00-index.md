# Zenoh Semantic Convention RFC — Index

**Status: Draft v1.2** (round 1: adversarial review + Zenoh 1.9 source
verification + D-Bus/Homie/OPC-UA research; round 2: base = session
namespace, delivery mechanics via zenoh-ext advanced pub/sub, storage
guidance — July 2026) ·
supersedes the exploratory drafts in `zensight-key-semantic/` (credited in
[03 §6.2](03-grammar.md)) · does **not** replace
[`docs/KEYSPACE.md`](../../KEYSPACE.md), which remains authoritative for
the shipped keyspace.

A key-space convention for Zenoh applications: how to shape key
expressions so that routing, subscriptions, storage selection, access
control, and bandwidth policy all fall out of the grammar instead of being
re-implemented per consumer. Written application-neutrally; **ZenSight** is
the reference application and supplies the worked examples.

---

## The convention on one page

```
<base>/@v1/<origin>/<class>/<producer>/<subject...>
```

| Position | Chunk | Example |
|---|---|---|
| 1 | **base** — deployment root = the session **namespace** (config; tenancy = deployment prefix; app code never spells it) | `zensight` |
| 2 | **version** — verbatim `@v<int>`; majors are mutually invisible by key algebra | `@v1` |
| 3 | **origin** — who publishes: self-minted stable host id, or verbatim service | `h-3fa9c2d41b7e` · `@catalog` |
| 4 | **class** — bus semantics: `telemetry` (superseded) · `state` (LWW+tombstone) · `events` (immutable) · verbatim planes `@rpc` · `@media` · `@blob` | `state` |
| 5 | **producer** — the component that produced it (`name[-instance]`; omitted under service origins) | `netlink` |
| 6+ | **subject** — open-ended, registry-governed meaning path | `alert/9f2c81ab04d7e3f1` |

Normative examples (base = `zensight`):

```
zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
zensight/@v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
zensight/@v1/h-3fa9c2d41b7e/state/netring/health
zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1
zensight/@v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7
zensight/@v1/h-3fa9c2d41b7e/events/netring/capture/01jgxqz4yqk8v6txw3m9f2a7cd
zensight/@v1/h-3fa9c2d41b7e/@rpc/netlink/sockets
zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main
zensight/@v1/h-3fa9c2d41b7e/@blob/store/sha256/ab12cd34ef56
zensight/@v1/@catalog/state/entity/h-3fa9c2d41b7e
zensight/@v1/@catalog/state/pdns/93-184-216-34
```

The canonical selectors, and the properties that make them safe, are in
[03 §4–5](03-grammar.md); the headline property: a per-host subscription
`zensight/@v1/h-xxx/**` delivers that host's complete data plane and can
never pull keys under `@rpc`/`@media`/`@blob` — by key algebra; that
frames and bulk actually live there is the registry's placement rule
(the theorem/precondition split of [03 §4](03-grammar.md)).

## Glossary

| Term | Meaning |
|---|---|
| **base** | the deployment's root chunk(s); everything the convention defines lives under it |
| **origin** | the publishing identity in every key — a host id or a named service |
| **class** | the update semantics of a subtree: telemetry / state / events |
| **plane** | a verbatim-isolated subtree no data wildcard can reach: `@rpc`, `@media`, `@blob` (the version chunk uses the same verbatim mechanism but is not a plane) |
| **producer** | the component (sensor/agent/service) that emits the data |
| **subject** | the registry-governed meaning path — the open part of the key |
| **catalog** | the singleton service that fuses identity evidence into entities; the only author of identity *conclusions* |
| **registry** | the machine-readable inventory binding every subject/procedure to a payload type, QoS, and lifecycle |
| **sidecar** | the `@adv` machinery keys zenoh-ext parks under a data key (`<key>/@adv/…`: publisher cache, liveliness, heartbeat) — verbatim-isolated, ACL-relevant, never application-published |

## Reading order

Chapters are numbered for reference, not reading. Suggested paths:

- **Evaluating the design** (reviewers): 01 → 03 → 04 → 05 → 06 → 12,
  with 10 for the influences and 03 §6 for the roads not taken.
- **Adopting the convention** (other Zenoh apps): 02 → 03 → 04 → 08 → 09,
  then 11 §4 for the replace-this checklist.
- **Operating a deployment**: 09, with 04 for the class semantics behind
  the recipes.

## Chapters

| # | File | What it holds |
|---|---|---|
| 00 | this file | grammar-on-a-page, glossary, reading order |
| 01 | [01-motivation.md](01-motivation.md) | the shipped keyspace, its eight structural pain points, goals and non-goals |
| 02 | [02-principles.md](02-principles.md) | the eleven design principles, each with provenance |
| 03 | [03-grammar.md](03-grammar.md) | **normative core**: conformance model, chunk-by-chunk grammar, lexical rules, reserved tokens, design properties D1–D6 (theorems + preconditions), alternatives considered |
| 04 | [04-planes.md](04-planes.md) | class semantics (telemetry/state/events), placement rules, QoS profiles, delivery mechanics (advanced pub/sub per class), storage mapping, liveliness |
| 05 | [05-control-rpc.md](05-control-rpc.md) | the `@rpc` plane: targeting, read/write/long-running idioms, mapping of every incumbent control channel |
| 06 | [06-identity.md](06-identity.md) | origin minting, observed devices, evidence, the `@catalog` contract |
| 07 | [07-bulk-planes.md](07-bulk-planes.md) | `@media` (live frames) and `@blob` (bulk/content-addressed transfer) |
| 08 | [08-registry.md](08-registry.md) | the subject registry: format, versioning policy, naming rules, ownership |
| 09 | [09-operations.md](09-operations.md) | cookbook: session/namespace config, selectors, storage (volumes, replication, GC), ACL recipes (rules/subjects/policies, per-plane), constrained-link policy |
| 10 | [10-prior-art.md](10-prior-art.md) | Keelson, uProtocol/automotive, rmw_zenoh, Sparkplug, OTel, NATS, Zenoh guidance, D-Bus, Homie, OPC UA — took/rejected per system |
| 11 | [11-zensight-profile.md](11-zensight-profile.md) | the reference application: profile constants, worked keys per sensor, full shipped-family mapping |
| 12 | [12-open-questions.md](12-open-questions.md) | the six genuinely open items, each with options and a default |

## Scope

**In scope**: the key grammar and its semantics; the class/plane system;
the RPC, identity, media, blob, and registry contracts; operational
recipes.

**Out of scope** (by decision, see [01 §5](01-motivation.md)): metric
renaming, multi-tenancy machinery, payload schema definitions, and —
deliberately — any migration plan. The verbatim `@v1` chunk guarantees the
shipped keyspace and this one can share a network indefinitely without
interference; when and how to walk across is a separate decision.
