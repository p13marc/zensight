# Local store

The frontend persists telemetry to a bounded local store (`src/store.rs`) so
history survives restart without growing unbounded on disk. It is a
Netdata-style tiered time-series store backed by [redb](https://docs.rs/redb),
with a separate keyed store for log events.

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

The Logs view seeds from the cold store when it opens: on view open the frontend
queries the `logs` table and delivers the results via `Message::LogHistoryLoaded`,
so historical lines are available immediately without waiting for live traffic.

## Async discipline

The in-memory ring append is O(1) and runs inline on the Iced update thread.
Every redb read/write, by contrast, runs **off** the UI thread via
`Task::future` + `spawn_blocking` — `PersistentStore` is `Send + Sync` and is
cloned behind an `Arc`. The UI thread never blocks on disk I/O.

The batching seam is explicit in the API: the in-memory side accumulates writes
(`record`, `record_log`) and hands off `take_flush_batch` / `take_log_flush_batch`
tuples of `(PersistentStore, rows)` to be flushed on a blocking task, keeping the
redb transaction off the render path.

## Other tables

The store also defines a content-addressed `chunks` table (key `<algo>/<hex>`,
value raw bytes) used as the immutable, idempotent substrate for large-data
transfer and directory-sync dedup/resume — writing a chunk once and reading it
back by content hash.
