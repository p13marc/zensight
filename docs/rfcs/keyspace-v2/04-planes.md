# 04 — Data Classes and Planes

**Status: Draft** · normative chapter

The `<class>` position ([03-grammar.md §1.4](03-grammar.md)) splits the
keyspace into three **data classes** — `telemetry`, `state`, `events` —
and three **verbatim planes** — `@rpc`, `@media`, `@blob`. This chapter
defines what each class *means*: its update semantics, its cardinality
budget, its QoS defaults, and which storage shape captures it. The verbatim
planes get their own chapters ([05](05-control-rpc.md),
[07](07-bulk-planes.md)); this one covers the three data classes and the
rules for deciding where a given piece of information belongs.

---

## 1. The three data classes

| | `telemetry` | `state` | `events` |
|---|---|---|---|
| A sample is… | a measurement at an instant | the current truth of one key | an occurrence that happened |
| Update model | superseded — the next sample replaces the last | last-writer-wins on one stable key | immutable — a key is written once, never updated |
| Delete (`SampleKind::Delete`) | meaningless; MUST NOT be sent | **meaningful** — a tombstone retiring the key | meaningless; MUST NOT be sent |
| Missing a sample costs… | one point on a chart | *the truth* — until the next refresh | the occurrence, forever |
| Key cardinality | bounded, enumerable per producer | bounded and enumerable, **or** population-keyed under an explicit registry budget (§1.2) | unbounded over time (unique id per event), bounded **per rate budget** |
| Subject example | `cpu/usage` | `health`, `alert/<key>` | `capture/<ulid>` |

### 1.1 `telemetry` — superseded time-series

- Numeric or low-cardinality measurements published on a cadence. The key
  set of a producer is stable and enumerable; only values change.
- A publisher MUST publish each metric on its own stable key; it MUST NOT
  encode values, timestamps, or sequence numbers in the key.
- Late-joiner support (last sample per key) SHOULD be provided by the
  publisher-side cache mechanism of the underlying middleware (e.g.
  zenoh-ext AdvancedPublisher history) rather than by re-publishing.

### 1.2 `state` — last-writer-wins with tombstones

- Anything whose *latest value is the truth*: health, liveness documents,
  configuration echoes, identity evidence, alerts, catalog entities.
- The key MUST be stable for the lifetime of the stateful thing, so that
  firing→resolved→gone is a sequence of writes and one delete on a *single*
  key — not a family of keys to garbage-collect.
- **TTL.** Every live-state subject declares its staleness TTL in the
  registry ([08-registry.md §2](08-registry.md)) — the registry value is
  authoritative for both sides: publishers MUST refresh at ≤ TTL/2, and
  consumers MUST apply the registry TTL (not a locally configured one) when
  aging state out. (The reference application uses 60 s re-emission against
  a 900 s evidence TTL.) State whose producer's liveliness token
  ([§5](#5-liveliness-presence)) is absent SHOULD be treated as suspect
  immediately and MUST be treated as stale after its TTL — this is what
  retires a firing alert whose publisher crashed without tombstoning.
- **Tombstones.** A retired key MUST be tombstoned with a Zenoh delete
  (`SampleKind::Delete` — not a payload marker), and consumers MUST treat
  the delete as authoritative retirement. Seed replies
  ([05-control-rpc.md §4](05-control-rpc.md)) MUST NOT present a key whose
  latest sample is a delete as live; deployments MUST retain tombstone
  visibility for at least the subject's TTL, so that every seed mechanism
  agrees on retirement.
- **Cardinality budget.** A state subject keyed by an *observed population*
  (one key per seen IP, device, unit — e.g. `evidence/names/<ip-slug>`,
  `@catalog/state/pdns/<ip-slug>`) is permitted only with an explicit
  registry budget: the entry MUST declare the expected population bound and
  an aging rule (entries past TTL are tombstoned by their publisher).
  Unbudgeted population-keyed state is a registry-review reject — it is the
  loophole through which the "bus is low-cardinality" corollary (§2) and
  the affordable-firehose story (D2) would otherwise leak.
- **No cross-key atomicity.** The bus cannot deliver multi-key transactions
  and the convention does not simulate them: every state key MUST be
  independently coherent — a consumer acting on one key in isolation may be
  briefly *incomplete*, never *wrong*. Where a transition spans keys (the
  catalog merge writes entity + alias + tombstone,
  [06-identity.md §5](06-identity.md)), the writer MUST order writes so
  every intermediate is safe, and consumers MUST tolerate the torn window.
  Anything needing true snapshot semantics must be one document on one key.
- **Oversized values.** State values SHOULD stay small (they ride
  firehoses and seeds). A subject whose value can grow large MAY register
  `delivery = "invalidate"` ([08-registry.md §2](08-registry.md)): the
  published document carries identity + version/content-hash + summary
  only, and consumers pull the body on demand (`@rpc` read or `@blob` by
  hash) — D-Bus's `invalidated_properties` pattern
  ([10-prior-art.md](10-prior-art.md)).

**Alerts are state.** An alert has a stable identity key
`state/<producer>/alert/<alert_key>`, transitions firing → resolved on that
one key, and is retired by tombstone. `<alert_key>` is normatively: the
FNV-1a 64-bit hash of the rule name and the sorted discriminating labels
(host-scoped labels excluded), rendered as 16 lowercase hex chars. The
producing source is *not* hashed — origin and producer are already in the
key, which is what makes the key origin-scoped. (This deliberately differs
from the incumbent `alert_key`, which prefixes the rule name and hashes the
source; see [11-zensight-profile.md §3](11-zensight-profile.md).) Modelling
alerts as events would force every consumer to re-derive "what is firing
now" from an unbounded log — the exact query the class system should answer
with one selector: `<base>/@v1/*/state/*/alert/*`.

### 1.3 `events` — immutable occurrences

- Discrete, low-rate happenings that are *records*, not measurements:
  capture triggered, artifact generated, unit entered failed state, config
  applied.
- Each event key MUST end in a unique, time-sortable id (ULID recommended,
  key-encoded lowercase per [03-grammar.md §2](03-grammar.md)) and MUST be
  written exactly once (a publisher attestation — no checker can observe
  it).
- `events` is a **budgeted** class: the registry entry for an event subject
  MUST declare its rate class in the `rate` field
  ([08-registry.md §2](08-registry.md): `rare` ≤ 1/h · `low` ≤ 1/min ·
  `burst(n/h)` a declared cap), and per-record streams that can burst
  unboundedly (log lines, flow records, packets) MUST NOT be events — they
  are served on demand via `@rpc` (rule R3 below). Events exist so that
  *rare, meaningful* occurrences survive verbatim; they are not a log
  transport.
- Retention/replay of events is a storage-deployment concern
  ([12-open-questions.md §4](12-open-questions.md)); the bus contract is
  only immutability + unique keys.

---

## 2. Placement rules

The class of a subject is fixed by the registry, decided with these rules:

- **R1 — Is the latest value the whole truth?** → `state`.
  (Health, liveness, evidence, alerts, stream status, entity documents.)
- **R2 — Is it a measurement where the next sample makes the last one
  obsolete?** → `telemetry`. (Counters, gauges, rates, rollups.)
- **R3 — Is it an occurrence that must not be lost or rewritten, at a rate
  a human could review?** → `events`. If the rate is machine-scale
  (per-line, per-flow, per-packet), it is **not** publishable: hold it in a
  bounded ring at the producer and serve it via `@rpc` on demand.
- **R4 — Is it opaque bytes at frame rate?** → `@media`.
- **R5 — Is it bulk bytes with an identity (file, tree, chunk)?** → `@blob`.
- **R6 — Is it a question or an instruction?** → `@rpc`. Nothing under the
  data classes is ever a command; the data planes are strictly
  producer→consumer.

A corollary worth stating: **the bus is low-cardinality by budget.**
Everything high-cardinality is pull-only (`@rpc`), content-addressed
(`@blob`), rate-budgeted (`events`), or population-budgeted state
(§1.2) — and the last two budgets live in the registry, where review can
refuse them. "By construction" would overclaim: the grammar cannot stop an
unbudgeted key family; the registry can.

---

## 3. QoS defaults per class

QoS is declared per subject as a **named profile** in the registry
(`qos = "<profile>"`, [08-registry.md §2](08-registry.md)); each class has
a default profile. The profile vocabulary is closed — these five, mapping
to Zenoh reliability × congestion control × priority:

| Profile | Reliability | Congestion | Priority | Default for |
|---|---|---|---|---|
| `sampled` | best-effort | drop | data-low | `telemetry` |
| `refreshed` | best-effort | drop | data | `state` that self-heals (see rule below) |
| `transition` | reliable | block | data | `state` written on transition; `events` |
| `alert` | reliable | block | interactive-high | `state/*/alert/*` |
| `frame` | best-effort | drop | interactive-high | `@media` (a stale frame is worthless; the encoder must never block) |

The `refreshed`/`transition` split inside `state` is decided by a testable
rule: state that is re-published on a cadence ≤ TTL/2 self-heals after a
drop and MAY use `refreshed`; state written **only on transition** (an
alert flank, an entity tombstone, a config echo) does not self-heal and
MUST use `transition` or stronger.

The query planes have no publisher-side profile — **Zenoh replies inherit
the QoS of the query** (the server-side reply-QoS setters are documented
no-ops). The obligations are therefore client-side: `@rpc` callers SHOULD
issue GETs at interactive-high priority; `@blob` callers MUST issue GETs at
data-low — that, not anything the responder does, is what makes bulk
transfer yield ([07-bulk-planes.md §2](07-bulk-planes.md)).

All publishers on the data classes and `@media` MUST be *declared*
publishers — never one-shot ad-hoc puts — so keys are interned, routing is
primed, and QoS is attached once ([02-principles.md P7](02-principles.md)).
Sole exemption: seeding a router-hosted `@blob` content store MAY use puts
([07-bulk-planes.md §2](07-bulk-planes.md)) — those keys are
content-addressed, written once, and ride no firehose.

---

## 4. Storage mapping

The class chunk is the storage selector. A deployment configures storages
per class, with `strip_prefix` = `<base>/@v1` (a literal leftmost run, as
Zenoh requires):

| Storage | Selector | Backend shape |
|---|---|---|
| latest-value | `<base>/@v1/*/state/**` | LWW store honouring tombstones (fs/redb/rocksdb-class) |
| time-series | `<base>/@v1/*/telemetry/**` | append-per-key (influx-class) |
| event log | `<base>/@v1/*/events/**` | append-only; retention is the **backend database's** policy (e.g. an InfluxDB retention policy) — Zenoh's storage `garbage_collection` GCs metadata, not data |
| catalog | `<base>/@v1/@catalog/state/**` | LWW store — **explicit**, because `*` never matches `@catalog` (design property D4) |
| catalog history | `<base>/@v1/@catalog/state/pdns/**` | time-series capture of an LWW stream — see below |

Storages require timestamped samples for LWW to be meaningful: deployments
MUST enable Zenoh timestamping on the publishing side (routers default to
on, peers/clients to off). Concrete config, volume plugins, and the
overlap/`complete` caveats: [09-operations.md §2](09-operations.md).

Two consequences the incumbent keyspace could not offer:

- **No key ever needs client-side filtering to classify.** An exporter that
  wants only telemetry subscribes `…/*/telemetry/**` and receives only
  telemetry — the discard-after-the-wire filter (`is_telemetry_key`)
  disappears, and so does the bandwidth it wasted.
- **Storage policy is a deployment file, not application knowledge.** The
  storage manager needs no list of "which prefixes are state-like"; the
  grammar already said it.

**History of state is a storage choice, not a class change.** A time-series
storage pointed at a `state` selector records every LWW transition — that
*is* the history of that state, **provided the backend records deletes**: a
history capture MUST persist tombstones as explicit retirement markers (a
backend that silently drops `SampleKind::Delete` cannot distinguish
"retired at T" from "still current" and MUST NOT be claimed as state
history). The reference application's passive-DNS record
(`@catalog/state/pdns/<ip-slug>`: the full accumulated name-set for an IP,
superseded on each update) is ordinary LWW state on the bus whose influx
capture yields the historical IP↔name record. No dedicated "historical
plane" is needed in the grammar.

---

## 5. Liveliness (presence)

Presence is not a data class — it is the middleware's liveliness primitive,
which auto-retracts tokens on crash/disconnect. The convention places
tokens on keys that **mirror the state grammar**, so presence selectors look
like data selectors:

| Token key | Declared by |
|---|---|
| `<base>/@v1/<origin>/state/<producer>/alive` | every producer instance |
| `<base>/@v1/<origin>/state/<producer>/device/<device>/alive` | producers tracking downstream devices |
| `<base>/@v1/@catalog/state/alive` | the elected catalog owner ([06-identity.md §5.3](06-identity.md)) |

- Liveliness tokens live in the middleware's separate liveliness space;
  they do not collide with data subscribers even though the key shapes
  align. The alignment is for humans and for selector reuse:
  `<base>/@v1/*/state/*/alive` (liveliness subscriber) is the entire
  fleet-presence protocol, zero payload bytes. To keep that shape
  unambiguous, `alive` is a reserved subject leaf
  ([03-grammar.md §3](03-grammar.md)) — never a data subject.
- Zenoh does **not** enforce token uniqueness (two sessions can hold the
  same token key); a token is presence, not a lock. Where exclusivity
  matters (`@catalog`), the convention builds an explicit claim protocol on
  top ([06-identity.md §5.3](06-identity.md)).
- Consumers SHOULD treat retraction of a producer's `alive` token the way
  GDBusProxy treats a vanished bus-name owner
  ([10-prior-art.md](10-prior-art.md)): mark that producer's cached state
  suspect immediately rather than waiting out the TTL, and re-seed when the
  token reappears.
- Producers MUST declare their `@rpc` queryables *before* declaring their
  `alive` token, so that "alive ⇒ callable" holds and callers can attribute
  RPC silence ([05-control-rpc.md §3](05-control-rpc.md)).
- The token *key* is the identity record (origin + producer + device) —
  the pattern proven by rmw_zenoh's `@ros2_lv` discovery space and
  Keelson's presence tokens ([10-prior-art.md](10-prior-art.md)).
- Richer "who am I" registration (versions, capabilities, config hash)
  is ordinary state: `state/<producer>/sensor` (a registration document),
  refreshed on the state cadence.
