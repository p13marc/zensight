# `@rpc/historian/range` and `/series`

Two read procedures, both fleet-fannable. Build the selector with
`zensight_common::keyexpr::historian_range_selector()` /
`historian_series_selector()` rather than a `format!`, and **target `All`**:
several historians may answer (one per site is the expected deployment), each on
its own concrete key, and `BestMatching` short-circuits to whichever replied
first and silently drops the rest of the fleet's history.

## `range`

Three decisions the server makes and the reply states — which series, at what
resolution, reduced how — plus two bounds. A reply that left any of them
implicit would be a chart nobody could check.

### Parameters

Zenoh `Parameters` are `;`-separated.

| Param | Default | Meaning |
|---|---|---|
| `origin` | `*` | `h-<12hex>`, or `*` for the fleet. |
| `producer` | `*` | `sysinfo`, `snmp`, … |
| `subject` | `**` | A key expression **within** the producer. `*` and `**` mean what they mean everywhere else on this bus. |
| `from` / `to` | last hour | Epoch ms, inclusive. |
| `step` | `60` | Seconds per point. Clamped to a tier — see below. |
| `agg` | by kind | `raw \| avg \| min \| max \| last \| rate`. |
| `limit` | 5 000 | Points **across all series**; hard ceiling 20 000. |
| `cursor` | — | Opaque. Pass back what the previous reply gave. |

`origin`, `producer` and `subject` compose one key-expression pattern matched
against the series path (`<origin>/<producer>/<subject>`), which is why the
matching is `zenoh::key_expr` intersection and not a string prefix.

### Step and tier

| `step` | Tier | Where it reads |
|---|---|---|
| `< 60` | per-second | the in-memory hot ring — the per-second resolution is never flushed as its own tier, so a sub-minute step is answered from memory or not at all |
| `< 3600` | per-minute | redb |
| `≥ 3600` | per-hour | redb |

**The reply states the `step_s` actually served.** A caller that asks for one
second over a month gets the hour tier; being told is the difference between a
coarse chart and a wrong one. Asking a coarse tier for a fine step would return
one bucket repeated across the step, which reads as data and is not.

### Aggregates

`agg` defaults **by kind**, per series, and the reply names the aggregate
applied to each — the default differs between series in one reply.

| Kind | Default | Why |
|---|---|---|
| counter | `rate` | The mean of a monotonically climbing number answers nothing. |
| gauge | `avg` | |
| bool | `max` | Over a minute the question is "did it flap at all"; an average of 0.03 hides a bounce that `1` states. |

`min` and `max` read the **bucket's own range**, which is why the store keeps
one: a coarse tier that reported only its closing value could not say a spike
happened.

`rate` is computed on the underlying series and *then* averaged into the step,
not the other way round — a rate of an average of a counter is not a rate of
anything, and the two differ whenever a step holds more than one bucket, which
is every coarse query. A counter reset restarts the accumulation from zero
(Prometheus' rule), so no step ever reports a negative rate.

`raw` is every stored bucket, unreduced — still bounded by `limit`. "Raw" is not
"unbounded".

### Bounds and paging

An unbounded query against a year of history is a denial of service with extra
steps. `limit` caps the points across all series; `truncated` and `next_cursor`
say when it bit. **A short page or a null cursor is the end** — the same
contract `@rpc/logs/events` has.

The cursor is opaque and must not be parsed: pass back exactly what the reply
gave. A malformed or stale one restarts at the beginning rather than erroring —
a cursor from a previous build should cost a repeated page, not a failed query.

Series are ordered by `(origin, producer, subject)` so a cursor means the same
thing on the next call; an unstable order would make paging silently skip and
repeat as the fleet interned new series between pages.

### Errors

Malformed values **default**, matching `@rpc/logs/events` — a fat-fingered
`limit` should get a page, not a rejection. The two exceptions are where a
default would be a silent wrong answer:

- an unrecognised `agg` → `error/invalid-args` (a misspelling that quietly
  became `avg` would be a wrong chart with nothing to notice);
- a `to` that precedes its `from` → `error/invalid-args` (an empty reply and an
  inverted window look identical to a chart).

### Example

```bash
zenctl get -c tcp/127.0.0.1:7447 \
  'v1/*/@rpc/historian/range?producer=sysinfo;subject=network/**;from=1788350000000;to=1788350300000;step=60'
```

```json
{
  "historian": "h-0ead7da13eea",
  "from": 1788350000000, "to": 1788350300000, "step_s": 60,
  "truncated": false, "next_cursor": null,
  "series": [
    { "origin": "h-0ead7da13eea", "producer": "sysinfo",
      "subject": "network/eth0/rx_bytes", "kind": "counter",
      "agg": "rate", "unit": null, "source": "vm-dev-01",
      "points": [[1788350040000, 1043.2]] }
  ]
}
```

## `series`

What this historian holds: `?origin=;producer=;subject=;limit=`. The same
key-expression matching, and the call a caller makes before it can ask a
sensible range question.

`subject` keeps a proxy producer's leading device chunk (`sw1/if/3/in_octets`)
while `source` names the observed device — neither is recoverable from the
other, which is why the store persists both.

## A note on `unit`

`unit` is UCUM-style and **absent means unknown, never dimensionless**. Today
only the SNMP sensor declares units on its telemetry points, so most series read
`null`. It is stored per series (schema v4) rather than taken from a live sample
because the caller that most needs it is the one that cannot get it that way: a
chart opening on a fleet whose sensors are quiet.
