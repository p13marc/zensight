# 06 — Identity, Origins, and the Catalog

**Status: Draft** · normative chapter

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

`h-<12hex>`, reference derivation `h-` + first 12 hex of
`sha256(machine-id + application salt)`:

- **Self-minted**: derivable at first startup with no coordinator. Every
  publisher on one machine derives the same value — so all producers of a
  host agree on their origin without talking to each other.
- **Stable**: survives restarts, upgrades, renames, re-addressing. A
  hostname change is a catalog event, not a re-key.
- **Opaque**: reveals nothing (the salt keeps the machine-id private) and
  promises nothing — it is an *address*, not a description. What the origin
  *is* (its names, addresses, roles, kind) lives in catalog documents and
  can be corrected freely ([02-principles.md P8](02-principles.md)).
- **Salt scope**: the salt is per-application-domain and fixed for a
  deployment's lifetime; changing it re-keys the fleet (it is part of the
  identity function, and MUST be treated as such).

### 1.1 Hosts without a machine-id

Constrained or ephemeral hosts (containers, RTOS nodes) MUST still mint a
stable id, in order of preference:

1. a persisted random id, generated once and stored locally
   (`h-` + 12 hex of a random 64-bit value);
2. a hash of the most stable hardware identity available (primary MAC,
   serial), same truncation.

The requirement is stability + uniqueness, not provenance; the catalog's
evidence model (below) absorbs the difference in confidence.

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
zensight/@v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
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
  devices is asking for a different trust model, recorded in
  [12-open-questions.md §2](12-open-questions.md).

## 4. Evidence — identity claims as ordinary state

Identity evidence is not a separate metadata plane (the incumbent
`_meta/evidence/**`); it is ordinary per-origin `state`, because that is
what it is — a producer's current claim, refreshed on a cadence, stale when
unrefreshed:

| Key | Claim |
|---|---|
| `<base>/@v1/<origin>/state/<producer>/evidence/self` | "my host is: hostname H, machine-id-hash M, addresses A…" (self-report) |
| `<base>/@v1/<origin>/state/<producer>/evidence/device/<device>` | "device `<device>` I observe has: sysName, MACs, addresses…" (third-party claim, weighted lower) |
| `<base>/@v1/<origin>/state/<producer>/evidence/names/<ip-slug>` | "IP X currently resolves to name N" (passive DNS observation) |

- Claims carry `last_updated`; consumers (the catalog) MUST ignore claims
  older than the evidence TTL, and publishers MUST refresh live claims at
  ≤ TTL/2. Evidence ages out; it never binds forever.
- The catalog subscribes to one selector:
  `<base>/@v1/*/state/*/evidence/**`.

## 5. The `@catalog` service

`@catalog` is the reserved service origin ([03-grammar.md §3](03-grammar.md))
for the deployment's identity/ontology service (the reference
implementation: `zensight-correlator`).

```
<base>/@v1/@catalog/state/entity/<entity-id>      merged entity document (LWW, tombstoned on retire/merge)
<base>/@v1/@catalog/state/alias/<old-id>          alias record: old-id → entity-id (id upgrades, merges)
<base>/@v1/@catalog/state/pdns/<ip-slug>          accumulated IP↔name record (historical tier via storage)
<base>/@v1/@catalog/state/alive                   liveliness token (single-writer guard)
<base>/@v1/@catalog/@rpc/names                    on-demand name resolution (?ip=…)
```

Contract:

- **Single writer.** Exactly one catalog instance publishes under
  `@catalog`; the liveliness token is the guard (a second instance detects
  it and exits). Everything under `@catalog` is a *conclusion*; conclusions
  have one author.
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

1. Subscribe `<base>/@v1/@catalog/state/entity/*` (+ GET the same selector
   as the late-joiner seed, [05-control-rpc.md §4](05-control-rpc.md)).
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
`<base>/@v1/@catalog/state/pdns/**` captures every transition — the
IP↔name history — with no dedicated plane and no consumer on the live bus
([04-planes.md §4](04-planes.md)). The verbatim `@catalog` origin keeps
this unbounded-cardinality stream structurally out of every fleet selector
(design property D4).
