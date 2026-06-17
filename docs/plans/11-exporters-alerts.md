# Plan 11 — Exporters surface alerts

**Goal:** the Prometheus and OTEL exporters subscribe to `zensight/**` and today
expect `TelemetryPoint`. After Plan 02 they also see `@/alerts/<key>` payloads.
This plan makes them (a) **not choke** on the new channel and (b) optionally
**export alerts** to the external observability stack.

**Depends on:** 02 (`Alert` type + `@/alerts`). **Effort:** S.

---

## 1. Don't choke (mandatory)

Both exporters' subscribers must skip keys they don't model. Verify the current
decode path filters to telemetry; if it blindly `decode::<TelemetryPoint>`s every
sample, add an early `@/`-segment guard so health/liveness/alerts keys are
ignored cleanly (no error spam). This is a correctness fix regardless of §2.

## 2. Prometheus: export firing alerts (optional but recommended)

`zensight-exporter-prometheus`:
- Decode `@/alerts/<key>` Put(Firing) / Resolved+Delete into an in-memory active
  set (same lifecycle as the frontend, reusing `common::Alert`).
- Expose:
  ```
  zensight_alert{source, protocol, kind, rule, severity} 1
  ```
  One series per active alert; **drop the series on Resolved/Delete** (the
  collector already has staleness expiry — reuse it).
- **Cardinality discipline (from netring `METRICS.md`):** keep `severity`/`rule`/
  `kind` as labels (bounded), but **do not** put per-IP/per-domain detail into
  label *values* that explode the TSDB. Bucket: the `alert_key` already buckets by
  `(rule, src)` (Plan 06), so series count ≈ active alerts, not per-packet.
  Offending-IP detail stays out of Prometheus (it's in the alert body for the GUI
  / EVE / logs).

## 3. OTEL: alerts as logs/events (optional)

`zensight-exporter-otel`:
- Map each `Alert` to an OTEL **log record** (it already maps syslog → OTEL logs,
  so reuse the severity mapping): `severity_number` from `AlertSeverity`, body =
  `summary`, attributes = `source`/`protocol`/`kind`/`rule` + labels, plus a
  `alert.state` attribute (firing/resolved) so downstream can correlate.
- Optionally also a metric `zensight.alerts.active` gauge by `(rule, severity)`.

## 4. Config
Add `export_alerts: bool` (default `true` for §1 skip-safety is implicit;
`false` by default for the §2/§3 *emission* so operators opt in). Document in each
exporter's config + README.

## 5. Tests
- Decode: an `@/alerts` sample does not produce a `TelemetryPoint` mapping error.
- Prometheus: firing alert → one `zensight_alert` series; Resolved → series gone.
- OTEL: alert → one log record with correct severity + `alert.state`.

## 6. Acceptance criteria
- Neither exporter logs decode errors when alerts flow on the bus.
- With `export_alerts: true`, a firing alert appears as a Prometheus series /
  OTEL log and disappears on resolve.
- No unbounded-cardinality series (verify with a port-scan generating one alert,
  not one-per-port).
