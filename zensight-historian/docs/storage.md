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

**Not yet the fourteen-day soak.** #911's acceptance is "measure via
`@rpc/historian/stats` after 14 days on the six-VM fleet", and that is elapsed
time, not work. What follows is what a short run establishes; the issue stays
open for the rest.

**One host, one sysinfo sensor, ~5 minutes** (dev box, debug build,
2026-09-02) — the run that first proved the service works end to end:

| | Observed |
|---|---|
| Series | 318 |
| Rows | 17 446 second · 1 908 minute · 318 hour |
| Database | 5 083 136 bytes |
| Bytes per row (all tiers) | ~258 — **but see below** |

That byte figure is not the acceptance number and must not be read as one. A
five-minute file is dominated by redb's allocated-but-unused pages and by the
`metrics` table's one row per series; the per-bucket cost only emerges once the
tiers hold far more buckets than the file has slack. It is recorded because a
number with its conditions stated is worth more than no number, and because it
is the baseline the fleet run will be compared against.

**Survives a restart**: the same 318 series and their buckets reopened from the
file after a `SIGTERM` and a fresh start, which is the property the whole epic
exists for.

## Outstanding

- The fourteen-day, six-VM soak, and the six acceptance numbers above measured
  against it.
- A synthetic-load harness that reaches 10 000 series in minutes, so the
  cardinality target can be tested without waiting for a fleet to grow into it.

Both are tracked in #911.
