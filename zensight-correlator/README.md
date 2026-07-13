# zensight-correlator

Headless ZenSight service — **the catalog** — that fuses per-sensor **identity
evidence** into one `HostEntity` per physical host. Sensors self-report a stable
`host_id` (and, with `evidence` on, republish the hosts and names they observe);
the catalog is the **single writer** of the verbatim `@catalog` origin
(`zensight/@v1/@catalog/…`) — it subscribes only to the evidence state
(`zensight/@v1/*/state/*/evidence/**`, never the telemetry firehose), merges
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
declares a liveliness claim at `zensight/@v1/@catalog/state/claim/<zid>`, the
lexically-lowest claim wins the election, and losers exit rather than
double-write. Only the elected owner declares `…/@catalog/state/alive` and the
catalog publishers/queryables (deterministic merge means a partition-split pair
would emit identical docs, so this is a safety net, not a lock).

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
