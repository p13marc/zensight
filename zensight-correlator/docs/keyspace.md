# Correlator keyspace

The keys the catalog (correlator) consumes and produces. The deployed fleet-wide
profile lives in [`../../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) (normative
spec: [`../../docs/rfcs/keyspace-v2/`](../../docs/rfcs/keyspace-v2/00-index.md));
this page is the correlator-scoped slice. All key builders are in
`zensight-common/src/keyexpr.rs`.

The catalog never touches the telemetry firehose
(`zensight/@v1/*/telemetry/**`). It consumes the fleet **state plane** and is the
single writer of the verbatim `@catalog` origin (`zensight/@v1/@catalog/…`).

## Consumes (subscriptions)

| Key | Payload | Notes |
|-----|---------|-------|
| `zensight/@v1/*/state/*/evidence/**` | `HostEvidence` | Self-report (`…/evidence/self`) or observed-device (`…/evidence/device/<device>`) claim. `AdvancedSubscriber` with `history()` + late-publisher detection, so a fresh catalog immediately gets the sensors' cached claims. A `DELETE` on a claim key drops the claim (an evidence tombstone). |
| `zensight/@v1/*/state/*/evidence/names/*` | `NameObservation` | Passive-DNS, one key per observed IP (`.`/`:` → `-`), last-writer-wins on the wire; accumulated in the name store. Own `AdvancedSubscriber` (the host-evidence subscriber skips the `names/` subtree, handled here). |
| `zensight/@v1/*/state/*/device/*/liveness` | `DeviceLiveness` | Plain subscriber (no history). Rolled up onto `entity.status`. Gated by `status_from_liveness`; skipped entirely when off. |

Wildcards used: `all_evidence_wildcard()`, `all_name_evidence_wildcard()`,
`all_liveness_wildcard()`.

## Produces (publications)

| Key | Payload | Notes |
|-----|---------|-------|
| `zensight/@v1/@catalog/state/entity/<entity_id>` | `HostEntity` | The merged entity view; `<entity_id>` is `h-<12hex>` (the origin id when a member has a `host_id`). The catalog is the **single writer**. A `PUT` upserts (cached plain publisher per id, reliable + block); a `DELETE` tombstones a retired entity. Re-emitted every `reemit_secs`. |
| `zensight/@v1/@catalog/state/pdns/<ip-slug>` | `PdnsRecord` | Historical passive-DNS: an IP's full accumulated name set, published on every name-store update for that IP. Plain `session.put` (the IP set is unbounded), reliable + block. Meant to be captured by a storage backend, not consumed live — see [`storage.md`](storage.md). |

## Queryables (late-joiner seed / on-demand)

| Key | Selector | Reply |
|-----|----------|-------|
| `zensight/@v1/@catalog/state/entity/*` | — | The entity seed IS the state selector: a late-joining frontend plain-GETs it on connect and the catalog answers **storage-shaped** — one JSON `HostEntity` reply per entity, each on its concrete state key. |
| `zensight/@v1/@catalog/@rpc/names` | `?ip=<addr>` | JSON `Vec<NameVal>` — up to 32 accumulated names for that IP. An `@rpc` procedure: resolves arbitrary/external IPs on demand instead of flooding the bus; a missing/blank `ip` replies with an empty set. |

## Liveliness (catalog ownership)

Ownership is an explicit claim protocol (`guard.rs`, RFC 06 §5.3): every
candidate declares a liveliness claim token at
`zensight/@v1/@catalog/state/claim/<zid>`, queries the claim set
(`…/state/claim/*`), and the lexically-lowest claim chunk wins the election —
deterministic and coordinator-free. Losers exit; only the elected owner declares
`zensight/@v1/@catalog/state/alive` and the catalog publishers/queryables.

## Why the pdns tier is off the firehose

`@catalog` is an `@`-verbatim chunk, so
`zensight/@v1/@catalog/state/pdns/<ip-slug>` is invisible to the telemetry class
selector (`zensight/@v1/*/telemetry/**`) **and** to the `*`-origin fleet state
selector (`zensight/@v1/*/state/**`) — `*` never matches a verbatim chunk. Only
the dedicated selector (`all_pdns_wildcard()` =
`zensight/@v1/@catalog/state/pdns/**`) captures it. A regression test in
`zensight-common` pins this.
