# Durable storage tiers

ZenSight is a live pub/sub system: sensors, the correlator, and the frontend
hold state in RAM and reseed it from cached publishers on restart. That is the
right default (no database to operate, a restart converges), but some data is
worth persisting to disk on a **router-hosted Zenoh storage** so it survives a
fleet-wide restart or feeds an external time-series store.

This doc covers the storages ZenSight ships configs for. All of them use the
standard `zenohd` **storage-manager** plugin (`zenoh-plugin-storage-manager`)
plus a backend volume — the same mechanism, three different data classes:

| Data class | Config | Backend | Keys | Mutability |
|------------|--------|---------|------|------------|
| Blob chunks (Tier-2) | [`configs/router-blob-storage.json5`](../configs/router-blob-storage.json5) | `zenoh-backend-fs` | `zensight/_blob/**` | **immutable** (content-addressed) |
| Identity control plane (#310) | [`configs/router-evidence-storage.json5`](../configs/router-evidence-storage.json5) | `zenoh-backend-fs` | `zensight/_meta/{evidence,entity}/**` | **mutable** (last-writer-wins) |
| Historical passive-DNS (#310) | [`configs/router-pdns-influxdb-storage.json5`](../configs/router-pdns-influxdb-storage.json5) | `zenoh-backend-influxdb` | `zensight/@pdns/**` | append (time series) |

The blob store is documented separately in
[`docs/BLOB-ROUTER-STORAGE.md`](BLOB-ROUTER-STORAGE.md); this doc covers the two
identity tiers added in #310.

---

## Mutable keys vs. immutable keys — why timestamping matters

This is the one correctness subtlety, and it is the difference between the blob
store and the identity stores.

- **Blob chunks are immutable.** A key is `<prefix>/<algo>/<hash>` — a content
  hash that only ever maps to one byte string. Re-PUTting is idempotent; two
  producers PUTting the same chunk PUT identical bytes. Last-writer-wins
  reconciliation is a no-op, so the blob config needs **no timestamping**.

- **Evidence, entity, and `@pdns` docs are mutable, last-writer-wins.** A sensor
  re-publishes its `HostEvidence` on every refresh; the correlator re-publishes
  each `HostEntity` (and `DELETE`-tombstones retired ones) and re-emits `@pdns`
  records as an IP's name set grows. The storage must keep the **newest** value
  per key and must **never let a late, duplicated, or replayed sample clobber
  newer state**.

  A Zenoh storage decides "newer" by the sample's **timestamp**. If a sample
  arrives without one, the storage can't order it. So a storage for mutable keys
  **must** run on a node with timestamping enabled, so every un-timestamped
  sample gets stamped on arrival:

  ```json5
  timestamping: {
    enabled: true,             // stamp any un-timestamped sample
    drop_future_timestamp: false, // re-stamp (don't drop) a clock-skewed sample
  },
  ```

  A **router** enables timestamping by default (`{ router: true, peer: false,
  client: false }`), so a router-hosted storage is already correct. The identity
  configs set `enabled: true` **explicitly** so they stay correct even if the
  storage is ever run on a peer/client. Both identity configs carry this block
  and a comment pointing here.

---

## Half 1 — durable evidence/entity (fs backend)

[`configs/router-evidence-storage.json5`](../configs/router-evidence-storage.json5)
persists the identity control plane to disk. Two storages on a filesystem volume:

- `zensight/_meta/evidence/**` — `HostEvidence` self-reports/observed claims and
  `NameObservation` passive-DNS batches (correlator *input*).
- `zensight/_meta/entity/**` — the correlator's merged `HostEntity` docs
  (correlator *output*; single writer).

```bash
export ZENOH_BACKEND_FS_ROOT=/var/lib/zensight/storage   # base dir for the fs volume
zenohd -c configs/router-evidence-storage.json5
```

DELETE tombstones (a dropped evidence claim, a retired entity) are handled by the
fs backend like any other sample — the key is removed. With timestamping on, a
tombstone only wins if it is newer, so a replayed old tombstone can't erase a
live doc.

This is **complementary** to the correlator's in-RAM reseed, not a replacement:
sensors still cache and re-emit their own evidence, and the correlator still
recomputes entities as a pure function of live evidence. The storage adds
*durability* — a late joiner (or a whole fleet restart) can seed the last known
identity state from disk before the sensors have re-reported.

## Half 2 — historical passive-DNS (`@pdns`, InfluxDB backend)

The evidence/entity tier is live state (TTL-swept — no history). For a durable
*historical* IP↔name record ("what did 10.0.0.9 resolve to last Tuesday?"), the
**correlator** publishes a `PdnsRecord` on `zensight/@pdns/<ip-slug>` every time
its name store learns/updates names for an IP (see
[`docs/KEYSPACE.md`](KEYSPACE.md) §4.3). The record carries the IP's *full
accumulated* name set, not just the latest single name.

Nothing consumes `@pdns` on the live bus — it exists to be captured into a
time-series store.
[`configs/router-pdns-influxdb-storage.json5`](../configs/router-pdns-influxdb-storage.json5)
subscribes `zensight/@pdns/**` into an InfluxDB bucket via
`zenoh-backend-influxdb`, so each PUT becomes a time-series point and you get a
queryable IP↔name history.

> **NOT live-tested.** There is no InfluxDB in the build/CI sandbox, so that
> config is a documented example only. Its schema follows the InfluxDB **v1.8**
> backend (volume id `influxdb`, `db` = database) — the shape verified against a
> working `zenoh-backend-influxdb` config — with the **v2.x** variant (volume id
> `influxdb2`, org + token, `db` = bucket) sketched in comments. Confirm the
> exact keys against the `zenoh-backend-influxdb` README for your backend version
> before deploying.

`@pdns` is an `@`-verbatim chunk, so — like `@media` (#359) — it is invisible to
the telemetry firehose (`zensight/**`) and the per-sensor control wildcard
(`zensight/*/@/**`), and the exporters' `@`-chunk reject keeps it off
Prometheus/OTel. A regression test pins that
(`keyexpr::pdns_tier_is_off_the_telemetry_and_control_buses`).

---

## Verification status

| Tier | How verified |
|------|--------------|
| Evidence/entity (fs) | **Config shape** validated live: `zenohd` loaded `router-evidence-storage.json5`, the storage-manager plugin accepted both storages and the `timestamping` block, and attempted to spawn the `fs` volume (failing only because `libzenoh_backend_fs.so` is not installed in the sandbox — only the `redb` backend is). The **persistence + LWW mechanism** was then verified end-to-end on the exact `zensight/_meta/evidence/**` key range using the installed `redb` backend as a stand-in: PUT a doc → kill the router → restart a fresh `zenohd` against the same on-disk storage → GET returned the persisted doc with no re-PUT; a newer PUT correctly superseded it (last-writer-wins). The shipped config uses `fs` (mirroring the blob store); swap `redb` back to `fs` once the fs backend is installed. |
| `@pdns` (InfluxDB) | Correlator-side publish verified by unit test (`engine::name_message_emits_historical_pdns_record`) + `keyexpr`/`PdnsRecord` round-trip tests. The InfluxDB storage config is **not** live-tested (no InfluxDB in sandbox); schema reviewed against a working v1.8 `zenoh-backend-influxdb` config. |

## Operational notes

- **Authorization.** A storage answers any GET and accepts any PUT in its key
  range. Gate the identity keyspace with Zenoh access control if it is sensitive
  (evidence carries hashed machine-ids, IPs, MACs, and passive-DNS names).
- **Retention.** The fs evidence/entity store keeps the latest doc per key; keys
  age out only when the correlator tombstones them, so disk use tracks the fleet
  size, not time. The InfluxDB `@pdns` store grows with history — set a bucket
  retention policy to bound it.
- **One storage per key range per fleet.** Run these on a stable router (or a
  small storage-only router), not on every peer.
