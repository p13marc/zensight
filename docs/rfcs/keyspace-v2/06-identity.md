# 06 — Identity, Origins, and the Catalog

**Status: v1.0 (ratified)** · normative chapter

The grammar puts a stable identity in every key (the origin chunk,
[03-grammar.md §1.3](03-grammar.md)). This chapter defines how that identity
is minted, how observed (proxied) devices are named, and the contract of the
`@catalog` service that turns per-origin claims into merged entities.

The design resolves the tension every identity-in-key scheme faces:

> A publisher must know its key at first startup, alone. But the *correct*
> entity identity (this modem and that hostname are the same box) is only
> knowable later, centrally, from accumulated evidence.

The resolution: **origins are self-minted and never re-keyed; the catalog
maps, it does not rename.**

---

## 1. Host origins — self-minted, stable, opaque

`h-<12hex>`. Reference derivation, byte-precise (two independent
implementations MUST mint the same id for the same machine):

```
input   = machine_id_hex ++ salt
machine_id_hex = the 32 lowercase-hex chars of /etc/machine-id,
                 whitespace/newline trimmed
salt    = the application salt, as UTF-8, no separator
origin  = "h-" ++ lowercase_hex(sha256(input))[0..12]
```

Test vector: machine-id `b642b4217b34b1e8d3bd915fc65c4452`, salt
`example-salt-v1` → `h-` + first 12 hex of
`sha256("b642b4217b34b1e8d3bd915fc65c4452example-salt-v1")` =
`h-20609002f7b6` (implementations MUST reproduce this).

- **Self-minted**: derivable at first startup with no coordinator. Every
  publisher on one machine derives the same value — so all producers of a
  host agree on their origin without talking to each other.
- **Stable**: survives restarts, upgrades, renames, re-addressing. A
  hostname change is a catalog event, not a re-key. (A **machine-id**
  change is a new host — see §5.4.)
- **Opaque**: reveals nothing (the salt keeps the machine-id private) and
  promises nothing — it is an *address*, not a description. What the origin
  *is* (its names, addresses, roles, kind) lives in catalog documents and
  can be corrected freely ([02-principles.md P8](02-principles.md)).
- **Salt scope**: the salt is an **application constant** — fixed at
  application level, identical across every deployment of that application,
  compiled in, not operator-configurable (the reference application ships
  `"zensight-host-id-v1"` as a non-configurable constant). This is what
  makes origin id ≡ entity id hold across a fleet without coordination.
  Changing the constant is an application-breaking change that re-keys
  every fleet; it is part of the identity function and MUST be treated as
  such. (A deployment-chosen salt would also work but breaks id
  portability between deployments of one application; an application MUST
  pick one model and document it.)
- **Collisions**: 48 bits of id means truncation collisions are possible
  in principle (birthday: ≈ 1.8 × 10⁻⁹ at 1 k hosts, 1.8 × 10⁻⁵ at 100 k,
  0.18 % at 1 M). The convention *detects* rather than prevents: the
  catalog MUST raise an operator-visible conflict when disjoint
  `evidence/self` claims (different machine-id hashes, different
  hostnames) persist under one origin — that signature has exactly two
  causes, id collision or origin spoofing, and both demand an operator.

### 1.1 Hosts without a machine-id

Constrained or ephemeral hosts (containers, RTOS nodes) MUST still mint a
stable id, in order of preference:

1. a persisted random id: 6 random bytes rendered as 12 hex, generated
   once, written **atomically** to an application-defined well-known local
   path (write-temp + rename), and read by every producer thereafter.
   Because this is a shared file, not a derivation, the
   all-producers-agree invariant needs the file: producers racing at first
   boot MUST create it with an atomic create-exclusive (loser re-reads);
2. a hash of the most stable hardware identity available (primary MAC,
   serial), derived as in §1 with the hardware id in place of the
   machine-id.

The requirement is stability + uniqueness, not provenance; the catalog's
evidence model (below) absorbs the difference in confidence. Note the
"same value on one machine" invariant of §1 holds *by derivation* only for
machine-id (and option-2 hardware) hosts; option-1 hosts get it from the
shared file.

## 2. Origins vs entities — the two-layer contract

- The **origin** is the *publisher's* identity claim: "these keys come from
  the same place." It is in the key because routing, ACL, and grouping need
  it before any correlation exists.
- The **entity** is the *deployment's* identity conclusion: "these origins
  and these observed devices are one host." It is computed by the catalog
  from evidence and can change as evidence improves — which is exactly why
  it MUST NOT be in data keys.

When the machine-id is known, the reference derivation makes the origin id
and the entity id **the same value** — the common case needs no mapping at
all. When identities merge or upgrade (a weakly-identified origin later
proves to be an already-known machine), the catalog publishes an alias, and
consumers re-group; no publisher re-keys, no history is orphaned.

## 3. Observed devices are subjects, not origins

A proxy producer (SNMP poller, Modbus master, gNMI collector, NetFlow
receiver) speaks *about* other devices. Those devices go in the **first
subject chunk**, never in the origin:

```
zensight/v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
                            ^ the poller's host      ^ the observed device
```

- The origin answers "who do I trust / throttle / ACL" — that is the
  machine running the producer, not the router it polls.
- The device chunk is the producer's local name for the device (config
  name, slugged). Cross-producer device identity (the same router polled
  from two collectors) is — like all identity — a catalog conclusion, fed
  by observed-evidence claims (§4).
- A device MAY be promoted to a first-class origin only by *running a
  publisher itself*; a deployment that wants per-device ACL on proxied
  devices is asking for a different trust model — decided against, with
  the revisit trigger recorded in
  [12-open-questions.md §2](12-open-questions.md).

## 4. Evidence — identity claims as ordinary state

Identity evidence is not a separate metadata plane (the incumbent
`_meta/evidence/**`); it is ordinary per-origin `state`, because that is
what it is — a producer's current claim, refreshed on a cadence, stale when
unrefreshed:

| Key | Claim |
|---|---|
| `<base>/v1/<origin>/state/<producer>/evidence/self` | "my host is: hostname H, machine-id-hash M, addresses A…" (self-report) |
| `<base>/v1/<origin>/state/<producer>/evidence/device/<device>` | "device `<device>` I observe has: sysName, MACs, addresses…" (third-party claim, weighted lower) |
| `<base>/v1/<origin>/state/<producer>/evidence/names/<ip-slug>` | "IP X currently resolves to name N" (passive DNS observation) |

- Claims carry `last_updated`; every consumer of evidence (the catalog
  first among them) MUST ignore claims older than the subject's registry
  TTL, and publishers MUST refresh live claims at ≤ TTL/2 — the TTL value
  is the registry's, authoritative for both sides
  ([04-planes.md §1.2](04-planes.md); the reference registry sets 900 s).
- `evidence/names/<ip-slug>` is **population-keyed state** and carries the
  mandatory cardinality budget of [04-planes.md §1.2](04-planes.md): the
  registry entry declares the expected population bound, and the publisher
  tombstones entries whose observation has aged past TTL. An
  internet-facing sensor MUST aggregate or sample before publishing — the
  per-IP key family is for the *actively observed* set, not for every
  address ever seen (that history belongs to the catalog's storage tier,
  §5.2).
- The catalog subscribes to one selector:
  `<base>/v1/*/state/*/evidence/**`.

## 5. The `@catalog` service

`@catalog` is the reserved service origin ([03-grammar.md §3](03-grammar.md))
for the deployment's identity/ontology service (the reference
implementation: `zensight-correlator`).

```
<base>/v1/@catalog/state/entity/<entity-id>      merged entity document (LWW, tombstoned on retire/merge)
<base>/v1/@catalog/state/alias/<old-id>          alias record: old-id → entity-id (id upgrades, merges)
<base>/v1/@catalog/state/pdns/<ip-slug>          accumulated IP↔name record (historical tier via storage)
<base>/v1/@catalog/state/alive                   liveliness token (declared by the elected owner, §5.3)
<base>/v1/@catalog/state/claim/<zid>             liveliness claim tokens (ownership protocol, §5.3)
<base>/v1/@catalog/@rpc/names                    on-demand name resolution (?ip=…)
```

Contract:

- **Single writer.** Exactly one catalog instance publishes under
  `@catalog`, arbitrated by the ownership protocol of §5.3. Everything
  under `@catalog` is a *conclusion*; conclusions have one author.
- **Pure function of live evidence.** The entity set is recomputed from the
  current evidence state (union-find over ranked identity rules — strong
  ids join, weak ids like bare IP/MAC never join alone, conflicting strong
  ids block a merge). A restarted catalog reseeds from evidence and reaches
  the same conclusions: no private database, no migration state.
- **Stable entity ids, aliases on upgrade.** `entity-id` = the machine-id
  hash form when known (== the host origin id, §2), else derived from the
  best available evidence. When an entity's id upgrades or two entities
  merge, the losing id gets an `alias/<old-id>` record and its entity
  document a tombstone; consumers re-point. Ids never round-robin.
- **Consumers join, producers don't wait.** Nothing a producer publishes
  depends on the catalog; if it is down, entities go stale and consumers
  degrade to grouping by raw origin — the same data, one join weaker.
- **Kinds, names, roles, relationships live here** — in entity documents —
  and nowhere in any key ([03-grammar.md §6.2](03-grammar.md), the
  ontology-in-key rejection).

### 5.1 How a UI joins

1. Subscribe `<base>/v1/@catalog/state/entity/*` **and**
   `<base>/v1/@catalog/state/alias/*` (+ GET the same selectors as the
   late-joiner seed, [04-planes.md §3.2](04-planes.md)) — alias
   records are their own key family, and without them step 3's
   origin→entity re-pointing on merges never arrives.
2. Group data keys by their origin chunk — a plain string read at
   position 3, no parsing heuristics.
3. `entity.origins[]` (and `alias` records) map origin → entity;
   unmatched origins render as bare hosts until evidence catches up.
4. Names for arbitrary external IPs (every CDN the flow sensor ever saw)
   are pulled on demand: `GET …/@catalog/@rpc/names?ip=…` — never
   broadcast.

### 5.2 The historical tier is a storage choice

`state/pdns/<ip-slug>` is LWW state (the latest accumulated name-set per
IP). Pointing a time-series storage at
`<base>/v1/@catalog/state/pdns/**` captures every transition — the
IP↔name history — with no dedicated plane and no consumer on the live bus
([04-planes.md §4](04-planes.md)). This is the catalog's *budgeted*
population-keyed state family ([04-planes.md §1.2](04-planes.md)) — the
one place per-IP keys are the design, aged and tombstoned by the catalog —
and the verbatim `@catalog` origin keeps it structurally out of every
fleet selector (design property D4).

### 5.3 Ownership protocol

Zenoh has no name-ownership primitive (a liveliness token is presence, not
a lock — two sessions can hold the same key). Ownership of a service
origin is therefore arbitrated by an explicit claim protocol, modelled on
D-Bus well-known-name ownership ([10-prior-art.md](10-prior-art.md)):

1. **Claim.** Each candidate declares a liveliness token at
   `…/@catalog/state/claim/<zid>` (its own Zenoh session id, lowercased),
   then queries liveliness on `…/state/claim/*`.
2. **Election.** The owner is the candidate whose claim chunk sorts
   lexically lowest — deterministic and coordinator-free; every candidate
   computes the same winner from the same token set, so simultaneous
   starts converge without messages.
3. **Standby.** Non-owners MAY keep their claim declared and idle
   (D-Bus `IN_QUEUE`), watching a liveliness subscriber on
   `…/state/claim/*` (the `NameOwnerChanged` analog). When the owner's
   claim retracts — crash, shutdown, disconnect — each standby re-runs
   step 2. A candidate that would rather exit than queue undeclares its
   claim and leaves.
4. Only the owner declares `…/state/alive`, the `@catalog` publishers, and
   the `@rpc/names` queryable; a standby declares nothing but its claim.

**What this does and does not guarantee.** On a connected network:
exactly one owner, automatic failover. It is *not* mutual exclusion:
during a **partition**, each side elects its own owner and both write; a
**deposed or paused ex-owner** learns of its loss asynchronously (no
fencing) and its buffered writes can land after the new owner's.
Mitigations, in order of force: every `@catalog` document carries the
writer's claim id and an `elected_at` incarnation timestamp, so dual
authorship is *detectable* (consumers and storages alert on incarnation
regress); and the pure-function contract above makes split-brain
*convergent* — after heal, the surviving owner's next full recompute over
the merged evidence overwrites interleaved conclusions. The convention
accepts **eventual** single-writer, not linearizable single-writer, and
says so.

### 5.4 Reinstall — machine-id change

A reinstall (new machine-id, same hardware) is neither an id upgrade nor a
merge: the host mints a *new* origin while evidence for the old one may
still be live, and the "conflicting strong ids block a merge" rule then
correctly *prevents* automatic linking — the catalog cannot know a
reinstall from two machines. The convention is honest about the default:
**a machine-id change is a new host**; after the old origin's evidence
ages out, its entity document is tombstoned and its history remains under
the old origin, reachable only via storage.

Deployments that want continuity assert it explicitly:
`GET …/@catalog/@rpc/link?old=<id>;new=<id>` (operator-invoked, gated) —
the catalog records the assertion as operator evidence (strong, does not
age out), publishes `alias/<old-id>`, and merges. Retention differs by
record kind: an **alias** is an ordinary put and persists in the
latest-value storage until an operator retires it (`@rpc/unlink`), with
catalog-side GC once unresolved for a deployment-configured horizon; an
entity **tombstone** is storage *metadata* and lives exactly as long as
the storage's `garbage_collection.lifespan`
([09-operations.md §2.3](09-operations.md)) — deployments running
replicated catalog storages SHOULD size that lifespan to their
partition-heal horizon, because a pruned tombstone is what lets a slow
replica resurrect a merged-away entity.

