# 03 — Canonical Grammar

**Status: v1.0 (ratified)** · normative chapter

This chapter defines the canonical key grammar of the convention. Everything
else in this RFC (planes, RPC, identity, registry) hangs off this shape.

## 0. Conformance

MUST/SHOULD/MAY are used per RFC 2119 **throughout this RFC**, not only in
this chapter. Normative statements bind one of four roles, named explicitly
or clear from context:

- a **publisher** (a producer process emitting keys/payloads),
- a **consumer** (anything subscribing or querying),
- a **deployment** (the operator: router config, storage, ACL, enrollment),
- a **registry** (the subject-registry maintainers and their CI).

Requirements are of three kinds, and chapters say which where it is not
obvious: *statically checkable* (CI against the registry and key constants),
*runtime checkable* (guard tests, bus observation), and *attestations*
(unobservable by a checker — e.g. "an events key is written exactly once";
the publisher attests by construction and review).

### 0.1 API-stability posture

The convention is layered against Zenoh's stability tiers, and every
mechanism is marked at first mention:

- The **grammar, planes, and control idioms** rest exclusively on stable
  API and config: key expressions, declared publishers/queryables,
  liveliness (tokens, queries, history subscribers), matching listeners,
  `Querier`, query target/consolidation/payload/attachment, `reply_err`,
  session `namespace`, ACL/interceptor/storage configuration. A minimal
  conforming participant needs nothing unstable.
- The **delivery baseline** ([04-planes.md §3.2](04-planes.md)) — plain
  declared publishers, per-producer liveliness, refresh/TTL staleness,
  storage-backed seeding — is also stable-only, and it is the *default*:
  it meets every delivery entitlement the registry grants by default.
- The **advanced tier** ([04-planes.md §3.3](04-planes.md)) — per-key
  publisher caches, history seeding, sample-miss detection and recovery —
  uses zenoh-ext's `AdvancedPublisher`/`AdvancedSubscriber` and Zenoh's
  `SourceInfo`, which are **unstable** (building with `zenoh-ext/unstable`
  pulls zenoh's `unstable` + `internal`). The tier is **opt-in per
  subject**, priced in 04's cost box, and chosen only where a subject's
  entitlements exceed the baseline (fast miss detection, storage-less
  seeding). It never changes a key shape: if the unstable surface moves,
  the keys and the registry do not.

---

## 1. Canonical form

```
<base>/v1/<origin>/<class>/<producer>/<subject...>
```

Six positions. Positions 2–5 are exactly one chunk each; `<base>` is one or
more literal chunks, fixed per deployment and known to every participant by
configuration; the subject is an open-ended path of one or more chunks.
Example (reference application):

```
zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
└──┬───┘ └┬┘ └─────┬──────┘ └───┬───┘ └──┬──┘ └───┬───┘
  base  version  origin       class   producer  subject
```

| Position | Name | Arity | Rule |
|---|---|---|---|
| 1 | `<base>` | ≥ 1 chunk (config-fixed) | Deployment root. Configurable per deployment; MUST be a fixed run of literal, non-verbatim chunks. |
| 2 | `v1` | 1 chunk | Convention major version. MUST be a **plain** chunk of the form `v<integer>` (§1.2 — it was verbatim in v1.0; the amendment explains why it is not). |
| 3 | `<origin>` | 1 chunk | Who publishes: a **host origin** (stable opaque id) or a **service origin** (verbatim `@<service>`). |
| 4 | `<class>` | 1 chunk | Message class: `telemetry` \| `state` \| `events`, or a verbatim plane `@rpc` \| `@media` \| `@blob`. |
| 5 | `<producer>` | 1 chunk | The component on the origin that produced the data (sensor/agent), optionally instance-suffixed. Omitted under service origins. |
| 6+ | `<subject...>` | ≥ 1 chunk | Registry-governed meaning path. The only open-ended part of the key. |

### 1.1 `<base>` — deployment root = Zenoh namespace

- The base chunk names the *deployment*, not the software: two independent
  installations on one Zenoh network MUST use different bases (or a shared
  base behind different routers/ACLs).
- Multi-tenancy, realms, and sites are **deployment prefixes**, not grammar
  chunks: `acme/fleet-a` is a valid `<base>` (two chunks) as long as it is a
  fixed literal run. Nothing in the convention ever inspects the base; every
  selector in this RFC is written relative to it. Positional tooling
  ("origin is chunk 3") MUST therefore resolve positions relative to the
  configured base, never by absolute index.
- **The base is exactly Zenoh's session `namespace`** (stable config,
  `namespace: "<base>"`), and setting it there is the RECOMMENDED
  implementation: the runtime transparently prepends it to every keyexpr
  the session emits (publications, subscriptions, queries, queryables,
  liveliness tokens, `@adv` sidecars), strips it on delivery, and
  *filters* non-matching ingress — application code and the registry never
  spell the base, and the base is an isolation boundary, not just a
  prefix ([12-open-questions.md §1](12-open-questions.md)). Lexical
  constraints: any **non-wild** keyexpr (multi-chunk bases work; wildcards
  rejected); verbatim chunks are type-permitted but a base MUST NOT
  contain them. Operational consequences — router-side configs still see
  full keys; router admin needs an un-namespaced session — live in the
  cookbook ([09-operations.md §0, §5](09-operations.md)).
- Rationale: an isolation token you rarely use should not cost every key a
  chunk — and with the namespace mechanism it costs no *code* either.
  Deployments that need it prepend it; deployments that don't, don't pay.
  (See NATS guidance: the first token is the isolation key —
  [10-prior-art.md §6](10-prior-art.md).)

### 1.2 `v1` — version chunk

- MUST be a **plain** chunk of the form `v<integer>` — *not* verbatim. Two
  convention majors are different literal chunks, so keys of one major are
  **invisible to a selector written against another**: a `<base>/v1/**`
  subscriber can never match a `<base>/v2/…` key, and vice versa. That is
  **version isolation**, and it is the property the convention needs.
- This makes coexistence of two convention majors a *property of the grammar*:
  both can share a network indefinitely without a bridge, and a consumer opts
  into exactly one (or explicitly both).

> **Amendment (v1.1) — the version chunk was verbatim (`@v1`) in v1.0.**
>
> Verbatim bought one thing more: invisibility to an **un-versioned** selector.
> Because `*`/`**` never cross an `@`, a legacy `<base>/**` firehose could not
> see v1 keys at all — "mutually invisible by key algebra". That was aimed at
> coexistence with a *pre-convention* keyspace during a migration.
>
> It cost more than it bought. Zenoh's advanced pub/sub (zenoh-ext) parks a
> publisher-detection liveliness token at `<key>/@adv/pub/<zid>/<eid>/…` and
> parses it back with `${remaining:**}/@adv/…`. Since `**` cannot cross an `@`,
> `remaining` could not span a key containing `@v1` — so **every** such token was
> unparseable by the only code that reads them. Late-publisher detection was
> silently dead and every subscriber logged *"malformed liveliness token key
> expression"* once per publisher, forever. No upstream fix is possible: the
> `@`-exclusion is a Zenoh *matching* rule, so no keformat can capture a key
> containing a verbatim chunk.
>
> The trade is sound because the un-versioned-selector case is a **migration**
> concern, not a steady-state one: it protects a pre-convention consumer, and
> once the pre-convention keyspace is retired there is none. Cross-major
> isolation — the property that keeps working forever — never depended on the
> `@` at all.
>
> The chunks that remain verbatim are the ones whose exclusion does daily work:
> the planes (`@rpc`/`@media`/`@blob`, §4 D2) and service origins (`@catalog`,
> §4 D4). No advanced publisher ever publishes on those.
>
> **Cost, stated plainly:** an un-versioned `<base>/**` selector now matches v1
> keys. A deployment migrating from a pre-convention keyspace must therefore
> keep the two apart by *base*, not by key algebra.
- The major is bumped **only** when the grammar or the semantics of a
  position change incompatibly. Additive evolution (new subjects, new
  producers, new procedures) is a **registry** change and never bumps the
  chunk ([08-registry.md](08-registry.md)).
- Everything the convention defines lives **under** the version chunk.
  Nothing — no status key, no discovery key, no side channel — may sit
  beside it. (Sparkplug placed its STATE topic outside `spBv1.0/` and needed
  a breaking release to fix it; see [10-prior-art.md §4](10-prior-art.md).
  Homie learned the same lesson from the other side: through v4 the
  convention major was an attribute *value* (`$homie`), so majors shared one
  topic space; v5 had to insert the major into the path — `homie/5/…` — to
  make coexistence possible. See [10-prior-art.md §9](10-prior-art.md).)

### 1.3 `<origin>` — publishing identity

Two forms:

**Host origin** — `h-<12hex>`: a stable, opaque, self-minted identifier of
the publishing machine.

- MUST match `h-[0-9a-f]{12}` exactly. Tooling MAY rely on this shape to
  distinguish host origins from service origins.
- MUST be derivable by the publisher alone, at first startup, with no
  network round-trip (no registration service, no coordinator).
- Reference derivation: `h-` + first 12 hex chars of
  `sha256(machine-id + application salt)` — byte-precise definition and a
  test vector in [06-identity.md §1](06-identity.md). The raw machine-id
  never leaves the host. All publishers on one machine derive the same
  value.
- MUST be stable across restarts and upgrades. MUST NOT encode a mutable
  name (hostname, DNS, IP). Human names belong to the catalog
  ([06-identity.md](06-identity.md)).
- 12 hex = 48 bits. Truncation is a deliberate trade
  (short keys, negligible birthday risk at fleet scale: ≈ 1.8 × 10⁻⁵ at
  100 k hosts); a collision is *detected*, not prevented — the catalog
  raises a conflict when disjoint `evidence/self` claims share one origin
  ([06-identity.md §1](06-identity.md)).
- Fallback rules for hosts without a machine-id are defined in
  [06-identity.md §1.1](06-identity.md).

**Service origin** — `@<service>`: a verbatim chunk naming a singleton,
deployment-level service (not tied to one host), e.g. `@catalog` for the
identity correlator.

- Verbatim on purpose: `*` does not match a verbatim chunk, so fleet-wide
  selectors like `<base>/v1/*/state/**` structurally exclude service
  output. Consumers of a service subscribe to it **by name** — a deliberate
  visibility boundary, since service output (merged entities, historical
  records) has different trust, cardinality, and storage properties than
  host data.
- Service origins MUST be registered ([08-registry.md](08-registry.md)).
  This RFC reserves `@catalog`.

### 1.4 `<class>` — message class

The class chunk declares the **bus semantics** of everything below it. Three
data classes (plain chunks — they participate in wildcards):

| Class | Semantics | One line |
|---|---|---|
| `telemetry` | superseded time-series | a missed sample is replaced by the next one |
| `state` | last-writer-wins + delete-tombstones | the latest value *is* the truth; deletion is meaningful |
| `events` | immutable, unique-keyed occurrences | never updated, never superseded |

and three verbatim planes (hermetic — no `*`/`**` reaches them):

| Plane | Mechanism | One line |
|---|---|---|
| `@rpc` | queryables | request/reply; all interaction is pull ([05-control-rpc.md](05-control-rpc.md)) |
| `@media` | plain pub | opaque high-rate frames, never on any firehose ([07-bulk-planes.md](07-bulk-planes.md)) |
| `@blob` | queryables | bulk/content-addressed transfer ([07-bulk-planes.md](07-bulk-planes.md)) |

Full class semantics, placement rules ("is an alert state or an event?"),
QoS, and storage mappings: [04-planes.md](04-planes.md).

Why class is a key position at all: it is the one attribute that *every*
infrastructure concern selects on — storage backends (latest-value vs
time-series), QoS defaults, ACLs, bandwidth allowlists. Sparkplug proved
message-type-in-topic works at industrial scale; this convention keeps the
idea but moves it *inside* the versioned namespace and gives the bulk planes
verbatim hermeticity ([10-prior-art.md §4](10-prior-art.md)).

### 1.5 `<producer>` — producing component

- One chunk: `<name>` or `<name>-<instance>` when several instances of the
  same producer run on one origin (`snmp` / `snmp-2`). To keep the chunk
  parseable back into (name, instance): the instance suffix MUST be
  `-<positive decimal integer>`, and producer base names MUST NOT end in
  `-<integer>` (registry-checked, [08-registry.md §5](08-registry.md)).
  Registry entries are keyed by the base name; consumers strip a trailing
  `-<int>` to find the entry.
- Instance numbers are assigned by local configuration: each instance MUST
  be configured with a distinct suffix (the first/only instance uses the
  bare name). There is no coordinator; a producer SHOULD probe the
  liveliness key of its intended producer chunk at startup and refuse to
  start over a live twin. The suffix rule is the collision rule: two
  publishers MUST NOT share `<origin>/<class>/<producer>` ownership of a
  subject.
- The producer chunk is **before** the subject, not after it. A trailing
  producer (Keelson's `source_id`) is only parseable when the subject has
  fixed depth; this convention's subjects are open-depth (gNMI paths,
  directory-like metrics), so a trailing chunk would be ambiguous —
  no parser could tell where the subject ends and the producer begins.
- Under service origins the producer position is **omitted** (the service
  *is* the producer): `<base>/v1/@<service>/<class>/<subject...>`. A parser
  disambiguates by the origin chunk alone: verbatim origin ⇒ chunk 5 (after
  the class) is already subject; host origin ⇒ chunk 5 is the producer.
- Under `@blob` the producer position is replaced by a reserved **tier
  token** — `artifact` | `tree` | `store` ([07-bulk-planes.md §2](07-bulk-planes.md)).
  Content-addressed data has no meaningful owning component (any producer's
  RPC can mint an artifact id; a hash is a hash); the tier tokens are listed
  in §3 and MUST NOT be used as producer names.

### 1.6 `<subject...>` — meaning path

- Open-ended, ≥ 1 chunk. The subject is the only part of the key whose
  vocabulary is not fixed by this grammar; it is governed by the subject
  registry ([08-registry.md](08-registry.md)).
- Proxy producers (a sensor observing *other* devices: SNMP, Modbus, gNMI,
  NetFlow) MUST put the observed device as the **first subject chunk**:
  `…/telemetry/snmp/router01/system/sys_uptime`. The observed device is
  subject matter, not origin — the origin is always the machine that runs
  the publisher ([06-identity.md §3](06-identity.md)).
- Every distinct subject (modulo its documented variable chunks) MUST map to
  exactly one payload type, so any wildcard selector yields a decodable,
  homogeneous result set (Zenoh guidance: one datatype per wildcard result;
  see [02-principles.md P5](02-principles.md)).

---

## 2. Chunk lexical rules

Zenoh legality is the outer bound: chunks are non-empty UTF-8 excluding
`* $ ? #`, keys have no leading/trailing `/` and no empty chunk (`//`).
This convention narrows it:

- Non-verbatim chunks MUST match `[a-z0-9]([a-z0-9._-]*[a-z0-9])?` —
  lowercase ASCII letters, digits, `.`, `_`, `-`; must start and end
  alphanumeric. No uppercase (case-sensitivity footguns), no `%`-escaping.
  Identifiers whose canonical text form is uppercase MUST be lowercased at
  key-build time — in particular **ULIDs are key-encoded in lowercase**
  (Crockford base32 decodes case-insensitively; payloads MAY carry the
  canonical uppercase form).
- Verbatim chunks MUST match `@[a-z0-9][a-z0-9_-]*` (plus the `@v<int>`
  version form).
- Values that contain characters outside this charset MUST be **slugged**
  before entering a key, and the original value MUST travel in the payload.
  Slugging MUST be canonical and injective within each documented variable
  domain — two spellings of one value MUST slug identically, and two
  distinct values MUST NOT slug to one chunk. Reference slugs:
  - IP address: **always slugged**, even though dotted IPv4 is
    charset-legal (dotted forms are non-canonical key chunks). IPv6 MUST
    first be canonicalized per RFC 5952 and IPv4 to minimal dotted-quad;
    then `.`/`:` → `-` (`10.0.0.7` → `10-0-0-7`, `2001:db8::1` →
    `2001-db8--1`).
  - systemd unit / filename: literal if already legal *per this section's
    charset* (not merely Zenoh-legal), else each excluded character is
    escaped losslessly as `_xNN_` (lowercase hex of the byte) — plain `-`
    substitution is forbidden because it is not injective (`foo@1.service`
    and `foo-1.service` must not share a key).
- Wildcards (`*`, `**`) and the sub-chunk wildcard `$*` are **selector**
  syntax and MUST NOT appear in published keys. Published keys and selectors
  MUST be in Zenoh canon form.
- `$*` MUST NOT be used in selectors either. If you feel the need for
  `if-eth0-rx$*`, the key is wrong: split the multi-valued chunk into
  chunks (`if/eth0/rx_bytes`). (Zenoh: `$*` is markedly slower and strains
  the infrastructure; see [02-principles.md P6](02-principles.md).)
- Per-message data (request ids, timestamps, sequence numbers) MUST NOT
  appear in any published key — unbounded-cardinality keys defeat interning,
  caches, and storage — with exactly four sanctioned exceptions: the unique
  id terminating an `events` key ([04-planes.md §1.3](04-planes.md)),
  content hashes under `@blob/store`, tree-index ids under `@blob/tree`
  (the root hash of the tree they index), and artifact ids under
  `@blob/artifact` ([07-bulk-planes.md §2](07-bulk-planes.md)).
- State subjects that key on an *observed population* (one chunk per
  observed IP, device, unit) are not per-message data but are also not free:
  they MUST carry an explicit cardinality budget in the registry
  ([04-planes.md §1.2](04-planes.md), [08-registry.md §2](08-registry.md)).

---

## 3. Reserved tokens

| Token | Position | Meaning |
|---|---|---|
| `@v<int>` | 2 | convention major version |
| `h-<12hex>` | 3 | host origin id (reference derivation §1.3) |
| `@catalog` | 3 | the identity/catalog service ([06-identity.md](06-identity.md)) |
| `telemetry`, `state`, `events` | 4 | data classes |
| `@rpc`, `@media`, `@blob` | 4 | verbatim planes |
| `artifact`, `tree`, `store` | 5 (under `@blob` only) | blob tiers ([07-bulk-planes.md](07-bulk-planes.md)); MUST NOT be producer names |
| `alive` | subject leaf under `state` | liveliness-token keys only ([04-planes.md §5](04-planes.md)); MUST NOT be registered as a data subject |

Applications MAY register further service origins and MUST NOT redefine the
tokens above. New class or plane tokens are a convention-major change.

---

## 4. Design properties (theorems and their preconditions)

Each property below separates a **theorem** — a claim that holds by key
algebra alone, pinned as a guard test in the reference implementation — from
its **precondition** — the placement or deployment rule that makes the
plain-prose reading true. The theorems cannot be violated by any publisher;
the preconditions are enforced by registry review, CI, and deployment
config, and the RFC is explicit about which is which.

**D1 — Version isolation.** *Theorem*: `<base>/v1/**` ∩ `<base>/v2/**` = ∅.
Two convention majors coexist with zero cross-talk: a selector written
against one can never match the other's keys. No precondition — this one is
pure algebra (they are different literal chunks).

> **Amended in v1.1.** D1 used to be the stronger *version hermeticity*:
> `<base>/**` ∩ `<base>/v1/**` = ∅ — v1 was invisible even to an
> **un-versioned** selector, because the version chunk was verbatim (`@v1`)
> and `**` does not cross an `@`. That property protected coexistence with a
> *pre-convention* keyspace during a migration, and it cost the advanced tier
> its publisher detection (§1.2). The version chunk is now plain, so
> `<base>/**` **does** reach v1 keys. Cross-major isolation — the part that
> matters after the migration — is unaffected.

**D2 — Per-origin firehose is data-only.**
*Theorem*: `<base>/v1/h-xxx/**` matches every key under the three data
classes of that host and **no** key under `@rpc`/`@media`/`@blob`.
*Precondition*: bulk and high-rate payloads actually live under the verbatim
planes — placement rule R4 ([04-planes.md §2](04-planes.md)), enforced by
registry review. A producer that publishes video frames at
`telemetry/cam0/frame` is lexically legal and would ride the firehose; the
algebra protects against *accident and selector mistakes*, the registry
protects against *misplacement*. With both, one subscription = one host's
complete data plane at bounded rate. This is the single most load-bearing
property of the design.

**D3 — Class disjointness.** *Theorem*: `<base>/v1/*/telemetry/**`,
`…/*/state/**`, `…/*/events/**` are pairwise non-intersecting — literal
class chunks differ. Storage and QoS policy select on them directly.

**D4 — Service exclusion.** *Theorem*: `<base>/v1/*/state/**` does not
match `<base>/v1/@catalog/state/**` (verbatim origin). Fleet selectors see
hosts only; catalog consumers subscribe by name. Corollary: this applies to
**every** keyexpr in a deployment, including ACL rules and storage
selectors — a rule written with `*` in the origin position never covers
`@catalog`, which needs its own rule ([09-operations.md §3](09-operations.md)).

**D5 — Targeted and fleet RPC from one key shape.**
*Theorem*: `GET <base>/v1/h-xxx/@rpc/netlink/sockets` reaches one host;
`GET <base>/v1/*/@rpc/netlink/sockets` intersects every host's queryable —
same key, `*` in the origin position (the `*` matches host origins but not
`@catalog`, by D4's mechanism). *Precondition*: collecting **all** replies
additionally requires the fan-in call discipline of
[05-control-rpc.md §2.1](05-control-rpc.md) — Zenoh's default query target
and consolidation can short-circuit to a single reply.

**D6 — Static policy prefixes.** *Theorem*: every security- or
storage-relevant boundary (deployment, version, origin, class) is a
fixed-position literal run from the left. So Zenoh storage `strip_prefix`
(which must be a literal prefix) can strip `<base>/v1` and select per
class, and a constrained link can be provisioned by prefix rules with no
per-key inspection. *Preconditions and limits*:
- **Per-principal ACL is a fixed set of prefix rules, one per plane — not
  one rule.** ACL matching is keyexpr *inclusion*, and `**` never crosses a
  verbatim chunk there either: `<base>/v1/h-xxx/**` does not cover the
  host's `@rpc` replies, `@media` frames, or `@blob` keys. "Host X may act
  only as itself" is expressed as ~4 literal-prefix rules
  (`…/h-xxx/**`, `…/h-xxx/@rpc/**`, `…/h-xxx/@media/**`, `…/h-xxx/@blob/**`)
  — still static, still literal, but plural ([09-operations.md §3](09-operations.md)).
- **Per-origin ACL requires enrollment.** Origins are self-minted (§1.3), so
  nothing in the grammar binds a transport identity to an origin id: absent
  a binding, any connected peer may publish (and tombstone!) any origin's
  keys while remaining grammar-conformant. A deployment relying on D6 for
  security MUST bind origins to transport identities — e.g. mTLS
  certificate CN = origin id, bound in ACL `subjects` — via an explicit
  enrollment step, and MUST accept that Zenoh ACL config is not
  runtime-reloadable (adding a host is a router config change). Without
  enrollment, D6 is a hygiene boundary, not a security boundary.

---

## 5. Normative example set

These keys are the reference examples used throughout the RFC (and by the
guard tests). Base = `zensight`.

```
zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
zensight/v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
zensight/v1/h-3fa9c2d41b7e/state/netring/health
zensight/v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1
zensight/v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7
zensight/v1/h-3fa9c2d41b7e/events/netring/capture/01jgxqz4yqk8v6txw3m9f2a7cd
zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets
zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main
zensight/v1/h-3fa9c2d41b7e/@blob/store/sha256/ab12cd34ef56
zensight/v1/@catalog/state/entity/h-3fa9c2d41b7e
zensight/v1/@catalog/state/pdns/93-184-216-34
```

And the canonical selectors:

| Need | Selector |
|---|---|
| everything about one host (data planes) | `zensight/v1/h-3fa9c2d41b7e/**` |
| all telemetry, fleet-wide | `zensight/v1/*/telemetry/**` |
| one protocol, fleet-wide | `zensight/v1/*/telemetry/snmp/**` |
| all state (→ latest-value storage) | `zensight/v1/*/state/**` |
| all alerts | `zensight/v1/*/state/*/alert/*` |
| one host's health | `zensight/v1/h-3fa9c2d41b7e/state/*/health` |
| fleet RPC fan-in | `zensight/v1/*/@rpc/netlink/sockets` (GET) |
| entity documents | `zensight/v1/@catalog/state/entity/*` |

---

## 6. Alternatives considered

### 6.1 Protocol-first (the incumbent shape)

`<base>/<protocol>/<source>/<metric>` — ZenSight's shipped keyspace
([01-motivation.md](01-motivation.md)). Rejected as the v1 canonical shape
because:

- the host cannot be a policy boundary: "host X may publish only its own
  keys" is inexpressible as a static ACL prefix when the host discriminator
  is a mutable, human-chosen name in position 3 of some families and absent
  in others;
- stable identity ends up payload-only, forcing every consumer through a
  correlation join before it can even group keys by machine;
- control channels scoped per protocol fan out to every host of the
  protocol, and host-targeting has to be retrofitted per channel.

The continuity is deliberate, though: the new shape is essentially the old
key with an origin prepended and a class inserted —
`<producer>` ≈ old `<protocol>`, proxy first-subject-chunk ≈ old
`<source>`, subject ≈ old `<metric>`. Protocol-centric consumers keep a
one-`*` selector (`…/*/telemetry/snmp/**`).

### 6.2 The entity-centric draft skeleton

The predecessor drafts (`zensight-key-semantic/`, ChatGPT-assisted) proposed:

```
zensight/v1/<realm>/assets/<asset>/entities/<kind>/<entity>/<state|telemetry>/<domain>/<component>/<producer>
```

Adopted from it — with credit: the versioned root, the state/telemetry
split, producer-in-key, catalog-not-hierarchy, stable-ids-in-keys /
names-in-metadata, immutable events, per-producer RPC, media isolation.
All of those survive in this grammar. The skeleton itself is rejected:

- **Filler nouns route nothing.** The literal plurals `assets/` and
  `entities/` appear in every key, so they can never discriminate a
  selector; they are pure wire and depth cost. No surveyed convention
  (Keelson, Sparkplug, uProtocol, rmw_zenoh) spends chunks on
  *non-discriminating* scaffolding nouns — literals that appear in every
  key and can never distinguish a selector. (Keelson's `pubsub` literal
  discriminates against its RPC branch, as this RFC's class chunk does.)
- **`<realm>` doesn't earn a fixed position** in a single-tenant system;
  isolation belongs to `<base>` (§1.1), where deployments that need it pay
  for it and others don't.
- **`entities/<kind>/` puts ontology in the key**, contradicting the
  draft's own first principle ("keys are routing addresses, not the
  ontology"). Whether `h-3fa9…` is a machine, a modem, or a service is a
  catalog fact and can change with better evidence; a kind chunk would make
  every reclassification a re-key.
- **Unresolved chicken-and-egg.** Sensors cannot publish under
  correlator-assigned entity ids they don't have at startup. This grammar
  resolves it with self-minted host origins plus catalog aliasing
  ([06-identity.md](06-identity.md)).
- **Trailing `<producer>` is unparseable** after a variable-depth
  `<domain>/<component>` (§1.5).
- **A parallel `raw/` tree doubles publication** on exactly the links that
  can least afford it; the producer chunk already preserves each producer's
  natural subtree under `telemetry/<producer>/…`.

### 6.3 Keelson-style trailing `source_id`

`{base}/@v{major}/{entity_id}/pubsub/{subject}/{source_id}` with open-ended
trailing depth. Works when subjects are single-chunk registry atoms
(Keelson's are); fails for open-depth subjects (§1.5). We keep Keelson's
verbatim `@v`/`@rpc` and its registry idea instead
([10-prior-art.md §2](10-prior-art.md)).

### 6.4 Type-hash in the key (rmw_zenoh)

`<topic>/<type_name>/<type_hash>` enforces schema compatibility by key
intersection — elegant, but silent: incompatible versions simply don't
communicate, with no operator-visible error, and every schema evolution
re-keys. In a single-vendor system the registry plus a schema id in the
payload/attachment gives the same guarantee *with* a diagnosable failure
mode ([08-registry.md](08-registry.md)).

### 6.5 Fixed arity with placeholder chunks (uProtocol)

11 chunks, literal `{}` placeholders for unused positions. Free positional
parsing, but the placeholder chunks are wire cost on every key, and the
open-depth subjects this convention needs (gNMI paths) don't fit fixed
arity. We take its lesson that the *addressable* part of the key (positions
1–5) should be fixed-arity — and it is.

### 6.6 Short class tokens (`t`/`s`/`e`)

Rejected (decided in [12-open-questions.md §5](12-open-questions.md)):
declared publishers intern keys — the wire cost is per-declaration, not
per-sample — and the declaration arithmetic puts the saving around 8 bytes
per key per hop, once, so readability wins outright. The decision record
keeps the revisit trigger and the no-alias-layer constraint.
