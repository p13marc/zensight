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
