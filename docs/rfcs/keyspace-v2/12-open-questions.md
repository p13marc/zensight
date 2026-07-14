# 12 — Decisions

**Status: v1.1 — all six original items DECIDED** (2026-07-12, review round 4);
**§7 added 2026-07-14** (the version chunk is plain, not verbatim). This
chapter began as the open-questions list; it is kept as the decision
record — each item preserves the alternatives and names its
**revisit trigger**, the concrete future fact that would reopen it.
Everything not listed here was a settled recommendation from the start.

---

## 1. Realm / multi-tenancy placement — DECIDED: deployment prefix

**Question.** Should the grammar reserve a tenant/realm position, or is a
deployment prefix (`acme/fleet-a` as `<base>`) sufficient?

**Alternatives.** (a) deployment prefix only
([03-grammar.md §1.1](03-grammar.md)); (b) reserve a fixed chunk between
`<base>` and the version chunk now, empty-tolerated later.

**Decision: (a).** The deployment prefix is *zero-code* — it is the
session `namespace`, set in config, invisible to application code, and
enforced as an ingress filter (a session cannot even accidentally consume
another realm's traffic). A reserved-but-unused chunk would cost every key
today for a need the namespace already serves, and the version chunk means a
future `v2` could introduce a realm position without ambiguity anyway.

**Revisit trigger.** A real deployment needs *cross-realm* consumers —
the one thing a namespaced session cannot be.

## 2. Observed-device promotion to origin — DECIDED: never

**Question.** May a proxied device (a router polled over SNMP) ever appear
in the origin position — e.g. a `d-<hash>` origin published on its behalf
by a gateway (Keelson's gateway pattern)?

**Alternatives.** (a) never — devices are subjects
([06-identity.md §3](06-identity.md)); (b) a registered gateway may mint
device origins, buying per-device ACL and link budgets at the cost of the
origin-equals-trust-boundary invariant.

**Decision: (a).** The origin chunk's entire value comes from meaning
exactly one thing — the machine that runs the publisher. That invariant is
load-bearing: D6's enrollment story (cert CN = origin) and the catalog's
evidence weighting both assume it. Per-device ACL on proxied devices is a
*different trust model*, and trust models arrive as reviewed amendments,
not drift.

**Revisit trigger.** A deployment that must ACL proxied devices
individually — bring it as an amendment defining who vouches for a
device origin and how it enrolls.

## 3. Durable command delivery — DECIDED: RPC-only, desired-state escape hatch

**Question.** RPC is deliberately not durable — an offline host misses the
call and the caller can determine that it did
([05-control-rpc.md §3](05-control-rpc.md)). Does any control path need
instructions that survive producer downtime?

**Alternatives.** (a) RPC-only; (b) desired-state reconciliation: a
controller publishes `state/<producer>/desired/<topic>` (LWW,
storage-backed), producers converge on (re)connect.

**Decision: (a), with (b) as the sanctioned escape hatch.** No shipped
control channel needs durability (checked against the full mapping,
[05-control-rpc.md §5](05-control-rpc.md)), and (b) is already
expressible in the grammar with zero new mechanism — which is exactly why
it does not need to be pre-built. The permanently forbidden third option
is durable pub/sub *commands* — fire-and-forget imperatives with no
convergence semantics. If durability is ever wanted, it must arrive as
desired state.

**Revisit trigger.** A control path whose miss is unacceptable *and*
whose semantics cannot be expressed as convergence on desired state.

## 4. `events` retention and replay — DECIDED: storage is the contract

**Question.** The bus contract for `events` is immutability + unique keys
+ a rate budget ([04-planes.md §1.3](04-planes.md)). What is the
*deployment* contract — how long are events queryable, and via what?

**Alternatives.** (a) events storage, replay = storage query (retention
set in the backend database); (b) a producer-side bounded ring served over
`@rpc` (`@rpc/<producer>/events?since=…`). Round 2 briefly claimed (b) was
"resolved" by the AdvancedPublisher cache; round 3 showed that is
unbuildable (a publisher owns one key; every events key is unique —
[04-planes.md §3.3](04-planes.md)).

**Decision: (a) is the normative deployment contract** — a deployment
whose registry grants `replay = window(t)` on any events subject MUST run
an events storage covering it. **(b) is a registered optional pattern**
for router-less deployments: a producer MAY serve a bounded recent-events
ring as an ordinary `@rpc` read procedure, registered like any procedure —
it covers only events whose producer is alive and is never the durable
record. The rate-class boundaries are **provisionally ratified** as
specified (`rare` ≤ 1/h · `low` ≤ 1/min · `burst(n/h)` declared): they
gate registry review, not wire behavior, so adjusting them after a release
of real data is a MINOR registry edit, not a convention change.

**Revisit trigger.** First-release data showing the rate-class boundaries
mis-sized (adjust the numbers), or a router-less deployment class making
the (b) pattern common enough to standardize its selector parameters.

## 5. Short class tokens — DECIDED: full words; measurement task closed

**Question.** `telemetry`/`state`/`events` are 5–9 bytes per key. Do they
cost anything real on constrained links, and should the grammar use
`t`/`s`/`e`?

**Decision: full words. The measurement task is closed by arithmetic, not
deferred.** Interning is source-verified: a declared publisher sends one
`DeclareKeyExpr` per key per hop and every subsequent sample carries a
varint id with zero key bytes — so the class token costs nothing in steady
state. The entire residual cost is declaration-time and selector bytes:
`telemetry` → `t` saves 8 bytes × keys × hops, *once* — ~80 KB one-time
across a 10 000-key fleet whose declarations already cost megabytes
dominated by the subject tail and (on the advanced tier) zid/eid chunks.
No bandwidth-shaped measurement can change a decision at that magnitude;
readable keys win. The OPC-UA constraint stands regardless: any future
compression MUST be a spelling change in the grammar, never a runtime
alias table with a different lifetime than the names it stands for
([10-prior-art.md §10](10-prior-art.md)).

**Revisit trigger.** A deployment where *declaration* bytes are the
binding constraint (extremely constrained links with large key
populations) — and even then, reduce the key population first
([04-planes.md §3.3](04-planes.md)'s arithmetic says entities, not token
length, are what scale).

## 6. Alert placement — DECIDED: subject under `state`

**Question.** Alerts live at `state/<producer>/alert/<key>`
([04-planes.md §1.2](04-planes.md)). Should they instead be a dedicated
class-level token (`alerts`) beside `telemetry`/`state`/`events`?

**Alternatives.** (a) subject under `state` — alerts inherit all state
machinery (seed, latest-value storage, tombstones, TTL);
(b) own class — a shorter fleet selector (`…/*/alerts/**` vs
`…/*/state/*/alert/*`) and a coarser ACL boundary.

**Decision: (a).** Alerts *are* LWW state semantically; a fourth data
class would dilute the "class = update semantics" rule for a selector
convenience. The strongest counter-evidence is acknowledged: alerts
already get a dedicated QoS profile (`alert`,
[04-planes.md §3](04-planes.md)) — but per-subject specialization is
exactly what the registry exists to express, and QoS-by-subject is a
lighter instrument than a class.

**Revisit trigger.** A real consumer that needs alerts-without-state read
permission (the ACL case) — the one thing the subject placement cannot
grant.


## 7. Verbatim version chunk — DECIDED (v1.1): plain `v<int>`, reversing v1.0

**Question.** The version chunk shipped as a verbatim `@v1` so that `*`/`**`
could not cross it, making v1 invisible even to an *un-versioned* selector
([03-grammar.md §1.2](03-grammar.md), D1). Is that worth its cost?

**The cost, discovered in production.** Zenoh's advanced pub/sub (zenoh-ext)
parks a publisher-detection liveliness token at
`<key>/@adv/pub/<zid>/<eid>/<meta>` and parses it back with
`${remaining:**}/@adv/…`. The publisher's key must be captured by
`${remaining:**}` — and `**` never matches a chunk beginning with `@`. So
`remaining` could not span any key containing `@v1`: **every** token we declared
was unparseable by the only code that reads them. `detect_late_publishers()` was
silently dead (the parse is the first thing its callback does), and every
subscriber logged *"malformed liveliness token key expression"* once per
publisher, indefinitely.

**Alternatives.** (a) plain `v<int>` — drop the `@`; (b) fold `<base>/@v1` into
the session `namespace`, so the app-level key zenoh-ext sees carries no verbatim
chunk; (c) keep `@v1`, disable publisher detection; (d) fork zenoh-ext to parse
the token textually rather than by keyexpr match.

**Decision: (a).** The `@` bought exactly one thing beyond what a plain chunk
gives: invisibility to an **un-versioned** selector — i.e. coexistence with a
*pre-convention* keyspace. That is a **migration** property, and the migration
is done. The property that keeps working forever — *a v1 selector never matches
a v2 key* — never needed the `@`, because `v1` and `v2` are different literal
chunks.

(b) works (verified) but is a large refactor and abuses the namespace, whose
purpose is to keep the *base* out of application code, not to smuggle a grammar
position out of sight; it also makes one session unable to speak two majors.
(c) permanently forfeits a delivery guarantee to keep a migration artifact.
(d) is impossible as a *fix*: the `@`-exclusion is a Zenoh **matching** rule, so
no keformat spelling can capture a key containing a verbatim chunk — a fork would
have to abandon keyexpr matching entirely, and we would carry it forever.

The chunks that remain verbatim are the ones still doing daily work — the planes
(`@rpc`/`@media`/`@blob`, D2) and service origins (`@catalog`, D4). No advanced
publisher ever publishes on those, so none of them is affected.

**Cost, stated plainly.** An un-versioned `<base>/**` selector now reaches v1
keys. A deployment coexisting with a pre-convention keyspace must separate the
two by `<base>`, not by key algebra. Pinned by
`zensight-keyspace/tests/guard.rs::d1_version_isolation` (which asserts the loss
explicitly, so it is a decision and not a drift) and
`zensight-keyspace/tests/adv_token.rs` (the token must parse).

**Revisit trigger.** Zenoh gains a wildcard that crosses verbatim chunks, or
zenoh-ext stops locating its sidecars by keyexpr match — either would let the
version chunk be verbatim again at no cost.
