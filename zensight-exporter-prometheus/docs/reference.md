# Prometheus exporter reference

The exporter subscribes to ZenSight telemetry over Zenoh, aggregates it into
Prometheus time series, and serves them on a pull `/metrics` endpoint (and,
optionally, pushes them via remote-write). Config lives in
[`../../configs/prometheus-exporter.json5`](../../configs/prometheus-exporter.json5);
this page is the mapping/behavior reference.

## Subscription

Two independent subscribers:

- **Telemetry** on `filters.key_expr` (default `zensight/**`). Narrow it — e.g.
  `zensight/netring/**` — to tame the firehose at the *subscription*, so unwanted
  protocols and the `_meta/**` control plane never reach the exporter over the
  wire. The `zensight/**` wildcard does **not** match `@/`-prefixed control keys.
- **Alerts** on `zensight/*/@/alerts/*` (only when `export_alerts` is on). Because
  the telemetry wildcard skips `@/` keys, alert export needs its own subscriber.

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
  leading digit prefixed with `_`, empty → `unnamed`. So `system/sysUpTime` →
  `system_sysUpTime`, `disk[sda]` → `disk_sda`.
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

`render()` groups series by name and emits `# HELP` / `# TYPE` comments per group
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

With `export_alerts` on (default), each **firing** alert from `@/alerts/*` becomes
one `<prefix>_alert` gauge series with value 1:

```
# HELP zensight_alert ZenSight sensor alert (1 = firing; series absent once resolved).
# TYPE zensight_alert gauge
zensight_alert{source="host01",rule="socket-missing",severity="critical",…} 1
```

Labels carry the alert's `source`, `rule`, `severity`, and its own labels (reserved
names are not overridden). The series disappears when the alert resolves or its
sensor tombstones it, so Alertmanager treats absence as resolved. Alerts are also
staleness-swept like metrics.
