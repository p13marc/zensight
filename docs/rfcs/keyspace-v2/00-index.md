# Zenoh Semantic Convention RFC — Index

**Status: v1.3 — RATIFIED** (v1.0 2026-07-12; adopted for ZenSight, migration
tracked in [#453](https://github.com/p13marc/zensight/issues/453) with the
enforcement crate `zensight-keyspace`).

> **v1.3 (2026-07-15) — `@media` tiers; wire-breaking payloads, key grammar
> unchanged** ([#494](https://github.com/p13marc/zensight/issues/494)). The
> `@media` video key's last chunk changes meaning from an undefined `<profile>`
> to a normative **`<tier>`** — a named bandwidth rung the publisher offers
> concurrently and the viewer subscribes to *exactly*. Three linked changes:
> **(1)** [07 §1](07-bulk-planes.md) defines `<tier>` and **revokes** the
> `…/video/<codec>/*` wildcard (amendment F′ licensed it against "the viewer
> cannot know this chunk"; the catalogue now publishes the tiers, so the viewer
> *can* — and the `*` would match every tier at once, breaking simulcast).
> **(2)** [08 §2](08-registry.md) gives `[[media]]` a real contract — its own
> normative field table, `attachment` CI-resolved against the shared type
> table, `cardinality` required on `{var}` media paths, and generated key
> builders. **(3)** The stream-control payloads (`StreamControl`,
> `StreamStatus`, `StreamDescriptor`) move to a tier-oriented, capability-bearing
> shape. The **key grammar is unchanged** — position count and version chunk
> stay `v1`; only one chunk's *meaning* and the control payloads move — but the
> payload change is wire-breaking, and backward compatibility was explicitly not
> a constraint for this release.

> **v1.2 (2026-07-14) — six amendments, all additive, no wire change**
> ([#467](https://github.com/p13marc/zensight/issues/467)). Every one is a
> lesson from actually migrating a real application onto v1.0/v1.1 — and each
> was fact-checked against the ratified chapter text before it was kept. Two
> proposed amendments were **dropped** because the RFC already said it (see
> "what did *not* change", below); that is the more useful half of the record.
>
> | | Chapter | What |
> |---|---|---|
> | **A** | [06 §6](06-identity.md) *(new)* | **The consumer identity bridge.** The payload `host_id` **is** the origin id; a consumer holding a *hostname* MUST resolve it to an origin before building an origin-scoped key. `host_id` appeared **nowhere** in 06, and §5.1 only ran origin → entity — never "I have a box, what key do I build?". A UI built on the missing half took every drill-down in the reference product down at once. **This is the amendment that would have prevented the outage.** |
> | **B** | [08 §1.1](08-registry.md) *(new)* | **The origin is an argument too.** The codegen contract is build/parse × **local/remote**, and the origin's kind SHOULD be a *type*, so "I built a key for my own host by accident" is a compile error rather than a timeout. Shipped as a bug three times. |
> | **C** | [08 §5, §6.1](08-registry.md) | **The registry MUST NOT lie.** registry ⊆ served is upgraded from "a finding" to a **MUST**, with the reverse-direction lint. The reference registry advertised **seven** surfaces no build served; `introspect` was shipping them to the fleet as truth. Also: the forward lint is *vacuous* wherever a producer registers a catch-all subject. |
> | **D** | [09 §0.1](09-operations.md) *(new)* | **Discovery and scouting.** `scout`/`gossip`/`multicast` had **zero hits across all 13 chapters**. Multicast and gossip are *independent* switches; isolated verification is multicast **off**, gossip **on**; a gossip-less hub silently breaks spoke→spoke discovery. |
> | **F′** | [07 §1, §3](07-bulk-planes.md) *(new §3)* | **The wildcard rule.** *A publisher MUST always use its concrete origin; a subscriber MAY wildcard a chunk it cannot know* — and on the bulk planes, not even then. §1 licensed `*` for the *profile* chunk; nothing forbade wildcarding the **origin** on `@media`, which subscribes to every host's stream of that name. |
> | **G** | [09 §6](09-operations.md) *(new)* | **Cutover acceptance.** A cutover is not done until the retired family is provably **silent** *and* a **consumer-shaped, concrete-key** probe passes. A `*`-origin probe cannot catch a broken origin path — the reference smoke was green while the product was entirely broken. |
>
> **What did *not* change, deliberately.** [05 §2.1](05-control-rpc.md) (fan-in
> call discipline) was proposed for amendment and **left alone**: it already
> mandated query target `All` and replying on the producer's own concrete key,
> as bolded MUSTs, with the right reasoning. Both were hit as real bugs — not
> because the chapter was silent, but because it had not been read. An
> editorial note now says so in place, so nobody "fixes" a section that was
> right. Likewise a proposed `@blob` wildcard amendment was dropped: 07 §2
> already said the **opposite** of what was proposed, and correctly.
>
> Also fixed: 07 §1 cited `05 §5` twice for the stream-control RPC idiom; the
> normative home is **05 §3**.

> **v1.1 (2026-07-14) — one amendment.** The version chunk is a **plain** `v1`,
> not the verbatim `@v1` of v1.0. Verbatim made zenoh-ext's `@adv`
> publisher-detection tokens structurally unparseable (`**` cannot cross an `@`),
> silently killing late-publisher detection; the invisibility it bought was a
> *migration* property we no longer need, while cross-major isolation never
> depended on it. Wire-breaking. See [03 §1.2](03-grammar.md) and
> [12 §7](12-open-questions.md).

Drafting history: round 1
adversarial review + Zenoh 1.9 source verification + D-Bus/Homie/OPC-UA
research; round 2 base = session namespace + storage guidance; round 3
delivery re-grounded (stable baseline default, advanced pub/sub a priced
opt-in tier); round 4 all open questions decided
([12-open-questions.md](12-open-questions.md) is the decision record). ·
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
<base>/v1/<origin>/<class>/<producer>/<subject...>
```

| Position | Chunk | Example |
|---|---|---|
| 1 | **base** — deployment root (config; tenancy = deployment prefix; normally the session **namespace**, so app code never spells it) | `zensight` |
| 2 | **version** — plain `v<int>`; majors are mutually invisible by key algebra | `v1` |
| 3 | **origin** — who publishes: self-minted stable host id, or verbatim service | `h-3fa9c2d41b7e` · `@catalog` |
| 4 | **class** — bus semantics: `telemetry` (superseded) · `state` (LWW+tombstone) · `events` (immutable) · verbatim planes `@rpc` · `@media` · `@blob` | `state` |
| 5 | **producer** — the component that produced it (`name[-instance]`; omitted under service origins) | `netlink` |
| 6+ | **subject** — open-ended, registry-governed meaning path | `alert/9f2c81ab04d7e3f1` |

Normative examples (base = `zensight`):

```
zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
zensight/v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
zensight/v1/h-3fa9c2d41b7e/state/netring/health
zensight/v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1
zensight/v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7
zensight/v1/h-3fa9c2d41b7e/events/netring/capture/01jgxqz4yqk8v6txw3m9f2a7cd
zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets
zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high
zensight/v1/h-3fa9c2d41b7e/@blob/store/sha256/ab12cd34ef56
zensight/v1/@catalog/state/entity/h-3fa9c2d41b7e
zensight/v1/@catalog/state/pdns/93-184-216-34
```

The canonical selectors, and the properties that make them safe, are in
[03 §4–5](03-grammar.md); the headline property: a per-host subscription
`zensight/v1/h-xxx/**` delivers that host's complete data plane and can
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
| **sidecar** | the `@adv` machinery keys zenoh-ext parks under a data key (`<key>/@adv/…`: publisher cache, liveliness, heartbeat) — advanced-tier-only, verbatim-isolated, ACL-relevant, never application-published, never a presence roster |

## Reading order

Chapters are numbered for reference, not reading. Suggested paths:

- **Evaluating the design** (reviewers): 01 → 03 → 04 → 05 → 06 → 12,
  with 10 for the influences and 03 §6 for the roads not taken.
- **Adopting the convention** (other Zenoh apps): 02 → 03 → 04 (delivery
  contracts §3.1–3.4 especially) → 08 → 09, then 11 §4 for the
  replace-this checklist.
- **Operating a deployment**: 09, with 04 for the class semantics behind
  the recipes.

## Chapters

| # | File | What it holds |
|---|---|---|
| 00 | this file | grammar-on-a-page, glossary, reading order |
| 01 | [01-motivation.md](01-motivation.md) | the shipped keyspace, its eight structural pain points, goals and non-goals |
| 02 | [02-principles.md](02-principles.md) | the eleven design principles, each with provenance |
| 03 | [03-grammar.md](03-grammar.md) | **normative core**: conformance model, chunk-by-chunk grammar, lexical rules, reserved tokens, design properties D1–D6 (theorems + preconditions), alternatives considered |
| 04 | [04-planes.md](04-planes.md) | class semantics (telemetry/state/events), placement rules, QoS profiles, delivery contracts + baseline + opt-in advanced tier, storage mapping, liveliness |
| 05 | [05-control-rpc.md](05-control-rpc.md) | the `@rpc` plane: targeting, read/write/long-running idioms, mapping of every incumbent control channel |
| 06 | [06-identity.md](06-identity.md) | origin minting, observed devices, evidence, the `@catalog` contract |
| 07 | [07-bulk-planes.md](07-bulk-planes.md) | `@media` (live frames) and `@blob` (bulk/content-addressed transfer) |
| 08 | [08-registry.md](08-registry.md) | the subject registry: format, versioning policy, naming rules, ownership |
| 09 | [09-operations.md](09-operations.md) | cookbook: session/namespace config, selectors, storage (volumes, replication, GC), ACL recipes (rules/subjects/policies, per-plane), constrained-link policy |
| 10 | [10-prior-art.md](10-prior-art.md) | Keelson, uProtocol/automotive, rmw_zenoh, Sparkplug, OTel, NATS, Zenoh guidance, D-Bus, Homie, OPC UA — took/rejected per system |
| 11 | [11-zensight-profile.md](11-zensight-profile.md) | the reference application: profile constants, worked keys per sensor, full shipped-family mapping |
| 12 | [12-open-questions.md](12-open-questions.md) | the decision record: all six former open questions decided, each with its alternatives and revisit trigger |

## Scope

**In scope**: the key grammar and its semantics; the class/plane system;
the RPC, identity, media, blob, and registry contracts; operational
recipes.

**Out of scope** (by decision, see [01 §5](01-motivation.md)): metric
renaming, multi-tenancy machinery, payload schema definitions, and —
deliberately — any migration plan. Convention majors are mutually invisible by
key algebra (`v1` and `v2` are different literal chunks), so two majors can share
a network indefinitely; when and how to walk across is a separate decision.
(In v1.0 the version chunk was verbatim `@v1`, which additionally hid v1 from an
*un-versioned* selector. It no longer is — see [03 §1.2](03-grammar.md).)
