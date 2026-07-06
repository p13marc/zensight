# Durable identity storage

The correlator is a live pub/sub service: it holds evidence and the entity view in
RAM and reseeds from the sensors' cached self-reports on restart. That is the
right default — no database to operate, a restart converges. But the identity
plane can optionally be persisted to a **router-hosted Zenoh storage** so it
survives a fleet-wide restart or feeds an external time-series store.

Both tiers use the standard `zenohd` **storage-manager** plugin
(`zenoh-plugin-storage-manager`) plus a backend volume. This page covers the two
identity tiers; the (immutable, content-addressed) blob store is documented
separately in [`../../zenoh-blob/docs/router-storage.md`](../../zenoh-blob/docs/router-storage.md).
The fleet-wide overview is [`../../docs/STORAGE.md`](../../docs/STORAGE.md).

| Tier | Config | Backend | Keys | Mutability |
|------|--------|---------|------|------------|
| Identity control plane | [`configs/router-evidence-storage.json5`](../../configs/router-evidence-storage.json5) | `zenoh-backend-fs` | `zensight/_meta/{evidence,entity}/**` | mutable (last-writer-wins) |
| Historical passive-DNS | [`configs/router-pdns-influxdb-storage.json5`](../../configs/router-pdns-influxdb-storage.json5) | `zenoh-backend-influxdb` | `zensight/@pdns/**` | append (time series) |

## Mutable keys need timestamping — the one correctness subtlety

This is what separates the identity stores from the immutable blob store.
Evidence, entity, and `@pdns` docs are **last-writer-wins mutable**: a sensor
re-publishes its `HostEvidence` on every refresh; the correlator re-publishes each
`HostEntity` (and `DELETE`-tombstones retired ones) and re-emits `@pdns` records as
an IP's name set grows. The storage must keep the **newest** value per key and
must **never let a late, duplicated, or replayed sample clobber newer state**.

A Zenoh storage decides "newer" by the sample's **timestamp**. An un-timestamped
sample can't be ordered, so a storage for mutable keys must run on a node with
timestamping enabled so every sample is stamped on arrival:

```json5
timestamping: {
  enabled: true,               // stamp any un-timestamped sample
  drop_future_timestamp: false, // re-stamp (don't drop) a clock-skewed sample
}
```

A **router** enables timestamping by default; both identity configs set it
explicitly so they stay correct even if run on a peer/client.

## Evidence + entity (fs backend)

[`configs/router-evidence-storage.json5`](../../configs/router-evidence-storage.json5)
persists the identity control plane with two fs storages:

- `zensight/_meta/evidence/**` — `HostEvidence` claims and `NameObservation`
  batches (correlator *input*).
- `zensight/_meta/entity/**` — the correlator's merged `HostEntity` docs
  (correlator *output*, single writer).

```bash
export ZENOH_BACKEND_FS_ROOT=/var/lib/zensight/storage
zenohd -c configs/router-evidence-storage.json5
```

`DELETE` tombstones (a dropped claim, a retired entity) remove the key like any
other sample; with timestamping on, a replayed old tombstone can't erase a live
doc. This is **complementary** to the correlator's in-RAM reseed, not a
replacement — it adds durability so a late joiner (or a whole-fleet restart) can
seed the last known identity state from disk before the sensors re-report.

## Historical passive-DNS (`@pdns`, InfluxDB backend)

The evidence/entity tier is live state (TTL-swept, no history). For a durable
*historical* IP↔name record ("what did 10.0.0.9 resolve to last Tuesday?"), the
correlator publishes a `PdnsRecord` — the IP's full accumulated name set — on
`zensight/@pdns/<ip-slug>` every time its name store learns/updates names for an
IP. Nothing consumes `@pdns` on the live bus; it exists to be captured.

[`configs/router-pdns-influxdb-storage.json5`](../../configs/router-pdns-influxdb-storage.json5)
subscribes `zensight/@pdns/**` into an InfluxDB bucket via
`zenoh-backend-influxdb`, so each write becomes a time-series point.

> **Not live-tested.** There is no InfluxDB in the build/CI sandbox, so this
> config is a documented example. The schema follows the InfluxDB **v1.8** backend
> (volume id `influxdb`, `db` = database); the **v2.x** variant (volume id
> `influxdb2`, org + token, `db` = bucket) is sketched in comments. Confirm the
> exact keys against the `zenoh-backend-influxdb` README for your backend version
> before deploying. The correlator-side publish is unit-tested
> (`engine::name_message_emits_historical_pdns_record`).

## Operational notes

- **Authorization.** A storage answers any GET and accepts any PUT in its key
  range. Gate the identity keyspace with Zenoh access control if sensitive —
  evidence carries hashed machine-ids, IPs, MACs, and passive-DNS names.
- **Retention.** The fs evidence/entity store keeps the latest doc per key; keys
  age out only when the correlator tombstones them, so disk use tracks fleet size,
  not time. The InfluxDB `@pdns` store grows with history — set a bucket retention
  policy to bound it.
- **One storage per key range per fleet.** Run these on a stable router (or a
  small storage-only router), not on every peer.
