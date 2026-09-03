# zensight-correlator

Headless ZenSight service — **the catalog** — that fuses per-sensor **identity
evidence** into one `HostEntity` per physical host. Sensors self-report a stable
`host_id` (and, with `evidence` on, republish the hosts and names they observe);
the catalog is the **single writer** of the verbatim `@catalog` origin
(`zensight/v1/@catalog/…`) — it subscribes only to the evidence state
(`zensight/v1/*/state/*/evidence/**`, never the telemetry firehose), merges
claims with a deterministic union-find over ranked identity rules, and publishes
the materialized entity view plus storage-shaped seed queryables.

One instance per fleet. Deployed like an exporter. Holds no database: it rebuilds
identical state on restart from the sensors' cached self-reports (an optional
router-hosted storage adds durability — see [`docs/storage.md`](docs/storage.md)).

## Quick start

```bash
# Fuse real sensor evidence into HostEntity docs
cargo run -p zensight-correlator --release -- --config configs/correlator.json5

# Drive the GUI host view with synthetic evidence (no sensors needed)
cargo run -p zensight-correlator --release -- --config configs/correlator.json5 --demo
```

Run with no `--config` to use built-in defaults (peer mode, 900 s evidence TTL,
60 s re-emit). `--demo` feeds a fixed, deterministic evidence set through the same
engine/store/publisher pipeline, so the frontend can develop against a live
correlator without any sensors.

Catalog ownership is an explicit claim protocol (`guard.rs`): every candidate
declares a liveliness claim at `zensight/v1/@catalog/state/claim/<zid>`, the
lexically-lowest claim wins the election, and losers exit rather than
double-write. Only the elected owner declares `…/@catalog/state/alive` and the
catalog publishers/queryables (deterministic merge means a partition-split pair
would emit identical docs, so this is a safety net, not a lock).

## The topology graph (#899)

Beside identity, the catalog resolves **relationships**. Sensors publish claims on
`state/<producer>/evidence/relation/{relation_id}` — pve a `Hosts` per guest, container a
`Runs` per running container, probe a `Probes` per checked target, netlink a `GatewayOf` for
the default route — and the correlator resolves both ends against the union-find result and
publishes `@catalog/state/edge/{edge_id}`. `L2Adjacent` is not published by anyone: it is
*derived* here from the observed-device identity claims already on the bus.

Two properties are load-bearing:

- **`edge_id` is `fnv1a_64(kind ‖ from ‖ to)` computed after resolution.** A refresh is an
  idempotent LWW overwrite, a restart with unchanged evidence publishes nothing, and two
  sensors seeing one relationship land on one key. Anything non-deterministic in the
  resolver would churn tombstones and upserts forever, so every lookup is sorted before it
  can reach the hash.
- **`merge.rs` never sees a relationship.** An edge cannot make two machines the same
  machine; a claim that could would be an identity claim wearing a different hat. A test
  greps to keep it that way.

### Consumer recipe: topology-aware alert inhibition

For a **key-agnostic** notifier — zenwatch (zenkey#389) is the motivating case — "do not
page for a guest whose hypervisor is down" needs three things off the bus and **no
application knowledge at all**:

1. **The edges.** `GET @catalog/@rpc/describe` for the `Edge` schema, then subscribe
   `v1/@catalog/state/edge/*` and seed with a `GET` on the same selector (it answers
   storage-shaped, one reply per edge on its own key). Deletes are tombstones. Filter to the
   **containment** kinds — `hosts`, `runs`, `gateway_of`, `probes`; `l2_adjacent` is inert
   and must not propagate, or a page floods the segment and blames a neighbour.
2. **Alert → entity.** An alert's key carries its origin (`v1/<origin>/state/<producer>/alert/…`).
   Subscribe `v1/@catalog/state/entity/*` and match `HostEntity.host_id == origin`; follow
   `@catalog/state/alias/*` so an entity that has been merged still resolves. That is the
   whole mapping — no per-application table.
3. **Down.** Liveliness tokens: an entity is down when every member origin's
   `…/state/<producer>/alive` is gone. What counts as "down" is deliberately the consumer's
   decision — lost liveliness, `HostEntity.status == "offline"`, an operator marking
   maintenance are all legitimate and differ per deployment.

Then walk `from → to` over containment edges from each down entity, bounded (ZenSight caps
at depth 4 with a visited set — the graph is built by independent sensors that have no way
to agree there is no cycle, and two hosts can each claim to be the other's gateway from a
stale table). Every firing alert on a reached entity is a **symptom**; the alert on the
entity that has no down containment ancestor is the **cause**.

`zensight_common::impact::attribute(edges, firing, down) -> Impact` is exactly this
function, pure and clock-free, if a consumer would rather link it than reimplement it.

## Documentation

- [`docs/correlation.md`](docs/correlation.md) — the operational merge model
  (ranked rules, conflict guard, entity ids, debounce/re-emit, tombstones).
- [`docs/keyspace.md`](docs/keyspace.md) — the evidence/entity/pdns keyspace
  this service consumes and produces.
- [`docs/storage.md`](docs/storage.md) — durable state/catalog storage and the
  historical passive-DNS (pdns) InfluxDB tier.
- [`../docs/KEYSPACE.md`](../docs/KEYSPACE.md) — the deployed fleet-wide
  key-expression profile (normative spec:
  [`../docs/rfcs/keyspace-v2/`](../docs/rfcs/keyspace-v2/00-index.md)).
- [`../docs/design/correlation.md`](../docs/design/correlation.md) — the full
  correlation design rationale (why these rules, ranks, and confidences).
