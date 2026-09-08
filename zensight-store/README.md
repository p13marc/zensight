# zensight-store

The tiered time-series store: a hot in-memory ring, minute and hour tiers in
[redb](https://docs.rs/redb), and the log, event and chunk tables that ride the
same file.

This was `zensight::store` — a module inside the Iced binary, writing
`~/.local/share/zensight/metrics.redb`, readable by nothing but the GUI that
wrote it (#904). On a fleet that GUI is open for minutes a week, so the history
it held was mostly gaps. It is a crate now so a headless service — the
`zensight-historian` of #898 — can write the same tiers and serve them to
everyone, and so the GUI's local cache and that service agree on what a series
is called.

## Who uses it

| Caller | How |
|---|---|
| `zensight` (the GUI) | a local cache: the hot ring feeds charts and sparklines live, the redb tiers survive restart. Enables the `blob` feature. |
| `zensight-historian` (#906) | the fleet's durable history, ingested from `v1/*/telemetry/**`. |

## Features

- **`blob`** (off by default) — `RedbContentStore`, the [zblob](https://github.com/p13marc/zblob)
  `ContentStore` adapter over the `chunks` table. It is the one part of this
  crate that is not a time series, and it is optional so a consumer that only
  wants history does not pull the blob stack. The `chunks` *table* is created
  either way, so the on-disk file is identical and a GUI can open a file a
  feature-off writer made.

  Its two tests are gated with it: `cargo test -p zensight-store --all-features`
  runs them, and so does `cargo test --workspace` (resolver-2 feature
  unification turns `blob` on via the GUI).

## Tiers

Numeric series flow through three tiers of decreasing resolution:

| Tier | Resolution | Where it lives |
|------|------------|----------------|
| **Hot** | per-second | Fixed-size in-memory `RingBuffer` per metric — bounded, held in timestamp order, read directly by charts. Default capacity `DEFAULT_HOT_CAPACITY = 3_600` (one hour of per-second samples). An in-order push is O(1); a late one is inserted at its place from the tail and reported as `Pushed::Reordered`, because the historian's subscriber has recovery on and a ring holding a non-monotonic sequence made `counter_rate` answer `None` (#1062). |
| **Warm** | per-minute | Periodically downsampled from hot, flushed to the redb `samples` table. |
| **Cold** | per-hour | Coarsest downsample, also in the `samples` table. |

`Tier::ALL` is `[Second, Minute, Hour]` (coarsest last). Each tier has a fixed
`bucket_secs()` width and a `retention_secs()`; the `PersistentStore::prune`
sweep evicts buckets older than their tier's retention, so the on-disk file stops
growing (retention increases from hot → cold).

### Keys and typing

A metric's **series path** is `<origin>/<producer>/<subject>` — the wire key
minus the class chunk. That is the identity a reader can derive from a sample
alone, which is what lets this store, used as a GUI cache, and the fleet
historian (#898) name the same series the same way without a catalog between
them. The origin is in it for the same reason `DeviceId` carries one (#474:
two hosts with one hostname are two devices).

Paths are interned to a compact `MetricId(u32)` per the architecture contract,
so the store is keyed by small integers rather than strings. The redb `samples`
table maps a packed `(metric_id, tier, bucket_ts)` key (a `u128`) to a
`Bucket { last: f64, min: f64, max: f64 }`. A `Sample` is a plain
`{ ts: i64 (ms), value: f64 }` record.

**Values keep their kind.** `SampleValue` is `Counter(u64) | Gauge(f64) |
Bool(bool)`, and the kind is written into the `metrics` row. Before v3 every
value was flattened to `f64` at ingest, which made a counter reset and a gauge
that fell the same negative delta — so every consumer that charted a counter
re-inferred resets by hand, three times in the GUI alone. `counter_rate` and
`rate_series` now live in `zensight_store::rate` and are computed where the
kind is known. `telemetry_to_f64` remains for callers that genuinely only want
a number.

**`min`/`max` per bucket.** The tier semantics are still
last-observation-per-bucket, and `last` is the value. `min`/`max` bound what
the bucket covered, so a coarse tier can still say a spike happened: an hour
bucket that reported only its closing value showed a gauge that touched 400 and
settled at 12 as twelve, flat.

They are **`f64`, and they are bounds** (#1061). They were `f32` — four bytes
each, on the reasoning that a chart's range is not the value — and written with
an `as` cast, which rounds to *nearest*. Rounding to nearest does not produce a
bound: `2^24 + 1` stored as `2^24`, at `rx_bytes` scale an ulp is ~65 KB, and
since `last` stayed exact a bucket could report a `max` below its own `last`.
The range API sells `agg=max` on exactly this number.

The range is **merged on write**, never replaced (#1060): a coarse bucket is
written once per flush, not once per bucket — at a ten-second flush an hour
bucket is written hundreds of times — so `write_batch` folds each flush
window's min/max into the row already on disk and takes `last` from the
newer window. Before that, each write replaced the row and an hour's range
was its final ten seconds.

**The ids and their metadata are persisted.** A `metrics` table maps each
interned path to `(id, kind, source, metric, unit)`, written in the same transaction
as the samples that use it, and the interner is rebuilt from it on open — so an
id means the same path, of the same kind, in every process that opens the file.
For a long time the ids were not written at all: they were minted in
network-arrival order, so every launch re-numbered every metric and a chart
seeded "from history" read another metric's buckets.

`source` and `metric` are stored because the series path no longer carries
them and they cannot be recovered from it. A proxy producer's subject is
`{device}/{metric...}`, so the observed device and the display metric name
would each have to be recovered by un-slugging a device chunk — a guess, in
the code that decides which host a chart belongs to.

A `meta` table carries the schema version (`SCHEMA_VERSION`, now **5** — v4
added `unit` to the metrics row, v5 widened a bucket's `min`/`max` to `f64`). **It
is read in its own transaction, before any other table is opened**: v3 re-typed
both `metrics` and `samples`, and opening a re-typed table fails with a redb
*table type mismatch*, which is not the error the "wrong layout, move it aside"
path recognises. An older file is therefore refused cleanly as
`StoreOpenError::Schema { found: N }` and moved aside
(`metrics.redb.schema-vN`), the same way a pre-v2 file and an older redb file
format already were. It is a cache; the history it shadows outlives it.

The move-aside is `PersistentStore::open_or_move_aside`, and **both** callers
go through it. The historian did not: it called `open_with_cache` and treated
every error as "run memory-only", so a schema bump would have cost it durable
history on every restart from then on — silently, until somebody deleted the
file by hand (#1061).

Without a database (`--demo`, a locked file, a read-only data dir) the store
keeps only the hot rings: nothing is buffered for a flush that cannot happen.

## Log events

Per-line log events are text with unbounded cardinality, so they do **not** go
through the numeric tiers. They get their own redb `logs` table keyed by a
time-sortable uid (`<ts><seq>`), storing a serialized `StoredLog`.

To keep this store bounded without losing signal, logs are written with
**template-aware sampling** (`LogRetention`):

- **Keep all errors** — any line at or above `LOG_ERROR_SEVERITY` (OTel severity
  17 = ERROR; FATAL is 21–24) is always persisted.
- **Keep novel templates** — the first sighting of a message template is kept.
- **Sample repetitive info** — known-template, non-error lines are kept 1-in-N
  (`LOG_SAMPLE_EVERY = 10`).

A row cap (`LOG_STORE_MAX_ROWS = 200_000`) bounds the table; `prune_logs` drops
the oldest rows beyond the cap — the log analogue of tier retention. The net
effect: search-back and boot-selection survive a restart, but the file can't grow
without limit.

### Logs view seeding

The Logs view seeds from **two** sources when it opens, in parallel:

1. the **sensors' durable stores** (#603) — the authoritative, unsampled
   history. The frontend sends `@rpc/logs/events` with a `from=` bound (24 h,
   or the picked time range), which is what routes the sensor to its redb
   store rather than its 500-line hot ring.
2. this **local cache** — the frontend queries the `logs` table and delivers
   the rows via `Message::LogHistoryLoaded`.

The local store is per-GUI-instance and template-sampled, so it is the offline
path, not the source of truth: when no sensor answers, the feed degrades to
cached history and says so (a banner names the fetch error — an unreachable
sensor must not be indistinguishable from "there are no logs"). Rows from both
sources dedup on `uid`, so the overlap is free.

The 5 s live-tail refresh deliberately sends **no** `from=`, so the steady-state
poll stays a cheap ring read.

Scrolling back further is **cursor-paginated** (#601): "Load older" sends
`after_uid=` with the oldest buffered uid, and the sensor replies with records
strictly older than it, newest-first — so pages abut without overlapping and
memory stays bounded by what the operator actually asked to see. A short page
(fewer than the reply cap) is the only "no more records" signal the reply
carries, so it is what ends the walk. An older page deliberately does **not**
advance the live-tail watermark: the tail must not skip forward past lines it
has never seen.

## Event records

Durable `events`-class records (SNMP traps today, #578) get their own redb
`events` table keyed by the record's **ULID**. ULIDs sort chronologically, so
the table is time-ordered by construction and "the most recent N events" is a
bounded reverse range walk — the same shape as the logs table.

There is deliberately **no sampler**: an event is already a rare, deliberate
record, and dropping a trap would defeat the point of persisting them. The
ULID key also makes writes idempotent, so a record delivered twice (the live
subscriber overlapping a storage backfill) updates in place instead of
duplicating.

`EVENT_STORE_MAX_ROWS = 20_000` bounds the table — two orders below the log
cap, because traps are rare and a trap storm should not evict a week of
history. `prune_events` drops the oldest rows beyond it on the shared prune
cadence.

The fleet trap feed seeds from this table at **boot** (not on view open): the
feed lives on the dashboard, which is the boot view, so the frontend queries
`events` during `boot()` and delivers the rows via
`Message::SnmpEventHistoryLoaded`. Records dedup by ULID against whatever the
live subscriber has already delivered. The net effect is the one #578 asked
for: the feed survives a GUI restart *without* requiring a bus-side Zenoh
storage aligned on `**/events/**`.

## Async discipline

The in-memory ring append is O(1) and runs inline on the Iced update thread.
Every redb read/write, by contrast, runs **off** the UI thread via
`Task::future` + `spawn_blocking` — `PersistentStore` is `Send + Sync` and is
cloned behind an `Arc`. The UI thread never blocks on disk I/O.

The batching seam is explicit in the API: the in-memory side accumulates writes
(`record`, `record_log`, `record_event`) and hands off `take_flush_batch` /
`take_log_flush_batch` / `take_event_flush_batch` tuples of
`(PersistentStore, rows)` to be flushed on a blocking task, keeping the redb
transaction off the render path.

## Other tables

The store also defines a content-addressed `chunks` table (key `<algo>/<hex>`,
value raw bytes) used as the immutable, idempotent substrate for large-data
transfer and directory-sync dedup/resume — writing a chunk once and reading it
back by content hash.
