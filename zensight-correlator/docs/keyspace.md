# Correlator keyspace

The keys the correlator consumes and produces. The authoritative fleet-wide
contract lives in [`../../docs/KEYSPACE.md`](../../docs/KEYSPACE.md); this page is
the correlator-scoped slice. All key builders are in `zensight-common/src/keyexpr.rs`.

The correlator never touches the telemetry firehose (`zensight/<protocol>/**`). It
lives entirely on the `_meta/**` control plane plus the `@pdns` verbatim tier.

## Consumes (subscriptions)

| Key | Payload | Notes |
|-----|---------|-------|
| `zensight/_meta/evidence/host/<sensor>/<source>` | `HostEvidence` | Self-report or observed host claim. `AdvancedSubscriber` with `history()` + late-publisher detection, so a fresh correlator immediately gets the sensors' cached claims. A `DELETE` on this key drops the claim (an evidence tombstone). |
| `zensight/_meta/evidence/names/<sensor>/<ip-slug>` | `NameObservation` | Passive-DNS, one key per observed IP (`.`/`:` → `-`), last-writer-wins on the wire; accumulated in the name store. Own `AdvancedSubscriber`. |
| `zensight/<protocol>/<source>/@/devices/*/liveness` | `DeviceLiveness` | Plain subscriber (no history). Rolled up onto `entity.status`. Gated by `status_from_liveness`; skipped entirely when off. The legacy protocol-scoped shape (`zensight/<protocol>/@/devices/*/liveness`, pre-0.8 sensors) gets a second subscriber for one release. |

Wildcards used: `all_evidence_wildcard()` = `zensight/_meta/evidence/**` (the host
subscriber skips the `/names/` subtree, handled by its own subscriber),
`all_name_evidence_wildcard()` = `zensight/_meta/evidence/names/**`,
`all_liveness_wildcard()` = `zensight/*/*/@/devices/*/liveness` (host-scoped; the
legacy shape is subscribed literally during the transition).

## Produces (publications)

| Key | Payload | Notes |
|-----|---------|-------|
| `zensight/_meta/entity/host/<entity_id>` | `HostEntity` | The merged entity view. The correlator is the **single writer**. A `PUT` upserts (cached plain publisher per id, reliable + block); a `DELETE` tombstones a retired entity. Re-emitted every `reemit_secs`. |
| `zensight/@pdns/<ip-slug>` | `PdnsRecord` | Historical passive-DNS: an IP's full accumulated name set, published on every name-store update for that IP. Plain `session.put` (the IP set is unbounded), reliable + block. Meant to be captured by a storage backend, not consumed live — see [`storage.md`](storage.md). |

## Queryables (late-joiner seed / on-demand)

| Key | Selector | Reply |
|-----|----------|-------|
| `zensight/_meta/query/entities` | — | JSON `Vec<HostEntity>` — the full current entity set. A late-joining frontend GETs this on connect to seed before the next re-emit. |
| `zensight/_meta/query/names` | `?ip=<addr>` | JSON `Vec<NameVal>` — up to 32 accumulated names for that IP. Resolves arbitrary/external IPs on demand instead of flooding the bus; a missing/blank `ip` replies with an empty set. |

## Liveliness (single-writer guard)

`zensight/_meta/correlator/@/alive` — a Zenoh liveliness token. On startup the
correlator GETs it; if another instance already holds it, the new instance
warn-and-exits. Otherwise it declares its own token for the process lifetime.

## Why `@pdns` is off the firehose

`@pdns` is an `@`-verbatim chunk, so it is invisible to the telemetry wildcard
(`zensight/**`) and the per-sensor control wildcard (`zensight/*/@/**`), and the
exporters' `@`-chunk reject keeps it off Prometheus/OTel. A regression test in
`zensight-common` pins this.
