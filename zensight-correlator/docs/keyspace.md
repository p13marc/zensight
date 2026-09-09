# Correlator keyspace

The keys the catalog (correlator) consumes and produces. The deployed fleet-wide
profile lives in [`../../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) (normative
spec: [`../../docs/rfcs/keyspace-v2/`](../../docs/rfcs/keyspace-v2/00-index.md));
this page is the correlator-scoped slice. All key builders are in
`zensight-common/src/keyexpr.rs`.

The catalog never touches the telemetry firehose
(`zensight/v1/*/telemetry/**`). It consumes the fleet **state plane** and is the
single writer of the verbatim `@catalog` origin (`zensight/v1/@catalog/…`).

## Consumes (subscriptions)

| Key | Payload | Notes |
|-----|---------|-------|
| `zensight/v1/*/state/*/evidence/**` | `HostEvidence` **or** `RelationshipEvidence` | One subscription, three families. Routed **on the refined subject**, never on a key substring: `…/evidence/self` and `…/evidence/device/<device>` are host identity, `…/evidence/relation/<id>` is graph input (#915), `…/evidence/names/*` has its own subscriber below. `AdvancedSubscriber` with `history()` + late-publisher detection, so a fresh catalog immediately gets the sensors' cached claims. A `DELETE` on a claim key drops the claim (an evidence tombstone). |
| `zensight/v1/*/state/*/evidence/names/*` | `NameObservation` | Passive-DNS, one key per observed IP (`.`/`:` → `-`), last-writer-wins on the wire; accumulated in the name store. Own `AdvancedSubscriber` (the host-evidence subscriber skips the `names/` subtree, handled here). |
| `zensight/v1/*/state/*/device/*/liveness` | `DeviceLiveness` | Plain subscriber (no history). Rolled up onto `entity.status`. Gated by `status_from_liveness`; skipped entirely when off. |

Wildcards used: `all_evidence_wildcard()`, `all_name_evidence_wildcard()`,
`all_liveness_wildcard()`.

**Relationship claims need no new subscription**, and that is worth stating because it is
also a hazard. `all_evidence_wildcard()` is `v1/*/state/*/evidence/**` — a hand-spelled
union of three `CommonFamily` selectors — so a fourth family under `evidence/**` is
delivered to a handler written when the subtree was entirely host identity. `HostEvidence`
carries no `deny_unknown_fields` and requires only `sensor` and `source`, both of which a
relationship claim has, so it would have decoded cleanly and entered the identity
union-find with no error and no log line. The dispatch is now an **allow-list of subjects**,
so the next family added here is inert by default. (Raised as zenkey#416; now normative — RFC 06
§4 v1.30, shipped in zenkey 0.8, which is also where `common = "evidence_relation"` comes from.)

## Produces (publications)

| Key | Payload | Notes |
|-----|---------|-------|
| `zensight/v1/@catalog/state/entity/<entity_id>` | `HostEntity` | The merged entity view; `<entity_id>` is `h-<12hex>` (the origin id when a member has a `host_id`). The catalog is the **single writer**. A `PUT` upserts (cached plain publisher per id, reliable + block); a `DELETE` tombstones a retired entity. Re-emitted every `reemit_secs`. |
| `zensight/v1/@catalog/state/edge/<edge_id>` | `Edge` | The resolved relationship graph (#917). Same lifecycle as `entity/`: cached plain publisher per id, reliable + block, `DELETE` as tombstone, re-emitted every `reemit_secs`. `<edge_id>` is `e-<16hex>` = `fnv1a_64(kind ‖ from ‖ to)` computed **after** resolution, so a refresh is an idempotent overwrite and two sensors seeing one relationship land on one key. |
| `zensight/v1/@catalog/state/pdns/<ip-slug>` | `PdnsRecord` | Historical passive-DNS: an IP's full accumulated name set, published on every name-store update for that IP. Plain `session.put` (the IP set is unbounded), reliable + block. Meant to be captured by a storage backend, not consumed live — see [`storage.md`](storage.md). |

## Queryables (late-joiner seed / on-demand)

| Key | Selector | Reply |
|-----|----------|-------|
| `zensight/v1/@catalog/state/entity/*` | — | The entity seed IS the state selector: a late-joining frontend plain-GETs it on connect and the catalog answers **storage-shaped** — one JSON `HostEntity` reply per entity, each on its concrete state key. |
| `zensight/v1/@catalog/state/edge/*` | — | The edge seed, storage-shaped like the entity seed: one `Edge` reply per edge on its concrete key. Without it a late-joining consumer sees a blank graph until something in the fleet's topology *changes* — which, with the change gate doing its job, may be a long time and is supposed to be. |
| `zensight/v1/@catalog/@rpc/names` | `?ip=<addr>` | JSON `Vec<NameVal>` — up to 32 accumulated names for that IP. An `@rpc` procedure: resolves arbitrary/external IPs on demand instead of flooding the bus; a missing/blank `ip` replies with an empty set. |

## Liveliness (catalog ownership)

Ownership is an explicit claim protocol
(`zensight_common::service_guard`, RFC 06 §5.3): every candidate declares a
liveliness claim token at `zensight/v1/@catalog/state/claim/<zid>`, then asks
two questions in order.

1. **Is there a live incumbent?** — i.e. does anyone hold
   `zensight/v1/@catalog/state/alive`. If so, stand by, whatever the zids say.
2. **Otherwise, who sorts first?** — query the claim set (`…/state/claim/*`);
   the lexically-lowest chunk wins. Deterministic and coordinator-free, so
   simultaneous starts converge without messages.

A loser **stands by**, polling until the owner's presence is gone, and then
campaigns again — it does not exit, which used to mean that killing the owner
left no catalog until a supervisor restarted a loser whose zid sorted right.

An **unreadable claim set is not sole candidacy** (#1105). A query that times
out used to log "assuming sole candidate" and make the caller a winner, so a
slow bus elected everybody; a campaign now retries, treats an answer that does
not contain its own claim as no answer, and stands by rather than guessing —
which is self-healing, because the standby loop campaigns again.

Only the elected owner declares `zensight/v1/@catalog/state/alive` and the
catalog publishers/queryables. `@desired` runs the identical protocol on its own
claim space (`zensight/v1/@desired/state/claim/*`).

## Why the pdns tier is off the firehose

`@catalog` is an `@`-verbatim chunk, so
`zensight/v1/@catalog/state/pdns/<ip-slug>` is invisible to the telemetry class
selector (`zensight/v1/*/telemetry/**`) **and** to the `*`-origin fleet state
selector (`zensight/v1/*/state/**`) — `*` never matches a verbatim chunk. Only
the dedicated selector (`all_pdns_wildcard()` =
`zensight/v1/@catalog/state/pdns/**`) captures it. A regression test in
`zensight-common` pins this.
