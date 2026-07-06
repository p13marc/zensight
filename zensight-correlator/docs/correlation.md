# The correlation model

How the correlator turns a stream of `HostEvidence` claims into one `HostEntity`
per physical host. This is the operational how-it-works; for the full rationale
(why these rules, ranks, and confidences) see
[`../../docs/design/correlation.md`](../../docs/design/correlation.md).

## The merge is a pure function of the evidence set

Every `HostEvidence` claim is keyed by `(sensor, source)`; the store keeps the
latest claim per key (`store.rs`). On recompute, the engine snapshots the
**TTL-live** evidence and hands it to `merge::correlate` (`merge.rs`), a pure
module with no Zenoh, tokio, or clock. Given the same evidence set in *any* input
order it produces **byte-identical** entities (ids, member order, ip/mac order).
That determinism is what makes the correlator stateless and restart-recoverable:
after a restart the sensors' cached self-reports rebuild the identical entity set.

## Union-find over ranked identity rules

Each claim is a node. A disjoint-set (union-find) groups nodes that a rule says
are the same host. Rules generate candidate **bridges** `(a, b, rule, confidence)`
that are applied **strongest-first**:

| Rule | Condition | Base confidence |
|------|-----------|-----------------|
| `host_id` | both have `host_id` and they are equal | 1.0 |
| `cloud_instance` | both have equal cloud `(provider, instance_id)` | 0.95 |
| `mac_ip` | share ≥1 MAC **and** share ≥1 IP | 0.8 |
| `fqdn` | equal non-empty FQDN (case-insensitive) | 0.5 |
| `hostname` | equal non-empty hostname (case-insensitive) | 0.25 |

Each rule is a kill-switch in config (`rules.{host_id,cloud_instance,mac_ip,fqdn,
hostname_enabled}`, all default true). Disable the weak `hostname` rule on
networks full of duplicate names like `MacBook-Pro.local`.

Key safety properties, all pinned by tests in `merge.rs`:

- **IP alone is never a bridge** (DHCP/NAT reuse) — only a hint.
- **MAC alone is never a bridge** (VMs clone MACs) — only MAC **and** IP together.
- **`cloud_instance` is authoritative per provider** (#311): a cloud control
  plane never hands out the same instance id twice, so it merges almost as
  strongly as `host_id`, and rescues cloned images that lost their machine-id.
  The same instance id on *different* providers does **not** merge.
- **`container_id` is never a merge key** — container ids are unique only per
  host runtime. It is unioned onto the entity descriptively, never used to join.

Bridges are sorted by a **content-derived** key (never input index), so the
applied order — and the resulting partition — is independent of arrival order.

## host_id-conflict guard

Two nodes with *different* `host_id`s must never land in one set, no matter what
weaker rule would otherwise bridge them (`UnionFind::try_union`). A host_id is
aggregated per set; any union that would place two distinct host_ids together is
dropped. So a shared hostname or shared cloud instance can never override two
machines that are provably distinct by machine-id — `host_id` stays the top
authority.

## Observer weighting

If *either* endpoint of a bridge is a third-party claim (`observer.is_some()` —
e.g. netring/netlink reporting a host they merely *saw* on the wire), the bridge
confidence is multiplied by **0.8**. Self-reports (`observer == None`) are also
preferred when choosing an entity's representative descriptive fields (hostname,
fqdn, vendor, platform): a self-report beats an observed value.

## Entity-id derivation

Ids are stable and order-independent (`entity_id_for`):

1. If any member has a `host_id` (the guard guarantees at most one distinct value
   per set), the id is `h_<first 12 hex of that host_id>`.
2. Otherwise, take the **highest-priority category any member has** —
   `fqdn` > `mac` > `hostname` > `ip` — and within it the
   lexicographically-smallest value, and the id is `h_<first 12 hex of
   sha256(value)>`. Picking the category before comparing values keeps the choice
   independent of member order.
3. If a set has none of those, fall back to `sha256` of the smallest member
   `sensor\u{1f}source` key — every node always has one, so an id always exists.

`fqdn`/`hostname` are lowercased before hashing, so casing can't split an id.

### Id upgrades and aliases

An observed asset can start life with a fallback id (e.g. fqdn-derived) and later
merge with a self-report that brings a `host_id`, producing a *new* id. The engine
detects this: an old id that shares ≥1 member with a new entity but is no longer a
current id is recorded in the new entity's `aliases` and its old id is tombstoned
(`apply_upgrades` in `engine.rs`).

## Debounced recompute, 60 s liveness re-emit

The async engine (`engine.rs`) coalesces bursts: an incoming claim arms a
debounce timer (`recompute_debounce_ms`, default 500 ms); recompute runs once the
bus goes idle for that gap. Each recompute:

1. sweeps evidence + name stores past their TTL (`evidence_ttl_secs`, default
   900 s),
2. runs the pure merge over live evidence,
3. injects passive-DNS **names** for the entity's IPs and rolls **device-liveness**
   up onto `entity.status` (worst-of-members: offline > degraded > online >
   unknown; gated by `status_from_liveness`),
4. diffs against the last published set using a `last_updated`-excluded content
   hash and emits `Upsert`/`Tombstone` ops for real changes only.

Separately, every `reemit_secs` (default 60 s) it re-publishes **every** current
entity with a fresh `last_updated` but unchanged content. This doubles as
correlator liveness (the frontend marks an entity stale after ~3× this period)
and reseeds a late-restarted bus. Because content is unchanged, re-emits are not
counted as changes by the diff.

## Tombstones

An entity is retired (a `DELETE` on its key) when it vanishes from the recomputed
set — because its evidence aged out past the TTL, was explicitly removed (an
evidence `DELETE` becomes `RemoveHost`, dropping the claim immediately rather than
waiting for the TTL), or was subsumed into an alias by an id upgrade.

## Names accumulate (they don't replace)

The `NameStore` is the one store that accumulates. Passive-DNS publishes one
`NameObservation` per IP (last-writer-wins on the wire), so the distinct names an
IP resolves to (an A record, a PTR, a TLS SNI) arrive as *separate* samples over
time. Replacing on each sample would keep only the latest name; instead each
observation add-or-refreshes a `(name, provenance)` entry (bumping `last_seen`
in place, capped per IP and globally). `entity.names` and the names queryable
return the ranked full set.
