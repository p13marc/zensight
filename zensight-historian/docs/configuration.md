# Configuration

`configs/historian.json5`. The shared `zenoh` / `serialization` / `logging`
blocks behave as everywhere else (see [`CLAUDE.md`](../../CLAUDE.md)), including
the `ZENSIGHT_ZENOH_*` environment overrides, which are applied once inside
`zensight_common::session::connect` and so need no per-crate wiring.

Everything below is under `historian`.

## `key_expr`

The class selector to ingest. Default `v1/*/telemetry/**`.

**Base-relative**, and the config validator refuses a value that spells the
deployment base. Since #466 the session sets the configured base as its Zenoh
namespace, so a selector that includes it matches nothing — and the symptom is a
healthy session with an empty database, which looks exactly like a quiet fleet.
That is the one misconfiguration worth failing at startup rather than
discovering in a week of missing history.

Narrow it to `v1/h-…/telemetry/**` for a per-site historian that should not
ingest the whole fleet.

## `store`

| Key | Default | What it costs |
|---|---|---|
| `path` | `null` | `$STATE_DIRECTORY`, else `$XDG_STATE_HOME/zensight`, else `~/.local/state/zensight/history.redb`. With none of those it runs memory-only and says so. |
| `hot_secs` | `600` | Seconds of per-second samples held **in memory, per series**. Ten minutes is enough for a live chart to be served from the ring; it is also the term that grows with the fleet, and the governor halves it first under pressure. |
| `retention.minute_days` | `2` | Days of minute buckets on disk. Applied by `prune_with` every `prune_interval_secs` (#1063 — for two releases the knob was parsed, validated, logged and never passed to the prune, which ran on the store's own 30-day constant). |
| `retention.hour_days` | `90` | Days of hour buckets. **Not** the GUI cache's 365: a year of hour buckets across a fleet's worth of series is the single biggest term in the file size, and no question has yet been asked of this service that a quarter could not answer. #911 measures it; raise it when a measurement says to, not before. |
| `max_db_bytes` | `2 GiB` | Ceiling on **live** data (`stats.stored_bytes`: rows plus engine metadata). Every prune pass, after the per-tier retention, takes whole days off the oldest end across every tier until live bytes fit. redb reuses the pages a prune frees, so a file at the ceiling stops growing — it does not shrink (compaction needs an exclusive handle; see #911). A pass in which the ceiling removed anything is logged at `warn` and counted in `stats.ceiling_prunes_total`: it means the retention does not fit the disk. `0` leaves only the per-tier retention. (#1064 — until then this key was parsed and read by nothing.) |
| `cache_bytes` | `64 MiB` | redb's page cache. Its own default is **1 GiB** (#625), which on a 1–2 GB VM reads as a slow multi-day RSS climb toward OOM. |
| `batch_size` | `4096` | Samples buffered before a flush is triggered early. |
| `flush_interval_secs` | `10` | Flush regardless of depth. One redb transaction, off the runtime. Floored at 1: running it more often than that turns a batching store into a write amplifier. |
| `prune_interval_secs` | `300` | How often retention runs. Floored at 30 — retention is a slow bound, and a prune every few seconds spends its time proving there is nothing to do. |

`retention.hour_days` must be **at least** `minute_days`. The coarse tier exists
to outlive the fine one; inverted, they leave a window that neither can
answer — old enough that the minute buckets are gone, recent enough that the
hour buckets never covered it. The validator refuses it.

## `resources.budget_rss_mb`

The RSS the memory governor holds the process to (#811/#812). Default absent,
which means **undeclared** — not "fine". With no budget there is no ladder and
no `sensor-budget` alert, and the health document says so rather than implying
health.

The shipped config sets 256. See the README's *Resource budget* section for what
the governor does with it, and `zensight-sensor-core/src/governor.rs` for the
ladder itself.
