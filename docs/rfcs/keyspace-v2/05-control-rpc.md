# 05 — Control Plane: `@rpc`

**Status: Draft** · normative chapter

All interaction — questions, instructions, downloads-of-detail — happens on
the `@rpc` plane through **queryables** (request/reply), never through
pub/sub commands. This chapter defines the procedure grammar, targeting,
parameters, reply conventions, and how the incumbent command/query channels
map onto it.

---

## 1. Key shape

```
<base>/@v1/<origin>/@rpc/<producer>/<procedure...>
<base>/@v1/@<service>/@rpc/<procedure...>
```

- `@rpc` is a verbatim chunk: no `*`/`**` data selector can ever reach a
  procedure, and no RPC traffic ever leaks into a data subscription
  (design property D2, [03-grammar.md §4](03-grammar.md)).
- `<procedure...>` is one or more chunks, registry-governed
  ([08-registry.md](08-registry.md)). Multi-chunk procedures group related
  endpoints (`artifact/request`, `artifact/status`).
- The **server** is the producer (it declares the queryable); the **client**
  is whoever GETs. Parameters ride the Zenoh selector
  (`?state=established;port=443`), request bodies ride the query payload.

## 2. Targeting — the fan-out problem, solved by position

The origin chunk makes addressing a property of the key, in both
directions:

| Intent | Selector |
|---|---|
| ask/instruct **one host** | `GET <base>/@v1/h-xxx/@rpc/netlink/sockets?ip=10.0.0.7` |
| ask **the fleet**, collect all replies | `GET <base>/@v1/*/@rpc/netlink/sockets?ip=10.0.0.7` |
| ask every producer of one origin | `GET <base>/@v1/h-xxx/@rpc/*/health-detail` (if registered by several) |

This subsumes both control patterns the incumbent keyspace had to choose
between per channel:

- **Point control** (restart a unit, open a stream, run a capture) targets
  one origin structurally — a mistargeted instruction is now a *non-matching
  key*, not a payload filter every sensor must implement correctly.
- **Fan-in joins** (which host owns the socket for this flow?) keep their
  one-GET ergonomics via the `*` origin. Only hosts that serve the
  procedure reply; the client collects and joins, exactly as before.

Consequently the convention has **no `target` field in any envelope**:
addressing is never payload.

### 2.1 Fan-in call discipline (normative)

Zenoh's *defaults* do not deliver "collect all replies"; the following
discipline does, and fleet callers MUST follow it:

- **Target.** Fan-in GETs MUST set query target **All**. The default
  (`BestMatching`) short-circuits to a *single* queryable the moment any
  matching queryable is declared `complete` — one storage config away from
  silently collapsing the fleet to one reply. For the same reason, `@rpc`
  queryables MUST NOT be declared `complete`.
- **Reply key.** A procedure MUST reply on its **own concrete key**
  (`…/h-xxx/@rpc/<producer>/<procedure>`), never by echoing the query's
  wildcard selector. Zenoh's default reply consolidation keeps one reply
  *per reply key* — distinct origins on distinct keys survive it; a fleet
  replying on the shared wildcard key is consolidated down to one survivor.
  Callers MAY additionally disable consolidation (`None`) for belt and
  braces.
- **Attribution.** The caller joins the reply set against the liveliness
  roster (`<base>/@v1/*/state/*/alive`, [04-planes.md §5](04-planes.md)) to
  attribute non-replies — the reply set alone cannot say who *should* have
  answered.

## 3. Read, write, and long-running procedures

Three procedure idioms, distinguished in the registry, all on the same key
shape:

**Read** — idempotent detail queries. High-cardinality data (processes,
sockets, flows, log lines) is held in bounded rings at the producer and
served on demand; it never rides the data classes
([04-planes.md R3](04-planes.md)). Replies are `Vec<Record>` of the
registered reply type.

**Write** — instructions with immediate effect (`set`, `apply`, `trigger`).
The GET carries the instruction as its payload. **A value reply always
means success; a failure always rides Zenoh's reply-error channel**
(`reply_err`) — never a success payload carrying `ok: false` (the D-Bus
guideline verbatim: "a reply always indicates success, and an error always
indicates failure"; [10-prior-art.md](10-prior-art.md)). The error payload
is:

```
{ "error": "<name>", "message": "<human text>" }
```

where `<name>` is machine-readable and namespaced like a key. The
convention reserves `error/invalid-args`, `error/unauthorized`,
`error/not-found`, `error/unsupported`, `error/busy`, `error/gated`;
producer-specific names live under `error/<producer>/…` and are registered
like subjects — deprecate-never-reuse applies
([08-registry.md](08-registry.md)). A successful write replies with an
empty or result-bearing value. (Envelopes are shown as JSON for
readability; the wire encoding is the deployment's payload default,
CBOR in the reference application.)

A write procedure MUST reply — so for a write, *silence is never a
refusal* (refusals are error replies; see §3.1 for what silence does
mean). Idempotency is per-procedure and MUST be documented in the registry
entry. Gated/dangerous writes (service actions) keep their gate at the
server (allowlist/polkit) and refuse with `error/gated`; the convention
adds ACL-by-prefix as an outer layer, since `…/h-xxx/@rpc/systemd/action`
is a literal key an ACL can deny per client.

**Long-running** — anything that outlives a query timeout (artifact
generation, captures). The pattern is *RPC to initiate, state to observe*:

1. `GET …/@rpc/<producer>/artifact/request` (body: kind + options) →
   `{ id }` immediately (or an error reply);
2. progress is ordinary observable state:
   `…/state/<producer>/artifact/<kind>` (LWW status document —
   generating/ready/failed/expired, tombstoned when freed);
3. completion may additionally emit an `events` record
   (`…/events/<producer>/artifact/<ulid>`) for the audit trail;
4. the bytes are pulled from `@blob` ([07-bulk-planes.md](07-bulk-planes.md));
5. `GET …/@rpc/<producer>/artifact/cancel?id=<ulid>` frees early.

This replaces the incumbent trio of a pub/sub request key, a status
queryable, and a cancel subscriber with one uniform mechanism — and makes
progress visible to *every* observer (it is state), not only the requester.

**No durable commands.** RPC is synchronous-ish: an offline host misses the
call, and the caller can *determine* that it did (§3.1). If a deployment
ever needs "instruction that survives producer downtime", the escape hatch
is desired-state reconciliation — the controller publishes
`state/<producer>/desired/<topic>` and the producer converges on
(re)connect. No current channel needs it; recorded in
[12-open-questions.md §3](12-open-questions.md).

### 3.1 What silence means (normative honesty)

"No reply" is not one condition. With plain GET semantics it conflates:

| Cause | Observable behavior |
|---|---|
| no queryable matches (offline host whose session expired, mistyped origin, procedure not served) | the query finalizes **empty, fast** |
| host reachable but session dying / mid-boot before queryable declaration | empty at the **query timeout** (default 10 s) |

Callers therefore MUST NOT treat "empty reply set" as a verdict about any
specific host. The discipline that makes silence attributable:

- producers declare `@rpc` queryables **before** their `alive` liveliness
  token ([04-planes.md §5](04-planes.md)), so *alive ⇒ callable* — a host
  that is alive on the roster but silent on RPC is a bug, not a boot race;
- callers consult the liveliness roster to classify: not on roster =
  offline/unenrolled; on roster + error reply = refused; on roster + no
  reply within timeout = investigate.

## 4. Late-joiner seeds are state, not RPC

The incumbent keyspace used queryables to seed late joiners (firing alerts,
stream catalogues, entity sets). Under this convention those seeds are
unnecessary as separate endpoints: the data *is* `state`, and a late joiner
seeds from the same keys it will then watch. A dedicated `query/alerts`
procedure would duplicate the state selector. RPC is reserved for what
state cannot express: parameterised, high-cardinality, or computed replies.

Two normative disciplines make the seed correct (they are part of the
delivery contract, [04-planes.md §3.1–3.2](04-planes.md); stated here
because this is where the incumbent's seed queryables dissolve):

- **Subscribe first, reconcile by timestamp.** A consumer MUST declare its
  subscriber *before* issuing the seed GET, and MUST merge seed replies
  with live samples per key by Zenoh (HLC) timestamp — newer value wins,
  newer tombstone wins. GET-then-subscribe is forbidden: a transition
  published in the gap is silently dropped, and a dropped delete is a
  resurrected key. Corollary: all state publishers and storages MUST run
  with timestamping enabled — an untimestamped sample cannot be
  reconciled.
- **There are exactly two seed paths, and they answer from different
  places.** (1) An **AdvancedSubscriber with `history()`** (the advanced
  tier, [04-planes.md §3.3](04-planes.md)) seeds from live publishers'
  `@adv` caches — no router storage needed, reconcile done internally.
  (2) A **plain GET on the state selector** (e.g.
  `<base>/@v1/*/state/*/alert/*`) is answered only by a router **storage**
  ([09-operations.md §2](09-operations.md)) — publisher caches live under
  the verbatim `@adv` sidecar that a plain GET cannot reach. They differ
  in *coverage*, not just mechanism: a cache dies with its publisher, a
  storage does not. So a consumer whose correctness depends on state from
  **crashed** producers (a UI rendering the firing alert of a host that
  died — exactly the case [04 §1.2](04-planes.md)'s TTL retirement
  exists for) MUST include the storage seed where one is deployed; cache
  seeding alone suffices only where dead producers' state may lapse until
  TTL. And composition is well-defined: the AdvancedSubscriber's own
  declare-time history query is internally race-free, so a consumer
  running both paths issues the storage GET first (or concurrently),
  declares the AdvancedSubscriber last on the session
  ([04 §3.3](04-planes.md)'s ordering note), and merges everything by the
  same timestamp rule — re-issuing the seed GET once after the declare if
  the fleet also contains baseline (cache-less) publishers, whose
  gap-window transitions no history query can replay. What no consumer may
  do is assume a plain GET reaches publisher caches or that `history()`
  reaches storages.

## 5. Mapping the incumbent channels

Reference-application mapping (normative for its migration, illustrative
for other adopters). `P` = the producer chunk.

| Incumbent key (protocol- or host-scoped) | Convention location |
|---|---|
| `…/@/commands/<topic>` + `…/@/status/<topic>` | `@rpc/P/<topic>/set` (write, ack reply) + `@rpc/P/<topic>` (read current) |
| `…/@/query/<topic>` | `@rpc/P/<topic>` (read) |
| `…/@/query/alerts` (firing seed) | GET on `state/*/alert/*` selector (§4) |
| `…/@/artifact/request` (pub/sub) | `@rpc/P/artifact/request` (write → `{id}` or error reply) |
| `…/@/artifact/status` (queryable) | `state/P/artifact/<kind>` (observable LWW status) |
| `…/@/artifact/cancel` (pub/sub) | `@rpc/P/artifact/cancel?id=` (write) |
| `…/@/artifact/blob/<id>/**`, `…/@/store/**`, `…/@/tree/**` | `@blob/…` ([07-bulk-planes.md](07-bulk-planes.md)) |
| logs `@/query/events?since=;max=;host=` | `@rpc/logs/events?since=;max=;source=` (`source=` filters the *observed* device — a centralized syslog receiver holds many sources' lines; origin targeting selects the receiver, not the line's source) |
| netlink `@/commands/expectations` | `@rpc/netlink/expectations/set` + read at `@rpc/netlink/expectations` |
| netring `@/commands/capture_disk` (`capture_now`) | `@rpc/netring/capture/trigger` (write) + `state/netring/capture` (mode/occupancy) + `events/netring/capture/<ulid>` |
| systemd `@/commands/action` (gated) | `@rpc/systemd/action` (write; gate unchanged, plus per-key ACL) |
| parallax `@/commands/stream` (`OpenStream`…) | `@rpc/parallax/stream/open`, `…/stream/close`, `…/stream/keyframe` (writes) |
| parallax `@/query/streams`, `@/status/streams` | `state/parallax/stream/<stream>` (catalogue + status as LWW docs; a closed stream keeps its doc with `open: false` — tombstone on *removal from config*, not on close, or the UI loses the "openable streams" catalogue) |

The generic first row covers, by name, every shipped config-style topic not
listed individually: logs `filter` → `@rpc/logs/filter/set` + read at
`@rpc/logs/filter`; netlink `collection` → `@rpc/netlink/collection/set`;
netring `detectors`, `capture_filter`, `threat_intel` →
`@rpc/netring/<topic>/set` — each with the read procedure at the same key
minus `/set`.

Two systematic effects of the mapping:

- **Every host-vs-protocol scoping asymmetry disappears.** sysinfo's
  host-scoped queries and netlink's protocol-scoped ones become the same
  shape; "which scope does this channel use?" is no longer a question the
  consumer can get wrong.
- **Status keys stop being a third mechanism.** What was
  command/status/query triples becomes: writes (RPC), reads (RPC), and
  observable state — each in its native plane.
