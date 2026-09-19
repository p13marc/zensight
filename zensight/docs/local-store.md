# Local store

The frontend keeps a bounded local store of telemetry so history survives
restart without growing unbounded on disk. Since #904 the store itself is a
crate — [`zensight-store`](../../zensight-store/README.md) — and that README is
the design reference: tiers, retention, interning, the schema, the log and
event tables, and the async discipline. This page is what the *GUI* does with
it.

## It is a cache, not the history

The store the frontend opens (`~/.local/share/zensight/metrics.redb`) holds
only what this viewer saw while it was running, template-sampled. On a fleet
the GUI is open for minutes a week, which is exactly why the durable fleet
history moved to a headless service (#898): the `zensight-historian` subscribes
`v1/*/telemetry/**` continuously and serves `@rpc/historian/range`.

Both write through the same crate, and since v3 both name a series the same
way — `<origin>/<producer>/<subject>`, the wire key minus the class chunk — so
a chart reads the fleet's history when a historian is alive and falls back to
this cache when none is (#909).

Which side answered comes from the **liveliness roster**: a historian's token
appearing flips every chart to the fleet, and its disappearing flips them back.
Probing instead would cost each chart a full GET timeout to learn a standing
fact the roster already knows.

A locally-sourced chart says so, above the content: *"Fleet history unavailable
— showing this viewer's local cache only"*. The two look identical otherwise,
and the difference is whether the window on screen is one viewer's or the
fleet's.

**The cache is rebuilt, not migrated, when its layout changes.** v3 re-typed
`metrics` and `samples`, so an older file is moved aside
(`metrics.redb.schema-v2`) and a fresh one started, with a logged warning. That
is the right trade for a cache whose contents a fleet service also holds — and
the store is never fatal: if the file cannot be opened at all, the GUI keeps
the hot rings and says so in the log.

## What bounds the hot rings (#1115)

Three mechanisms, and only one of them is the backstop.

| | |
|---|---|
| **Lazy allocation** | a ring allocates what it holds, not its capacity. It used to reserve 3 600 × 16 B the moment a metric was first seen |
| **Eviction** | a device reaped from the dashboard takes its series with it, and a series idle for `SERIES_IDLE_TTL_MS` (2 h) is dropped on its own |
| **`MAX_HOT_SERIES`** | 40 000, a hard ceiling. New series past it are **refused and counted**, never silently dropped |

The numbers matter. The GUI subscribes `v1/*/telemetry/**` by default and on a
50-host fleet reaches 10–15 k series — sysinfo per CPU, per mount, per
interface; systemd per unit; container, probe and pve with churning
`{name}`/`{target}`/`{vmid}` chunks. At 3 600 pre-allocated slots each, the
**reserve alone was 600–860 MB**, most of it for rings holding a handful of
samples.

The two evictions answer different questions and both are needed. A *device*
eviction reaps a host that went away for good; it runs at 24 h, because a
known-down host's card should stay visible. A *series* eviction reaps a series
whose device is still very much alive — a container that ran for an hour, a
probe target removed from the config, a guest destroyed. It runs at 2 h:
longer than any chart window the GUI offers, so nothing is reaped out from
under something on screen, and short enough that a day of churn does not
accumulate.

**A series with unflushed samples is never evicted**, whatever its age.
Dropping it would discard history the flush is about to write — a different and
worse bug than the one this fixes. It goes on the next sweep, after the flush.

`MAX_HOT_SERIES` is a backstop for a label explosion, not the mechanism. It is
roughly three times what a 50-host fleet reaches, so a deployment that is
merely large never meets it; one that does has something wrong upstream, and
`MetricStore::refused_series()` is a **counter** rather than a flag because the
rate is the diagnosis — one over the cap and four hundred a minute are
different problems.

Interner ids are **not reused** when a series is evicted. They are dense
ordinals that name rows in the samples table, and reusing one would silently
re-label somebody else's history; the hole costs 24 bytes. `live_len()` is what
is currently named, `len()` is the id space, and the two diverge by design.

## Where the GUI reads it

| Surface | Reads |
|---|---|
| Device detail chart | the fleet historian when one is alive, else the minute tier (or the **hour** tier past two days) for the requested window, plus live hot samples |
| Dashboard sparklines | the hot ring only (`device_hot_samples`), per render |
| Topology edge rates | the hot ring, through `zensight_store::rate::counter_rate` |
| Logs view | the `logs` table (see below) |
| Trap/event feed | the `events` table, seeded at boot |
| Debug-report download | the `chunks` table, via `RedbContentStore` (the `blob` feature) |

Every redb read runs off the Iced update thread via `Task::future` +
`spawn_blocking`; the hot-ring append is O(1) and runs inline.

## Log events

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

The `events` table is seeded at **boot** rather than on view open, because the
trap feed is on the dashboard — the first screen — so waiting for a view switch
would show an empty feed on every launch (#578). It arrives as
`Message::SnmpEventHistoryLoaded`.
