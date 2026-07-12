# 03 — Canonical Grammar

**Status: Draft** · normative chapter · uses MUST/SHOULD/MAY per RFC 2119

This chapter defines the canonical key grammar of the convention. Everything
else in this RFC (planes, RPC, identity, registry) hangs off this shape.

---

## 1. Canonical form

```
<base>/@v1/<origin>/<class>/<producer>/<subject...>
```

Six positions; the first five are exactly one chunk each, the subject is an
open-ended path of one or more chunks. Example (reference application):

```
zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
└──┬───┘ └┬┘ └─────┬──────┘ └───┬───┘ └──┬──┘ └───┬───┘
  base  version  origin       class   producer  subject
```

| Position | Name | Arity | Rule |
|---|---|---|---|
| 1 | `<base>` | 1 chunk | Deployment root. Configurable per deployment; MUST be a literal, non-verbatim chunk. |
| 2 | `@v1` | 1 chunk | Convention major version. MUST be a verbatim chunk of the form `@v<integer>`. |
| 3 | `<origin>` | 1 chunk | Who publishes: a **host origin** (stable opaque id) or a **service origin** (verbatim `@<service>`). |
| 4 | `<class>` | 1 chunk | Message class: `telemetry` \| `state` \| `events`, or a verbatim plane `@rpc` \| `@media` \| `@blob`. |
| 5 | `<producer>` | 1 chunk | The component on the origin that produced the data (sensor/agent), optionally instance-suffixed. Omitted under service origins. |
| 6+ | `<subject...>` | ≥ 1 chunk | Registry-governed meaning path. The only open-ended part of the key. |

### 1.1 `<base>` — deployment root

- The base chunk names the *deployment*, not the software: two independent
  installations on one Zenoh network MUST use different bases (or a shared
  base behind different routers/ACLs).
- Multi-tenancy, realms, and sites are **deployment prefixes**, not grammar
  chunks: `acme/fleet-a` is a valid `<base>` (two chunks) as long as it is a
  fixed literal run. Nothing in the convention ever inspects the base; every
  selector in this RFC is written relative to it.
- Rationale: an isolation token you rarely use should not cost every key a
  chunk. Deployments that need it prepend it; deployments that don't, don't
  pay. (See NATS guidance: the first token is the isolation key —
  [10-prior-art.md §6](10-prior-art.md).)

### 1.2 `@v1` — version chunk

- MUST be verbatim (`@`-prefixed). A verbatim chunk is matched only by an
  identical chunk — `*` and `**` never cross it — so keys of two convention
  majors are **mutually invisible by key algebra**, not by discipline.
  A `<base>/**` subscriber sees nothing under `<base>/@v1/…`, and a
  `<base>/@v1/**` subscriber sees nothing outside it.
- This makes coexistence of an old and a new keyspace a *property of the
  grammar*: both can share a network indefinitely without a bridge, and a
  consumer opts into exactly one (or explicitly both).
- The major is bumped **only** when the grammar or the semantics of a
  position change incompatibly. Additive evolution (new subjects, new
  producers, new procedures) is a **registry** change and never bumps the
  chunk ([08-registry.md](08-registry.md)).
- Everything the convention defines lives **under** the version chunk.
  Nothing — no status key, no discovery key, no side channel — may sit
  beside it. (Sparkplug placed its STATE topic outside `spBv1.0/` and needed
  a breaking release to fix it; see [10-prior-art.md §4](10-prior-art.md).)

### 1.3 `<origin>` — publishing identity

Two forms:

**Host origin** — `h-<12hex>`: a stable, opaque, self-minted identifier of
the publishing machine.

- MUST be derivable by the publisher alone, at first startup, with no
  network round-trip (no registration service, no coordinator).
- Reference derivation: `h-` + first 12 hex chars of
  `sha256(machine-id + application salt)`. The raw machine-id never leaves
  the host. All publishers on one machine derive the same value.
- MUST be stable across restarts and upgrades. MUST NOT encode a mutable
  name (hostname, DNS, IP). Human names belong to the catalog
  ([06-identity.md](06-identity.md)).
- Fallback rules for hosts without a machine-id are defined in
  [06-identity.md §2](06-identity.md).

**Service origin** — `@<service>`: a verbatim chunk naming a singleton,
deployment-level service (not tied to one host), e.g. `@catalog` for the
identity correlator.

- Verbatim on purpose: `*` does not match a verbatim chunk, so fleet-wide
  selectors like `<base>/@v1/*/state/**` structurally exclude service
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
  same producer run on one origin (`snmp` / `snmp-2`). The instance suffix
  is the collision rule: two publishers MUST NOT share
  `<origin>/<class>/<producer>` ownership of a subject.
- The producer chunk is **before** the subject, not after it. A trailing
  producer (Keelson's `source_id`) is only parseable when the subject has
  fixed depth; this convention's subjects are open-depth (gNMI paths,
  directory-like metrics), so a trailing chunk would be ambiguous —
  no parser could tell where the subject ends and the producer begins.
- Under service origins the producer position is **omitted** (the service
  *is* the producer): `<base>/@v1/@<service>/<class>/<subject...>`.

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
- Verbatim chunks MUST match `@[a-z0-9][a-z0-9_-]*` (plus the `@v<int>`
  version form).
- Values that contain `/`, `:`, or other excluded characters MUST be
  **slugged** before entering a key, and the original value MUST travel in
  the payload. Reference slugs:
  - IP address: `.`/`:` → `-` (`10.0.0.7` → `10-0-0-7`,
    `2001:db8::1` → `2001-db8--1`)
  - systemd unit / filename: literal if already legal, else `-` for each
    excluded character
- Wildcards (`*`, `**`) and the sub-chunk wildcard `$*` are **selector**
  syntax and MUST NOT appear in published keys. Published keys and selectors
  MUST be in Zenoh canon form.
- `$*` MUST NOT be used in selectors either. If you feel the need for
  `if-eth0-rx$*`, the key is wrong: split the multi-valued chunk into
  chunks (`if/eth0/rx_bytes`). (Zenoh: `$*` is markedly slower and strains
  the infrastructure; see [02-principles.md P6](02-principles.md).)
- Per-message data (request ids, timestamps, sequence numbers) MUST NOT
  appear in `telemetry` or `state` keys — unbounded-cardinality keys defeat
  interning, caches, and storage. The single sanctioned exception is the
  unique id of an `events` key and content hashes under `@blob`
  ([04-planes.md §4](04-planes.md)).

---

## 3. Reserved tokens

| Token | Position | Meaning |
|---|---|---|
| `@v<int>` | 2 | convention major version |
| `h-<12hex>` | 3 | host origin id (reference derivation §1.3) |
| `@catalog` | 3 | the identity/catalog service ([06-identity.md](06-identity.md)) |
| `telemetry`, `state`, `events` | 4 | data classes |
| `@rpc`, `@media`, `@blob` | 4 | verbatim planes |

Applications MAY register further service origins and MUST NOT redefine the
tokens above. New class or plane tokens are a convention-major change.

---

## 4. Design properties (each one is a testable claim)

The grammar is chosen so that the following hold *by key algebra* — they are
pinned as guard tests in the reference implementation, not enforced by
review:

**D1 — Version hermeticity.** `<base>/**` ∩ `<base>/@v1/**` = ∅.
Old and new keyspaces coexist with zero cross-talk.

**D2 — Per-origin firehose is data-only.**
`<base>/@v1/h-xxx/**` matches every `telemetry`/`state`/`events` key of that
host and **no** `@rpc`/`@media`/`@blob` key. One subscription = one host's
complete data plane, and it can never accidentally pull video frames or blob
chunks. This is the single most load-bearing property of the design.

**D3 — Class disjointness.** `<base>/@v1/*/telemetry/**`,
`…/*/state/**`, `…/*/events/**` are pairwise non-intersecting — literal
class chunks differ. Storage and QoS policy select on them directly.

**D4 — Service exclusion.** `<base>/@v1/*/state/**` does not match
`<base>/@v1/@catalog/state/**` (verbatim origin). Fleet selectors see hosts
only; catalog consumers subscribe by name.

**D5 — Targeted and fleet RPC from one key shape.**
`GET <base>/@v1/h-xxx/@rpc/netlink/sockets` reaches one host;
`GET <base>/@v1/*/@rpc/netlink/sockets` fans in over every host serving that
procedure — same key, `*` in the origin position. (The `*` matches host
origins but not `@catalog`, by D4's mechanism.)

**D6 — Static policy prefixes.** Every security- or storage-relevant
boundary (deployment, version, origin, class) is a fixed-position literal
run from the left, so:
- Zenoh storage `strip_prefix` (which must be a literal prefix) can strip
  `<base>/@v1` and select per class;
- ACL rules can be written as literal prefixes plus one trailing `**`
  (fast path) — e.g. host `h-xxx` may publish only `<base>/@v1/h-xxx/**`;
- a constrained link can allowlist by prefix (`…/h-xxx/state/**` +
  `…/h-xxx/telemetry/<producer>/**`) with no per-key inspection.

---

## 5. Normative example set

These keys are the reference examples used throughout the RFC (and by the
guard tests). Base = `zensight`.

```
zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
zensight/@v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
zensight/@v1/h-3fa9c2d41b7e/state/netring/health
zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7
zensight/@v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7
zensight/@v1/h-3fa9c2d41b7e/events/netring/capture/01JGXQZ4YQK8V6TXW3M9F2A7CD
zensight/@v1/h-3fa9c2d41b7e/@rpc/netlink/sockets
zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main
zensight/@v1/h-3fa9c2d41b7e/@blob/store/sha256/ab12cd34ef56
zensight/@v1/@catalog/state/entity/h-3fa9c2d41b7e
zensight/@v1/@catalog/state/pdns/93-184-216-34
```

And the canonical selectors:

| Need | Selector |
|---|---|
| everything about one host (data planes) | `zensight/@v1/h-3fa9c2d41b7e/**` |
| all telemetry, fleet-wide | `zensight/@v1/*/telemetry/**` |
| one protocol, fleet-wide | `zensight/@v1/*/telemetry/snmp/**` |
| all state (→ latest-value storage) | `zensight/@v1/*/state/**` |
| all alerts | `zensight/@v1/*/state/*/alert/*` |
| one host's health | `zensight/@v1/h-3fa9c2d41b7e/state/*/health` |
| fleet RPC fan-in | `zensight/@v1/*/@rpc/netlink/sockets` (GET) |
| entity documents | `zensight/@v1/@catalog/state/entity/*` |

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
zensight/@v1/<realm>/assets/<asset>/entities/<kind>/<entity>/<state|telemetry>/<domain>/<component>/<producer>
```

Adopted from it — with credit: the versioned root, the state/telemetry
split, producer-in-key, catalog-not-hierarchy, stable-ids-in-keys /
names-in-metadata, immutable events, per-producer RPC, media isolation.
All of those survive in this grammar. The skeleton itself is rejected:

- **Filler nouns route nothing.** The literal plurals `assets/` and
  `entities/` appear in every key, so they can never discriminate a
  selector; they are pure wire and depth cost. No surveyed convention
  (Keelson, Sparkplug, uProtocol, rmw_zenoh) spends chunks on scaffolding
  nouns.
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

Deferred, not rejected: declared publishers intern keys (the wire cost is
per-declaration, not per-sample), so readability wins until a measurement on
a constrained link says otherwise. Recorded as an open question
([12-open-questions.md §5](12-open-questions.md)).
