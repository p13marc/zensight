# ZenSight → Rerun data mapping (#417)

How each ZenSight domain concept lands in Rerun 0.34 (API names as pinned in
[01-capabilities.md](01-capabilities.md)). The mapping is implemented Rerun-free in
`zensight-rerun/src/mapping.rs` / `events.rs` / `topology.rs` (pure functions, unit-tested)
and consumed by the single Rerun-aware module `rerun_sink.rs`.

## 1. Entity-path scheme

Rerun entity paths are the viewer's tree; they carry the correlation story.

| ZenSight concept | Rerun entity path |
|---|---|
| Telemetry, source correlated to a host | `hosts/<entity_id>/<protocol>/<metric...>` |
| Telemetry, source not (yet) correlated | `sensors/<protocol>/<source>/<metric...>` |
| Normalized event (per host) | `hosts/<entity_id>/events` (fallback `sensors/<protocol>/<source>/events`) |
| Alert lifecycle | `alerts/<protocol>/<rule>` |
| Sensor health transitions | `health/<sensor>/<source>` |
| Topology graph | `topology/hosts` (nodes+edges, see [09-topology.md](09-topology.md)) |

- `<entity_id>` is the correlator's stable `h_<12hex>` (`HostEntity.entity_id`); the join from
  `(sensor, source)` → entity is exactly `HostEntity.members[]` (`zensight-common/src/entity.rs`),
  maintained live from the `zensight/_meta/entity/**` subscription in an `EntityIndex`.
- The **fallback path is not a temporary alias**: once a source becomes correlated, *new*
  samples move to the `hosts/...` path; data already logged under `sensors/...` stays where it
  was (Rerun entities are append-only histories). The viewer shows the seam. Re-keying history
  would require rewriting the recording — out of scope, recorded as an ergonomics finding.
- **Alias handling**: after a correlator merge/upgrade, the surviving entity keeps its
  `entity_id` and prior ids move to `aliases`. The `EntityIndex` maps member claims of an
  aliased id to the surviving id, so telemetry follows the merge for new samples. Same seam
  caveat as above.
- Metric paths reuse ZenSight metric chunks verbatim (`cpu/usage` →
  `hosts/h_.../sysinfo/cpu/usage`); they are already key-expression-safe, so no Rerun path
  escaping is needed.

## 2. `TelemetryPoint` → time series

`TelemetryPoint { timestamp: i64 ms, source, protocol, metric, value, labels }`
(`zensight-common/src/telemetry.rs`).

| `TelemetryValue` | Mapping |
|---|---|
| `Gauge(f64)` | `Scalars::single(v)` on the metric path |
| `Counter(u64)` | rate-converted (below), then `Scalars::single(rate)` |
| `Boolean(bool)` | `Scalars::single(1.0 / 0.0)` — step-plot readable |
| `Text(String)` | **not** a scalar → normalized event, see §4 |
| `Binary(_)` | dropped (counted in adapter stats) — no meaningful Rerun rendering |

Example: `zensight/sysinfo/host1/cpu/usage` = `Gauge(42.5)`, source `host1` correlated to
`h_0123456789ab` →

```text
set_timestamp_nanos_since_epoch("zensight_time", 1_752_192_000_000 * 1_000_000)
log("hosts/h_0123456789ab/sysinfo/cpu/usage", Scalars::single(42.5))
```

`SeriesLines` styling (name, color, width) is logged once per series with `log_static` on the
same path, on first sight of the series.

### Counter → rate policy

Raw monotonic counters plot as ever-growing ramps — useless next to gauges. Policy
(config `rerun.counters`, default `rate`):

- `rate`: emit **per-second rate** `(v1 - v0) / (t1 - t0)`; timestamps are ms → scale by 1000.
  - First sample of a series: **no emission** (nothing to differentiate against).
  - **Reset detection** (`v1 < v0`, e.g. process restart or counter wrap): no emission,
    re-arm on the new baseline — one silently absorbed gap instead of a huge negative spike.
  - Non-advancing clock (`t1 <= t0`): no emission (avoids div-by-zero / nonsense spikes).
- `raw`: emit the counter value as-is (debugging).
- `both`: `.../<metric>` gets the rate, `.../<metric>/raw` the raw value.

The converter keys state by the **concrete series** (entity path), so two hosts' `rx_bytes`
never cross-contaminate.

## 3. Timestamps & clock skew

- Every `log` call is preceded by `set_timestamp_nanos_since_epoch("zensight_time", ms * 1e6)`
  using the **domain timestamp** (`TelemetryPoint.timestamp`, `Alert.timestamp`, script time
  for demos) — never adapter receive time. This is what makes multi-sensor incident replay
  line up.
- Rerun's auto-injected `log_time` (receive time) is left enabled as a free diagnostic: the
  *difference* between `log_time` and `zensight_time` visualizes sensor→adapter latency and
  producer clock skew. We do not correct skew — ZenSight sensors already stamp at source and
  fleet-time discipline (NTP) is an operational assumption, same as for the Iced frontend.
  Out-of-order arrivals are fine: Rerun chunks are sorted/compacted by the store, not by
  arrival ([RRD format](https://rerun.io/docs/concepts/logging-and-ingestion/)).

## 4. Text telemetry & structured events → `TextLog`

`NormalizedEvent` (adapter-internal, Rerun-free — `events.rs`) is the single event shape for:
`Text` telemetry, `events/...`-keyed telemetry (netlink control-plane timeline: route changes,
ipsec, link up/down), alert transitions, and health-status transitions.

Emission: `TextLog::new(message).with_level(level)` on the event path, plus an `AnyValues`
bundle on the same path carrying the structured attributes (kind, target, interface,
correlation_id, protocol, plus the event's own labels) so the viewer's selection panel shows
them and dataframe queries can filter on them.

## 5. Alerts — lifecycle mapping

ZenSight alerts (`zensight-common/src/alert.rs`) are keyed state machines:
`Put(Firing)` → `Put(Resolved)` → `Delete` tombstone on `zensight/<protocol>/@/alerts/<alert_key>`.

Rerun has no retract/delete of previously logged rows (Clear exists for entity *visualization*
state, not for time-series history) — so alerts map to **transition events + a level series**:

- `alerts/<protocol>/<rule>`: `TextLog` per transition —
  `Firing`: `"[FIRING] <summary>"` at severity level; `Resolved`: `"[RESOLVED] <summary>"`
  at `INFO`. Attributes carry `alert_key`, `source`, `kind`, labels, correlation id.
- `alerts/<protocol>/<rule>/state`: `Scalars` step series — severity weight while firing
  (info 1, warning 2, critical 3), `0.0` on resolve. This gives a timeline lane where firing
  windows are visible as plateaus, the closest Rerun analogue to the frontend's alert rows.
- **Delete-tombstone note**: the Zenoh `Delete` carries no payload; the prior `Resolved` Put
  already produced the resolved event, so tombstones are ignored (same as the OTel exporter,
  `zensight-exporter-otel/src/subscriber.rs`). Consequence: an alert that is *tombstoned
  without a Resolved Put* (not the sensors' contract, but possible) would leave the state
  series stuck at its firing level — recorded as a limitation.

### Severity → level/color

| `AlertSeverity` / event severity | `TextLogLevel` | state weight |
|---|---|---|
| `Info` | `INFO` | 1.0 |
| `Warning` | `WARN` | 2.0 |
| `Critical` | `CRITICAL` | 3.0 |
| (event-only) `Debug`-ish | `DEBUG` | — |
| resolved | `INFO` | 0.0 |

Colors are left to the viewer's level defaults; we don't hand-pick per-severity colors in
the sink (viewer-side assessment item).

## 6. Correlation ids

`NormalizedEvent.correlation_id` (e.g. the incident demo's `inc-<base_ts>`) is emitted as a
plain string attribute in the `AnyValues` bundle of every event that carries it. Rerun has no
first-class "trace" linking; cross-entity correlation is *visual* (shared timeline cursor) and
*queryable* (dataframe filter on the attribute). This is honest: finding "everything for
incident X" is one filter, but there is no click-through link between correlated items —
ergonomics to assess on the GPU box.

## 7. `HealthSnapshot` → health transitions

`zensight/*/*/@/health` → `HealthSnapshot` (`zensight-common/src/health.rs`). The adapter
tracks last status per `(sensor, source)` and emits a `NormalizedEvent`
(`EventKind::HealthChange`) only on **transitions** (Healthy→Degraded etc.), at level
`WARN`/`ERROR` by target status. Steady-state health snapshots (~every few seconds per sensor)
are deliberately not logged — they'd be noise in a text log.

## 8. `HostEntity` → roots + topology

- On entity arrival/update, `log_static` an `AnyValues` bundle at `hosts/<entity_id>` with
  hostname, fqdn, ips, macs, status, member count — so selecting the host root in the viewer
  shows its identity card.
- Nodes/edges for the topology graph view are derived in `topology.rs`
  (see [09-topology.md](09-topology.md)).
