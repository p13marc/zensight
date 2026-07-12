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
- A late joiner on a cadence-published metric (position, temperature,
  a counter) needs no seeding: **the next publication is the seed** —
  `seed = none` is the class default (§3.1), and on constrained links it
  is also the right answer (zero standing machinery, zero seed traffic).
  The rare subject that genuinely needs a tail on arrival declares
  `seed = tail(n)` in the registry and gets it from a seed source
  (§3.1) — never from re-publishing on the data key, which corrupts the
  time-series.

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

The `refreshed`/`transition` split inside `state` is about the **cost of
waiting out a missed write**, not about self-healing: *all* live state
refreshes at ≤ TTL/2 (§1.2), so every state subject eventually self-heals.
State whose value changes on rare transitions that consumers cannot afford
to learn a refresh-period late (an alert flank, an entity tombstone, a
config echo) MUST use `transition` or stronger — the reliable profile buys
the *latency* of truth; the refresh mandate already guarantees its
eventuality.

The query planes have no publisher-side profile — **Zenoh replies inherit
the QoS of the query** (the server-side reply-QoS setters are documented
no-ops). The obligations are therefore client-side: `@rpc` callers SHOULD
issue GETs at interactive-high priority; `@blob` callers MUST issue GETs at
data-low — that, not anything the responder does, is what makes bulk
transfer yield ([07-bulk-planes.md §2](07-bulk-planes.md)).

All publishers on `telemetry`, `state`, and `@media` MUST be *declared*
publishers — never one-shot ad-hoc puts — so keys are interned, routing is
primed, and QoS is attached once ([02-principles.md P7](02-principles.md)).
Two scoped exemptions, both for write-once keys where interning buys
nothing: seeding a router-hosted `@blob` content store
([07-bulk-planes.md §2](07-bulk-planes.md)), and **`events` publication** —
every events key is written exactly once (§1.3), so there is no routing to
prime and no key to reuse; events are one-shot puts carrying their QoS
profile explicitly, kept cheap by the class's rate budget. (A declared
publisher per event key would be a declaration plus a put plus an
undeclare — the same wire cost wearing a compliance costume.)

**Profiles can be deployment-enforced.** Zenoh's `qos` overwrite
interceptor (stable config) rewrites priority/congestion/express per
keyexpr at the router, ignoring whatever the API set. Because the class is
a fixed key position, a deployment MAY pin the profile table above as
router policy — one overwrite rule per class prefix
([09-operations.md §4](09-operations.md)) — turning publisher discipline
into infrastructure guarantee. This is the QoS counterpart of storage
selection: one more infrastructure concern the grammar made a config file.

### 3.1 Delivery contracts per class

QoS says how samples travel; this section says what consumers are
**entitled to** — and deliberately not which library delivers it. The
entitlements live in the registry beside QoS
([08-registry.md §2](08-registry.md)); *mechanisms* are a deployment/build
choice, exactly as QoS profiles are enforceable by router config:

| Entitlement | Registry field | Meaning | Class defaults |
|---|---|---|---|
| **Seed** | `seed = none \| latest \| tail(n)` | what a late joiner may obtain per key without waiting out a cadence | `state`: `latest` (mandatory) · `telemetry`: `none` (a blank chart until the next cadence conforms) · `events`: n/a (see replay) |
| **Miss detection** | `detect_s` (live `state` only; default = `ttl_s`) | the maximum time within which a consumer can *detect* a missed transition | baseline meets `detect_s = ttl_s` for free (refresh + aging + liveliness); smaller values need the advanced tier (§3.3) |
| **Replay** | `replay = none \| window(t)` (`events` only) | how far back events are queryable | satisfied at deployment level by the events storage |
| **Tombstone visibility** | (from `ttl_s`) | a delete is observable ≥ TTL (§1.2) | enforced by storage GC sizing ([09-operations.md §2.3](09-operations.md)) |

Two universal rules, mechanism-independent:

- Late-joiner support MUST NOT be implemented by re-publishing on the data
  key — it corrupts the time-series and the LWW record; seeds come from a
  seed source, never from fake samples.
- **A deployment MUST provide at least one seed source for every `state`
  subject** — a latest-value storage covering `state/**`
  ([09-operations.md §2](09-operations.md)), publisher-side caches (§3.3),
  or both. A deployment with neither has no late-joiner story at all,
  which does not conform.

Telemetry loss needs no detection machinery: a dropped sample is priced
into the `sampled` profile and superseded by the next cadence — spending
bytes to detect drops the QoS invited is waste (consumers wanting a loss
*metric* can count, §3.3).

### 3.2 Baseline mechanics (stable API — the default)

The baseline is what every conforming participant runs unless a subject's
entitlements say otherwise. It uses nothing unstable:

- **Publishers**: one plain declared publisher per key with the class QoS
  profile (§3), plus one-shot puts for `events` (§3 exemption).
- **Presence & staleness**: one liveliness token per producer
  (`state/<producer>/alive`, §5) — *not* per key. A consumer marks a
  producer's state suspect on token retraction and stale at TTL; the
  mandated refresh (≤ TTL/2, §1.2) is the self-heal. Together these meet
  `detect_s = ttl_s` with zero per-key machinery.
- **Seeding**: subscribe-first, then a GET on the state selector answered
  by the latest-value storage, merged per key by HLC timestamp — newer
  value wins, newer tombstone wins ([05-control-rpc.md §4](05-control-rpc.md)).
- **Events**: reliable one-shot puts; replay and post-crash visibility are
  the events storage's job.

The honest limits of the baseline, so the trade is explicit: detection
latency for a missed transition is bounded by TTL (aging), not by a
heartbeat period; a telemetry late joiner sees nothing until the next
cadence unless the deployment adds a telemetry storage; per-sample gap
*recovery* does not exist — a lost reliable sample surfaces as staleness,
not retransmission.

### 3.3 The advanced tier (opt-in, unstable)

zenoh-ext's `AdvancedPublisher`/`AdvancedSubscriber` add per-key
publisher-side history, per-source sequence numbers, and gap recovery.
The tier is **opt-in per subject** — it is never a class default —
because its costs are per *key*, and the fleet's key population multiplies
them:

> **Cost box** (from the zenoh 1.9 source; there are no published
> benchmarks). A fully-optioned AdvancedPublisher creates **4 entities per
> key**, two of which — the cache queryable and the liveliness token at
> `<key>/@adv/pub/<zid>/<eid>/…` — are **network-wide routed declarations
> that no router can aggregate** (the key embeds zid+eid). A periodic
> `heartbeat(p)` publishes unconditionally forever (a 4-byte seqnum every
> `p`); the moment one subscriber enables `recovery(heartbeat)`, **every
> matching publisher's heartbeat crosses the network to it** — K
> matching keys ⇒ K/p msg/s per such subscriber, paid when nothing is
> lost. `history()` cold-start costs O(publishers × depth) reply samples;
> adding `detect_late_publishers()` can double it and turns every
> publisher restart into one token replay + one GET *per subscriber*.
> At a 10 000-key fleet that is ~40 000 entities, ~20 000 router-table
> entries per router, and (at `heartbeat(1 s)`) ~10 000 msg/s per
> recovering subscriber. Field evidence from the closest comparable
> workload (rmw_zenoh's token-per-entity model): seconds of router CPU
> saturation per node restart; its maintainers ship miss-detection off by
> default for traffic reasons.

Where the tier earns its cost:

- **Low-count, high-value transition state** needing `detect_s ≪ ttl_s` —
  alerts, config echoes: O(1–10) keys per producer. Configuration:
  `cache(1)` + `sample_miss_detection(sporadic_heartbeat(detect_s))` —
  sporadic, because these keys are idle almost always and the sporadic
  mode publishes only after a change. Consumers pair it with
  `recovery(heartbeat())`.
- **Router-less / storage-less meshes**, where publisher caches are the
  *only* possible seed source: `cache(1)` on state subjects (no miss
  detection unless `detect_s` demands it), consumers seed with
  `history()`.
- **Chart-tail seeding without a telemetry storage**: `cache(n)` on the
  handful of subjects whose registry says `seed = tail(n)` — not across a
  wide telemetry fan.

What the tier is NOT for: wide telemetry fans (hundreds of keys per
producer — the entity and heartbeat arithmetic above), telemetry gap
*recovery* (fighting the `sampled` profile: re-querying shed samples
presses on exactly the congested link), `events` replay (**unbuildable**:
a publisher owns one key, every events key is unique — an events "cache
ring" cannot exist; replay is the storage's job), and `@media` (recovering
a superseded frame is anti-useful).

Implementation notes for the tier (production-learned): timestamping must
already be on ([09-operations.md §0](09-operations.md)) — and do not rely
on the builder to catch its absence: only the cache-*only* configuration
self-checks; cache + miss-detection builds happily untimestamped. Always
set `HistoryConfig::max_samples` — **the default is unbounded buffering**;
`history()` MUST drain through an unbounded (or provably burst-sized)
handler channel or the declare deadlocks the session; declare
AdvancedSubscribers after one-shot seed GETs on the same session (§3.2's
subscribe-first rule is satisfied by the AdvancedSubscriber itself — its
declare-time history query is internally race-free; the ordering rule is
about *other* GETs sharing the session); prefer `periodic_queries(p)` over
`recovery(heartbeat)` on wide wildcard subscriptions — it bounds load by
the subscriber's choice instead of the publishers' key count. A consumer
that wants a loss *metric* without recovery uses
`sample_miss_listener()` (reports `{source, count}`).

The sidecar keys the tier creates are verbatim-isolated under the data key
(`<key>/@adv/pub/<zid>/…`, `<key>/@adv/sub/<zid>/…`): they ride no
firehose and no data selector, but principals that opt in DO need the
`@adv` ACL rules — and a missing rule fails *silently* (empty seeds,
denied recovery, indistinguishable from "nothing to recover";
[09-operations.md §3](09-operations.md)).

### 3.4 Choosing a tier

| Deployment shape | State | Telemetry | Events |
|---|---|---|---|
| Router + storages (the normal fleet) | baseline; advanced only on subjects with `detect_s ≪ ttl_s` | baseline (storage supplies any tail) | baseline + events storage |
| Router-less mesh (no storage anywhere) | advanced `cache(1)` — publisher caches are the only seed | baseline; `cache(n)` only where a tail entitlement exists | accept producer-lifetime visibility, or add a storage node |
| Constrained leaf | baseline + plain subscriber + local store (no history bursts, no heartbeats) | baseline | baseline |

The split mirrors §3's QoS design: **entitlements in the registry,
mechanisms in deployment/build config.** A subject's row in the registry
says what consumers may rely on; whether a cache or a storage delivers it
is invisible to the keys and to the wire contract.

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
on, peers/clients to off). Two facts that shape deployment choices: Zenoh
**storage replication** (anti-entropy alignment between replicas) works
only on latest-value storages — history capture does not replicate at the
Zenoh layer, its availability is the backend database's concern; and the
storage `garbage_collection.lifespan` is the knob that bounds tombstone
visibility — it MUST be set ≥ the longest state TTL, or §1.2's
tombstone-retention rule is silently violated (the default is 24 h).
Concrete config, volume guidance, replication, and the overlap/`complete`
caveats: [09-operations.md §2](09-operations.md).

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
- Producers MUST declare their `@rpc` queryables **and** (if on the
  advanced tier) all their AdvancedPublishers *before* declaring their
  `alive` token — "alive ⇒ callable **and seedable**": callers can
  attribute RPC silence ([05-control-rpc.md §3](05-control-rpc.md)), and a
  §5-triggered re-seed can never race an undeclared cache.
- **One roster, not two.** The advanced tier's `publisher_detection`
  tokens (`<key>/@adv/pub/…`, §3.3) are per-*publisher-entity* machinery,
  consumed only by AdvancedSubscriber internals
  (`detect_late_publishers()`); the per-producer `alive` token is the
  only presence signal consumers, dashboards, and RPC attribution may
  read. An AdvancedSubscriber already re-seeds natively on `@adv` token
  events and MUST NOT double-trigger from `alive`.
- The token *key* is the identity record (origin + producer + device) —
  the pattern proven by rmw_zenoh's `@ros2_lv` discovery space and
  Keelson's presence tokens ([10-prior-art.md](10-prior-art.md)).
- Richer "who am I" registration (versions, capabilities, config hash)
  is ordinary state: `state/<producer>/sensor` (a registration document),
  refreshed on the state cadence.
