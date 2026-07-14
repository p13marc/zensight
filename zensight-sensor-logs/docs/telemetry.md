# logs telemetry

The logs sensor publishes under `zensight/v1/<origin>/…`, where `<origin>` is
the **sensor host's** stable id (`h-<12hex>`); the originating host of each log
line (network or journald) rides in the records and labels, not the key. Two
planes:

1. **Per-line log events** — high-cardinality detail, served **on demand** from a
   bounded ring via a GET on `@rpc/logs/events`, never streamed (#358).
2. **Derived rollups** — cheap low-rate aggregates that ride the telemetry bus for
   charts and alerts.

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative
contract and [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md)
for the canonical reference.

## Per-line events — `@rpc/logs/events` (`Vec<LogRecord>`)

Served as a read procedure at `zensight/v1/<origin>/@rpc/logs/events` (fleet
callers select `zensight/v1/*/@rpc/logs/events` with query target `All`). Each
`LogRecord` (see
`zensight-common/src/query_detail.rs`) keeps the #104 identity — a unique,
time-sortable `uid` (`<timestamp_ms><seq>`, zero-padded) — plus the OpenTelemetry
logs data model:

| Field | Notes |
|---|---|
| `uid` | `<timestamp_ms><seq>` — every line survives (no last-writer-wins loss) |
| `ts` | epoch ms |
| `host` | originating host |
| `facility` / `severity` | syslog facility name / severity name |
| `severity_number` | OTel severity 1–24 |
| `app` / `pid` | app-name / proc-id when present |
| `message` | the log line |
| `labels` | structured fields: `severity_text`, `log.record.uid`, `log.record.original` (when raw kept), journald fields under `sd.journald.*`, `source_type` = `udp`/`tcp`/`unix`/`journald`, template labels, `category=security` for audit/SELinux entries |

Among the journald labels is `sd.journald.invocation_id` (#303) — the unit's
`_SYSTEMD_INVOCATION_ID`, which joins a log line to the exact systemd unit
invocation that produced it (matches systemd sensor `UnitDetail.invocation_id`).

### Selectors

Parameters are Zenoh selector params, **`;`-separated** (not `&`):

```
zensight/v1/*/@rpc/logs/events?since=1719999000000;max=500;source=web01
```

| Param | Meaning |
|---|---|
| `since=<epoch_ms>` | only records with `ts >= since` (inclusive) |
| `max=<n>` | reply cap, newest-first (default 500, clamped to the ring capacity) |
| `source=<name>` | only records from one observed device (a central receiver holds many); `host=` is accepted as a legacy alias |

Ring size is `events_ring_capacity` (default 10 000 ≈ 3 MB, min 100). The GUI
seeds its buffer from this queryable on open and refreshes on a slow tick.

> Pre-#358 sensors streamed each line as a per-uid telemetry key; that legacy
> shape is retired with the v1 migration — per-line detail is pull-only.

## Derived rollups (`derived`, default on)

Emitted every `derived_interval_secs` (default 10) under
`zensight/v1/<origin>/telemetry/logs/` — one sensor-wide series (the metric
names below, each starting with the `logs/` chunk):

| Key | Type | Meaning |
|---|---|---|
| `logs/by_severity/<level>_total` | Counter | per-severity line counts (8 syslog levels) |
| `logs/errors_total`, `logs/warnings_total` | Counter | error / warning totals |
| `logs/by_unit/<unit>/messages_total`, `.../errors_total` | Counter | top-N per-unit rollups (capped to `top_units` + an `other` bucket) |
| `logs/units_in_failure` | Gauge | units currently in a failure/error state (windowed) |
| `logs/journald/{read,published,dropped,sampled_out}_total` | Counter | journald throughput accounting |

### Ingest accounting (`logs/ingest/*`, #106)

Network-path loss accounting, parity with journald: `logs/ingest/{received,
parsed,parse_failed,dropped}_total`. A sustained-loss `ErrorReport` is raised once
the dropped fraction over a window exceeds `ingest.drop_alert_ratio`.

### Template series (`templating`, default on, #102)

Drain3-style mining masks variables and clusters each line into a stable template
(`template_id` / `template` labels on the per-line records), emitting bounded
`logs/by_template/<id>/{count,errors}_total` series (top-N + an `other` bucket).

### Error-budget gauges (`error_budget`, #105)

Layered on the per-unit rollups: `logs/by_unit/<unit>/error_ratio` (window
`errors/messages`) and `logs/by_unit/<unit>/burn_rate` (× budget). Emitted even
when alerting is disabled (cheap + bounded).

## Alerts — `state/logs/alert/<alert_key>`

Lifecycle alerts on `zensight/v1/<origin>/state/logs/alert/<alert_key>`
(`<alert_key>` = 16-hex FNV-1a of rule + labels; firing = Put, resolved =
Put(Resolved) then a Delete tombstone):

| Rule | Source | When |
|---|---|---|
| `log-error-budget` | `error_budget.enabled` (#105) | a unit burns budget (`error_ratio > target_ratio * burn_rate`) for `burn_windows` consecutive windows |
| `log-novelty` | `novelty.enabled` (#103) | a never-before-seen template shape appears after warm-up |
| `log-rate-spike` | `novelty.enabled` + `rate_spike_multiplier > 1` | a known template's window rate jumps N× over its EWMA baseline |
| journald known-events | `journald.detect_events` (#61) | coredump / unit-failed / OOM matched by `MESSAGE_ID` |

Late joiners seed the firing set with a plain GET on the same
`state/logs/alert/*` selector (a storage-shaped queryable answers one reply per
firing key). See
[configuration.md](configuration.md) for the tuning knobs and
[filtering.md](filtering.md) for journald event detection.
