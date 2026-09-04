# Prometheus exporter reference

The exporter subscribes to ZenSight telemetry over Zenoh, aggregates it into
Prometheus time series, and serves them on a pull `/metrics` endpoint (and,
optionally, pushes them via remote-write). Config lives in
[`../../configs/prometheus-exporter.json5`](../../configs/prometheus-exporter.json5);
this page is the mapping/behavior reference.

## Subscription

Two independent subscribers:

- **Telemetry** on `filters.key_expr` (default `v1/*/telemetry/**`,
  the v1 telemetry class selector). The class chunk *is* the filter — state
  documents and the verbatim `@rpc`/`@media`/`@blob` planes structurally never
  match, so nothing is discarded client-side. Narrow it — e.g.
  `v1/*/telemetry/netring/**` — to tame the firehose at the
  *subscription*, so unwanted producers never reach the exporter over the wire
  (a structural v1 check, `is_telemetry_key`, stays as belt-and-braces for
  narrowed overrides).
- **Alerts** on `v1/*/state/*/alert/*` (only when `export_alerts` is
  on). Alerts are state-class keys, so the telemetry class selector cannot see
  them — alert export needs its own subscriber.

`include_protocols` / `exclude_protocols` / `include_metrics` / `exclude_metrics`
(glob) / `include_sources` / `exclude_sources` apply as a post-receive filter on
top of the subscription.

## TelemetryPoint → Prometheus mapping

Metric name: `{prefix}_{protocol}_{metric_path}` (prefix default `zensight`).
Host-metrics keys covered by the OTel semantic-conventions map (#100) instead
export under their `system.*` name with no protocol segment, and their
state/direction/device become labels.

| `TelemetryValue` | Prometheus type |
|------------------|-----------------|
| `Counter(u64)` | counter |
| `Gauge(f64)` | gauge |
| `Boolean(bool)` | gauge (0/1) |
| `Text(String)` | info (value 1, text carried as a label) |
| `Binary(Vec<u8>)` | not exported |

### Name and label sanitization

- **Metric names** are forced to match `[a-zA-Z_:][a-zA-Z0-9_:]*`: invalid chars
  → `_`, consecutive underscores collapsed, trailing underscores trimmed, a
  leading digit prefixed with `_`, empty → `unnamed`. So `if/3/in_octets` →
  `if_3_in_octets`, `disk[sda]` → `disk_sda`. (The old example here was
  `system/sysUpTime` → `system_sysUpTime`, demonstrating case preservation —
  true of the sanitizer, but no shipped metric name exercises it since #559.)
- **Label names** match `[a-zA-Z_][a-zA-Z0-9_]*`; a name starting with the
  reserved `__` is prefixed with `z`; empty → `label`.

Every series carries `source` and `protocol` labels, plus the point's own labels
and any `default_labels` from config. Reserved `source`/`protocol` in the point's
own labels do not override the built-ins.

## Staleness and memory bounding

The collector stores one `StoredMetric` per unique series key.

- **Staleness.** A background task runs every `cleanup_interval_secs` and drops any
  series untouched for `stale_timeout_secs` (default 300 s), so a device that stops
  reporting stops appearing in `/metrics`.
- **Series cap.** New series are rejected once `max_series` (default 100 000) is
  reached (counted in `points_dropped_max_series`), bounding memory against a
  cardinality explosion.

`render()` groups series by name and emits a `# TYPE` comment per group (`# HELP` is currently emitted only for
alerts — see #768)
in the standard `text/plain; version=0.0.4` exposition format.

## Endpoints

| Endpoint | Purpose |
|----------|---------|
| `/metrics` (configurable `path`) | Prometheus exposition format |
| `/health` | always 200 |
| `/ready` | 200 once telemetry has been received (Kubernetes probe) |

Self-metrics on `/metrics`: `zensight_exporter_series_total`,
`zensight_exporter_points_received_total`, `..._points_accepted_total`,
`..._points_filtered_total`.

## Remote-write (push)

Set `remote_write.enabled: true` with a `url` for push-based / agent topologies
where the backend (Grafana Cloud, Mimir, Thanos Receive, VictoriaMetrics) cannot
scrape the exporter. Every `interval_secs` (default 30) the collector's current
state is snapshotted and POSTed as a snappy-compressed protobuf `WriteRequest`
(Prometheus remote-write 1.0): `Content-Encoding: snappy`,
`Content-Type: application/x-protobuf`, `X-Prometheus-Remote-Write-Version: 0.1.0`.
Extra `headers` (e.g. `Authorization`, `X-Scope-OrgID`) are attached to each push.

- One sample per live series, stamped with the push time — exactly what a scrape
  at that instant would produce. Info (text) series are sent as value `1` with the
  text in a `value` label.
- Alert series and the exporter's self-metrics stay on the **pull** endpoint only.
- The `/metrics` endpoint keeps serving regardless.
- Validation requires an `http(s)` URL and a non-zero interval when enabled.
- The protobuf types are hand-written with `prost` derive, so no `protoc` is
  needed at build time. Exemplars are deliberately omitted (would need a
  histogram-shaped value type and a real trace id, neither of which the bus
  carries).

## Alert export

With `export_alerts` on (default), each **firing** alert from
`zensight/v1/*/state/*/alert/*` becomes
one `<prefix>_alert` gauge series with value 1:

```
# HELP zensight_alert ZenSight sensor alert (1 = firing; series absent once resolved).
# TYPE zensight_alert gauge
zensight_alert{source="host01",rule="socket-missing",severity="critical",…} 1
```

Labels carry the alert's `source`, `rule`, `severity`, its own labels (reserved
names are not overridden), and **`acked`** (#926 — see below). The series
disappears when the alert resolves or its sensor tombstones it, so Alertmanager
treats absence as resolved.

Alerts are **not** staleness-swept, unlike metrics, and the difference is
load-bearing (#758). Sensors publish alerts edge-triggered: a firing alert is
put once, and again only to resolve. A 300 s sweep therefore removed every
alert older than five minutes — and since absence *is* the resolve signal, that
closed live incidents in Alertmanager. A firing alert now leaves the store for
three reasons, all of them real events: a `Resolved` put, a `Delete` tombstone,
or its sensor's **liveliness token vanishing**.

The store is keyed by **`(origin, alert_key)`**, not by the hash alone. Since
epic #453 the hash excludes the source — the wire key's origin chunk scopes it
— so two hosts firing the identical rule share an `alert_key`, and a
hash-keyed store showed one of them.

## Incidents and acknowledgement (#926)

The catalog groups firing alerts **by entity** and publishes the result
(RFC 06 §5.5). With `export_alerts` on, this exporter mirrors both halves.

`zensight_alert` gains an **`acked`** label:

```
zensight_alert{acked="true",source="host01",rule="socket-missing",…} 1
```

It reports whether an ack **applies**, not whether an ack document exists. The
projection rule is *"an ack applies only while a firing alert with
`timestamp <= fired_at` exists"*, so an orphan left by a dead catalog reads as
`false`, and an alert that cleared and came back reads `false` too — the
operator acknowledged a different occurrence. Without a catalog every alert
reads `acked="false"`, which is the honest answer: nobody has said they are on
it.

Each incident is one `<prefix>_incident` gauge:

```
# HELP zensight_incident ZenSight incident: firing alerts grouped by entity
#      (value = members neither acknowledged nor silenced).
# TYPE zensight_incident gauge
zensight_incident{incident="inc-h_guest",entity="h_guest",severity="critical",
                  origins="h-3fa9c2d41b7e,h-7c1e0a5b93d2",symptom_of="h_hyp"} 2
```

- **The value is the open member count** — neither acknowledged nor silenced,
  which is an operator's actual queue. A fully-handled incident reads `0`
  *without vanishing*, so a dashboard can still show that it exists; only a
  tombstone (no member firing) removes the series.
- **`origins` is plural**, comma-joined. That plurality is the point of keying
  by entity: a host that publishes under its own sensor, a hypervisor polling
  it and a prober checking it is *one* incident.
- **`symptom_of`** names the entity this incident is downstream of, when the
  catalog attributed one. An Alertmanager deployment gets
  inhibition-by-label from it directly:

  ```yaml
  inhibit_rules:
    - source_matchers: [ 'zensight_incident' ]
      target_matchers: [ 'symptom_of!=""' ]
  ```

Empty-string labels mean "the catalog did not say" — an incident with no entity
or no attributed cause — rather than a value.

### Both are seeded at startup

The exporter GETs `@catalog/state/incident/*` and `@catalog/state/ack/*` once
before entering its loop, alongside the alert seed it has done since #758, and
feeds the replies through the same handlers a live sample takes.

This is not an optimisation. `acked` is a **label on `zensight_alert`**, so an
exporter that restarts mid-incident and takes only live `Put`s renders every
acknowledged alert as `acked="false"` and Alertmanager re-pages for work
someone is already doing. "It will correct itself on the next update" is false
here: the catalog re-emits only on a **content change**, and an acknowledged
incident is typically the most stable thing on the bus — it may not re-emit for
hours.

The subscribers themselves stay plain (`declare_subscriber`, not the
history/recovery helper) because these are LWW documents rather than a
recoverable stream, which is the same call the alert and liveliness
subscribers make. CI's #763 guard holds that line by naming the exempt keys
one at a time, so a plain subscriber cannot slide in unnoticed — it caught this
one before it merged, with the seed missing.

The **OTel exporter deliberately does the opposite** on the same key: there an
incident is a log record per transition, not a gauge, so seeding would re-emit
"incident opened" for every incident an earlier incarnation already shipped —
duplicating history instead of recovering it. Same discipline as its traces
seed, which primes the tracker without re-emitting.

## Why `/metrics` is untimestamped and remote-write is not

The two paths deliberately disagree about timestamps, and the asymmetry is not
an oversight.

**Remote-write stamps each sample with the point's own timestamp** and skips a
series whose timestamp has not advanced since the last push (#759). Stamping
push time instead meant that for up to `stale_timeout_secs` after a sensor died,
every interval manufactured a fresh datapoint from the last known value — up to
ten synthetic samples per dead series at the 30 s default — so Grafana drew a
flat line where there should have been a gap. Using the point's timestamp
without the skip introduces the opposite bug: an unchanged series is re-pushed
with an identical `(series, timestamp)` every interval, which receivers reject
as a duplicate sample.

**The scrape endpoint emits no per-sample timestamps at all**, and must not.
Prometheus does not synthesise staleness markers for explicitly-timestamped
samples, and rejects samples outside its lookback window — so timestamping the
pull path would *remove* the very gap the push path had to work for. On a scrape
the series simply stops being exposed once `cleanup_stale` ages it out, which is
already the correct signal.
