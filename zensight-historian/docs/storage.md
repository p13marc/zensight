# Storage: tiers, retention, and what they cost

The tiers are [`zensight-store`](../../zensight-store/README.md)'s — hot ring in
memory, minute and hour buckets in redb. This page is what they cost when a
fleet writes to them, and how that was measured.

## The shape

| Tier | Resolution | Where | Default retention |
|---|---|---|---|
| Hot | per-second | memory, per series | `hot_secs` (600) |
| Warm | per-minute | redb `samples` | `retention.minute_days` (2) |
| Cold | per-hour | redb `samples` | `retention.hour_days` (90) |

A bucket is `{last, min, max}`: 16 bytes of value plus the packed
`(metric_id, tier, bucket_ts)` key. `last` is the value and the tier semantics
are last-observation-per-bucket; the range is what lets a coarse tier say a
spike happened at all, rather than reporting only where the value landed on the
hour.

Retention runs every `prune_interval_secs`, walking `(metric, tier)` by
`(metric, tier)` and extracting each one's aged range — not scanning the file.

## Acceptance numbers (#911)

These are the numbers the reference fleet — a Proxmox host and six 1–2 GB VMs —
must meet at the shipped defaults. Every retention default in the store was
originally chosen on a laptop, and that fleet is where the sensor bundle
OOM-killed a VM on 2026-08-17; the historian must not be the next one.

| | Target |
|---|---|
| Series | ≤ 10 000 |
| Bytes per minute bucket on disk | ≤ 48 |
| Database at defaults | ≤ 2 GiB (else `max_db_bytes` prunes early) |
| RSS steady, `budget_rss_mb = 256` | ≤ 256 MiB, no ladder step above L1 |
| Prune wall time | ≤ 2 s |
| `range` GET p95, 24 h at the minute tier | ≤ 200 ms |

## What has been measured

**Not the fourteen-day soak.** #911's acceptance is "measure via
`@rpc/historian/stats` after 14 days on the six-VM fleet", and that is elapsed
time, not work. What follows is what a bench establishes in minutes; the issue
stays open for the rest.

`cargo run --release -p zensight-historian --example historian-bench` writes
through `MetricStore::record` and `take_flush_batch` — the real ingest seam, so
what it measures is what the historian would write, interning and downsampling
included.

### 10 000 series, 120 simulated minutes (dev box, release build, 2026-09-02)

| | Observed | Target | |
|---|---|---|---|
| Series | 10 000 | ≤ 10 000 | at the target |
| Minute buckets | 1 200 000 | — | |
| Hour buckets | 30 000 | — | |
| Database | 269 MB | — | |
| Bytes per bucket, as written | **219** | ≤ 48 | **4.6× over** |
| Bytes per bucket, after `compact()` | **89** | ≤ 48 | **1.9× over** |
| Prune, worst case | **7.0 s** | ≤ 2 s | **3.5× over** |
| Range GET p95 | 0.01 ms | ≤ 200 ms | far inside |

The prune figure is deliberately the **worst case**: every bucket aged out at
once. In steady state a pass finds only what expired since the last one, which
is why the historian runs it every five minutes rather than every flush.

### The per-second tier was being persisted, and is not any more

The bench's first run wrote 1.2 M *second* buckets alongside 1.2 M minute
buckets — half the rows in the file, for a resolution the hot ring already
serves, since a sub-minute `step` reads the ring and not the disk. The config
and this page had said since #906 that the per-second tier "is not persisted";
measuring is what made it true. `MetricStore::persist_tiers` now takes the tier
set, the historian sets `[Minute, Hour]`, and the same run halved the database
(539 MB → 269 MB) and cut the prune from 22.4 s to 7.0 s.

That is what #911 is for, and it is worth noting that nothing else would have
found it: the code was doing what the store was written to do, and only a
number said it was the wrong thing to do at fleet scale.

### Two numbers still miss, and the defaults have not been changed

Bytes-per-bucket and prune time are both over target. Extrapolating the
observed density, two days of minute buckets at 10 000 series is far past the
2 GiB `max_db_bytes` ceiling.

The defaults are **not** adjusted here, deliberately. Which lever to pull —
shorter minute retention, a coarser base tier, a smaller cardinality budget, or
a schema change to shrink the per-bucket cost — depends on what the fleet's
real cardinality turns out to be, and the bench's synthetic series (one per
minute, every series active every minute) is the densest possible case. A real
fleet's series are sparser and arrive unevenly. Changing a retention default on
the strength of a synthetic worst case would be tuning for a load nobody runs.

### One measurement withdrawn, and why it should not have been

An earlier version of this page reported that `redb::Database::compact` took
the file from 269 MB to 6 MB, then withdrew the figure because `tier_rows` and
an ad-hoc external scan of the same file disagreed about how many minute
buckets it held. The disagreement was described here, and on #911, as
unexplained, and `tier_rows` was named as a possible cause.

**That was wrong, and the cause was a bug in the bench.** `--ingest-only`, the
flag whose whole purpose is to hand an external reader a file as ingest left
it, returned *after* the prune rather than before it. `prune_at` is chosen so
that every minute bucket ages out at once, so the file being called "closed and
consistent" had just had its entire minute tier deleted — and the `removed:`
line that would have shown it was skipped by the very return that came too
late. Both readers were right about the file each looked at. The message was
wrong about which file that was.

The numbers reconcile exactly, which is the proof: 2 000 series × 120 minutes =
240 000 minute buckets, and `base_ms` sits 800 s into an hour so a 7 200 s span
touches 3 hour boundaries — 2 000 × 3 = 6 000. Post-fix, an independent reader
of the same file reports precisely those two numbers. **`tier_rows` was correct
throughout, and `@rpc/historian/stats` was not over-reporting to anyone.**

The bench now returns before the prune, prints `removed:` on every path that
prunes, and asserts that compaction leaves the row count unchanged — the check
that would have caught the original mistake, since it is exactly the arithmetic
that hides a file whose contents changed underneath a size measurement.

### Compaction reclaims more than half the gap

With the ordering fixed the measurement is worth having, and it changes what
the 4.6× miss means:

| | Database | Bytes per bucket |
|---|---|---|
| As ingest leaves it | 269 MB | 219 |
| After `compact()` (340 ms) | 109 MB | **89** |

So roughly 60% of the per-bucket cost is reclaimable slack, not schema. The
original 44× figure was `compact()` reclaiming a file that had just had 1.2 M
rows deleted — an ordinary post-mass-delete reclaim, and not a property of the
schema at all.

This does not clear the target: 89 B/bucket still misses ≤ 48 by 1.9×, so a
schema change is not off the table. But it does say that the first lever to
reach for is a compaction pass on a timer — 340 ms to halve the file — rather
than a retention default. The defaults are still not changed here, for the
reason in the section above: the bench's series are the densest possible case.

## Outstanding

- The fourteen-day, six-VM soak, and the six acceptance numbers measured
  against it — in particular the steady RSS under the governor's ladder, which
  no bench reproduces: it depends on how many series are *active* at once and
  how fast they arrive, not on how many exist.
- Whether a periodic `compact()` belongs in the prune timer, and at what
  interval — it halves the file in 340 ms at bench scale, but it takes an
  exclusive handle, so the cost is a pause in ingest rather than CPU.
- Whether the two failing numbers survive a real fleet's cardinality, and which
  default to move if they do.

All tracked in #911.
