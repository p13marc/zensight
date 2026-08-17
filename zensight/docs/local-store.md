# Local store

The frontend persists telemetry to a bounded local store (`src/store.rs`) so
history survives restart without growing unbounded on disk. It is a
Netdata-style tiered time-series store backed by [redb](https://docs.rs/redb),
with separate keyed stores for log events and event records.

Metric history used to live only in an in-memory `VecDeque` (capped per metric)
and was lost on restart. The store replaces that with a hot in-memory ring plus
downsampled, retention-bounded persistence.

## Tiers

Numeric series flow through three tiers of decreasing resolution:

| Tier | Resolution | Where it lives |
|------|------------|----------------|
| **Hot** | per-second | Fixed-size in-memory `RingBuffer` per metric — O(1) append, bounded, read directly by charts. Default capacity `DEFAULT_HOT_CAPACITY = 3_600` (one hour of per-second samples). |
| **Warm** | per-minute | Periodically downsampled from hot, flushed to the redb `samples` table. |
| **Cold** | per-hour | Coarsest downsample, also in the `samples` table. |

`Tier::ALL` is `[Second, Minute, Hour]` (coarsest last). Each tier has a fixed
`bucket_secs()` width and a `retention_secs()`; the `PersistentStore::prune`
sweep evicts buckets older than their tier's retention, so the on-disk file stops
growing (retention increases from hot → cold).

### Keys and typing

Metric paths are interned to a compact `MetricId(u32)` per the architecture
contract, so the store is keyed by small integers rather than strings. The redb
`samples` table maps a packed `(metric_id, tier, bucket_ts)` key (a `u128`) to a
downsampled `f64`. A `Sample` is a plain `{ ts: i64 (ms), value: f64 }` record,
and the `TelemetryValue → f64` projection lives in one place
(`telemetry_to_f64`).

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
