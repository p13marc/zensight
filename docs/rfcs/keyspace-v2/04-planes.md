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
| Key cardinality | bounded, enumerable per producer | bounded, enumerable per producer | unbounded over time (unique id per event), bounded **per rate budget** |
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
- Publishers of live state SHOULD refresh at ≤ TTL/2 of whatever staleness
  contract consumers apply, so that state ages out rather than lying
  forever (the reference application uses 60 s re-emission against a 900 s
  evidence TTL).
- A retired key MUST be tombstoned with a delete, and consumers MUST treat
  the delete as authoritative retirement.

**Alerts are state.** An alert has a stable identity key
(`state/<producer>/alert/<alert_key>`, where `<alert_key>` is a stable hash
of rule + discriminating labels), transitions firing → resolved on that one
key, and is retired by tombstone. Modelling alerts as events would force
every consumer to re-derive "what is firing now" from an unbounded log —
the exact query the class system should answer with one selector:
`<base>/@v1/*/state/*/alert/*`.

### 1.3 `events` — immutable occurrences

- Discrete, low-rate happenings that are *records*, not measurements:
  capture triggered, artifact generated, unit entered failed state, config
  applied.
- Each event key MUST end in a unique, time-sortable id (ULID recommended)
  and MUST be written exactly once.
- `events` is a **budgeted** class: the registry entry for an event subject
  MUST declare an expected rate class ([08-registry.md](08-registry.md)),
  and per-record streams that can burst unboundedly (log lines, flow
  records, packets) MUST NOT be events — they are served on demand via
  `@rpc` (rule R3 below). Events exist so that *rare, meaningful*
  occurrences survive verbatim; they are not a log transport.
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

A corollary worth stating: **the bus is low-cardinality by construction.**
Everything high-cardinality is either pull-only (`@rpc`), content-addressed
(`@blob`), or rate-budgeted (`events`).

---

## 3. QoS defaults per class

Each class carries a default QoS profile; registry entries MAY override per
subject with justification. (Profiles map to Zenoh reliability × congestion
control × priority.)

| Class / plane | Reliability | Congestion | Priority | Rationale |
|---|---|---|---|---|
| `telemetry` | best-effort | drop | data-low | superseded — a dropped sample is replaced |
| `state` (health/liveness/evidence) | best-effort → reliable* | drop → block* | data | *fast-refresh state may drop; slow-refresh state (evidence, entities, alerts) MUST NOT |
| `state/*/alert/*` | reliable | block | interactive-high | an alert transition must arrive |
| `events` | reliable | block | data | immutable records must arrive |
| `@rpc` | (query semantics) | — | interactive-high | operator-facing latency |
| `@media` | best-effort | drop | interactive-high | a stale frame is worthless; the encoder must never block |
| `@blob` | (query semantics) | — | data-low | bulk transfer yields to everything |

The split inside `state` is deliberate: state that refreshes every few
seconds (health) self-heals after a drop; state that refreshes slowly or
never (an alert transition, an entity tombstone) does not, and MUST ride
reliable/block.

All publishers MUST be *declared* publishers — never one-shot ad-hoc puts —
so keys are interned, routing is primed, and QoS is attached once
([02-principles.md P7](02-principles.md)).

---

## 4. Storage mapping

The class chunk is the storage selector. A deployment configures storages
per class, with `strip_prefix` = `<base>/@v1` (a literal leftmost run, as
Zenoh requires):

| Storage | Selector | Backend shape |
|---|---|---|
| latest-value | `<base>/@v1/*/state/**` | LWW store honouring tombstones (fs/redb/rocksdb-class) |
| time-series | `<base>/@v1/*/telemetry/**` | append-per-key (influx-class) |
| event log | `<base>/@v1/*/events/**` | append-only, retention-windowed |
| catalog | `<base>/@v1/@catalog/state/**` | LWW store — **explicit**, because `*` never matches `@catalog` (design property D4) |
| catalog history | `<base>/@v1/@catalog/state/pdns/**` | time-series capture of an LWW stream — see below |

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
*is* the history of that state. The reference application's passive-DNS
record (`@catalog/state/pdns/<ip-slug>`: the full accumulated name-set for
an IP, superseded on each update) is ordinary LWW state on the bus whose
influx capture yields the historical IP↔name record. No dedicated
"historical plane" is needed in the grammar.

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
| `<base>/@v1/@catalog/state/alive` | the catalog (doubles as its single-writer guard) |

- Liveliness tokens live in the middleware's separate liveliness space;
  they do not collide with data subscribers even though the key shapes
  align. The alignment is for humans and for selector reuse:
  `<base>/@v1/*/state/*/alive` (liveliness subscriber) is the entire
  fleet-presence protocol, zero payload bytes.
- The token *key* is the identity record (origin + producer + device) —
  the pattern proven by rmw_zenoh's `@ros2_lv` discovery space and
  Keelson's presence tokens ([10-prior-art.md](10-prior-art.md)).
- Richer "who am I" registration (versions, capabilities, config hash)
  is ordinary state: `state/<producer>/sensor` (a registration document),
  refreshed on the state cadence.
