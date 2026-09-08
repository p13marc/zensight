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

### Bounds, paging and coverage

An unbounded query against a year of history is a denial of service with extra
steps. `limit` caps the points across all series.

The reply carries the **RFC 05 §3.2 envelope fields** (#1067):

| Field | Says |
|---|---|
| `partial` | The answer is less than the question — the `limit` bit, *or* the tier could not cover the window. Always present. |
| `next_cursor` | Where to resume, or `null` at the end. |
| `scanned` | Buckets read to build this page, so an expensive empty answer can be told from a cheap one. |
| `covers_from` | The oldest instant the chosen tier could have answered for, as an **RFC 3339 string** — present only when that is later than `from`. |
| `truncated` | The `limit` bit. Superseded by `partial`; kept for one release. |

`partial` is the marker, and it is the one that matters:
`zenkey_fleet::CallAnswer::page_signal()` — what `zenctl call` and the RFC 13
judges read — keys off a boolean field spelled exactly that and nothing else.
For as long as `truncated` was the only signal, this reply was not a *bad*
envelope, it was not seen as one at all.

**Coverage is not truncation.** A sub-minute `step` is served from the hot ring,
which holds minutes. A caller asking twenty-four hours at `step=10` used to get
whatever the ring held with `truncated: false` and `next_cursor: null` — and a
null cursor is the end, as this page says two paragraphs down. The reply echoes
`from`/`to` unchanged, so a chart drew a 24-hour axis with ten minutes of data
at the right edge and no gap marker. `covers_from` is that statement, and it is
a string because the generic reader takes it with `as_str()`: epoch millis is
read as *absent*, silently, by exactly the tooling that would report the gap.

`partial: true` with `next_cursor: null` is a contract violation an observer MAY
report — **unless** the reply states `covers_from`. A gap in time has no next
page; it says why instead.

The cursor is opaque and must not be parsed: pass back exactly what the reply
gave. A malformed or stale one restarts at the beginning rather than erroring —
a cursor from a previous build should cost a repeated page, not a failed query,
and that now includes the positional cursors this procedure issued before
#1068.

Series are ordered by `(origin, producer, subject)`, and **the cursor is a value
in that order**, not a position in it (#1068). Sorting keeps the order stable,
not the indices: a series interned before the cut used to shift everything right
so page two re-read one, and retention removing one shifted left so page two
skipped one — neither signalled. Naming the series the page stopped in makes
both cases correct, and a series that has disappeared between pages resumes at
the first that sorts after it.

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
  "truncated": false, "partial": false, "next_cursor": null, "scanned": 5,
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
