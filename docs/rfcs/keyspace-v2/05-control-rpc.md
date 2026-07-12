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

## 3. Read, write, and long-running procedures

Three procedure idioms, distinguished in the registry, all on the same key
shape:

**Read** — idempotent detail queries. High-cardinality data (processes,
sockets, flows, log lines) is held in bounded rings at the producer and
served on demand; it never rides the data classes
([04-planes.md R3](04-planes.md)). Replies are `Vec<Record>` of the
registered reply type.

**Write** — instructions with immediate effect (`set`, `apply`, `trigger`).
The GET carries the instruction as its payload; the reply is an
acknowledgement envelope:

```
{ "ok": true }                              — accepted/applied
{ "ok": false, "error": "<reason>" }        — rejected (authz, validation, unsupported)
```

A write procedure MUST reply — silence is a transport failure, not a
refusal. Idempotency is per-procedure and MUST be documented in the
registry entry. Gated/dangerous writes (service actions) keep their gate at
the server (allowlist/polkit); the convention adds ACL-by-prefix as an
outer layer, since `…/h-xxx/@rpc/systemd/action` is a literal key an ACL
can deny per client.

**Long-running** — anything that outlives a query timeout (artifact
generation, captures). The pattern is *RPC to initiate, state to observe*:

1. `GET …/@rpc/<producer>/artifact/request` (body: kind + options) →
   `{ ok, id }` immediately;
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
call, and the caller *knows* (no reply). If a deployment ever needs
"instruction that survives producer downtime", the escape hatch is
desired-state reconciliation — the controller publishes
`state/<producer>/desired/<topic>` and the producer converges on
(re)connect. No current channel needs it; recorded in
[12-open-questions.md §3](12-open-questions.md).

## 4. Late-joiner seeds are state, not RPC

The incumbent keyspace used queryables to seed late joiners (firing alerts,
stream catalogues, entity sets). Under this convention those seeds are
unnecessary as separate endpoints: the data *is* `state`, and latest-value
delivery (publisher-side cache / storage-backed query on the state
selector) seeds any late joiner from the same keys it will then watch.
A GET on a **state selector** (e.g. `<base>/@v1/*/state/*/alert/*`) is the
seed; a dedicated `query/alerts` procedure would duplicate it. RPC is
reserved for what state cannot express: parameterised, high-cardinality,
or computed replies.

## 5. Mapping the incumbent channels

Reference-application mapping (normative for its migration, illustrative
for other adopters). `P` = the producer chunk.

| Incumbent key (protocol- or host-scoped) | Convention location |
|---|---|
| `…/@/commands/<topic>` + `…/@/status/<topic>` | `@rpc/P/<topic>/set` (write, ack reply) + `@rpc/P/<topic>` (read current) |
| `…/@/query/<topic>` | `@rpc/P/<topic>` (read) |
| `…/@/query/alerts` (firing seed) | GET on `state/*/alert/*` selector (§4) |
| `…/@/artifact/request` (pub/sub) | `@rpc/P/artifact/request` (write → `{ok,id}`) |
| `…/@/artifact/status` (queryable) | `state/P/artifact/<kind>` (observable LWW status) |
| `…/@/artifact/cancel` (pub/sub) | `@rpc/P/artifact/cancel?id=` (write) |
| `…/@/artifact/blob/<id>/**`, `…/@/store/**`, `…/@/tree/**` | `@blob/…` ([07-bulk-planes.md](07-bulk-planes.md)) |
| logs `@/query/events?since=;max=;host=` | `@rpc/logs/events?since=;max=` (host= obsolete — origin targets it) |
| netlink `@/commands/expectations` | `@rpc/netlink/expectations/set` + read at `@rpc/netlink/expectations` |
| netring `@/commands/capture_disk` (`capture_now`) | `@rpc/netring/capture/trigger` (write) + `state/netring/capture` (mode/occupancy) + `events/netring/capture/<ulid>` |
| systemd `@/commands/action` (gated) | `@rpc/systemd/action` (write; gate unchanged, plus per-key ACL) |
| parallax `@/commands/stream` (`OpenStream`…) | `@rpc/parallax/stream/open`, `…/stream/close`, `…/stream/keyframe` (writes) |
| parallax `@/query/streams`, `@/status/streams` | `state/parallax/stream/<stream>` (catalogue + status as LWW docs, tombstone on close) |

Two systematic effects of the mapping:

- **Every host-vs-protocol scoping asymmetry disappears.** sysinfo's
  host-scoped queries and netlink's protocol-scoped ones become the same
  shape; "which scope does this channel use?" is no longer a question the
  consumer can get wrong.
- **Status keys stop being a third mechanism.** What was
  command/status/query triples becomes: writes (RPC), reads (RPC), and
  observable state — each in its native plane.
