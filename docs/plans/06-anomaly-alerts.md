# Plan 06 — Anomaly alerts (Pillar A)

**Goal:** turn netring's prebuilt anomaly detectors (port scan, C2 beacon, DGA,
ECH/TLS downgrade) and capture self-health into `Alert`s — giving ZenSight its
first **network-threat** alert category. Small plan: the detectors already exist;
this wires `detector emit → common::Alert → AlertReporter → @/alerts`.

**Depends on:** 02 (`AlertReporter`), 05 (the netring sensor / `monitor.rs` + drain tasks).
**Effort:** S–M.

---

## 1. Config (`NetringConfig.anomalies`)

```json5
anomalies: {
  detectors: ["port_scan", "beacon", "dga", "ech_downgrade"],
  // per-detector tuning passed through to netring detector builders:
  port_scan:  { min_ports: 20, severity: "warning" },
  beacon:     { severity: "warning" },
  dga:        { entropy_threshold: 3.5, severity: "warning" },
  default_for_secs: 0,        // anomalies fire immediately by default
}
```

## 2. Attach detectors in `monitor.rs` (Plan 05 §5 hook) — real netring API

Detectors are enrolled with `.detect(detector_macro)` (sugar over `.on_ctx`); the
anomalies they `ctx.emit(...)` flow to whatever sink is attached. Use the built-in
**`ChannelSink`** (a `tokio::mpsc` of `OwnedAnomaly`) — **no custom sink trait**:

```rust
use netring::anomaly::{ChannelSink};
use netring::monitor::layer::MinSeverity;

let (anom_tx, anom_rx) = tokio::sync::mpsc::channel::<OwnedAnomaly>(4096);

for d in &cfg.anomalies.detectors {
    match d.as_str() {
        "port_scan" => b = b.detect(pattern_detector!(PortScanDetector::new(cfg.port_scan.clone()))),
        "beacon"    => b = b.detect(pattern_detector!(BeaconDetector::new(cfg.beacon.clone()))),
        "dga"       => b = b.detect(pattern_detector!(DgaScorer::new(cfg.dga.clone()))),
        "ech_downgrade" => b = b.on_ctx(ech_downgrade_handler()),   // emits via ctx.emit
        other => tracing::warn!("unknown detector {other}"),
    }
}
b = b.sink(ChannelSink::new(anom_tx)).layer(MinSeverity::from(cfg.min_severity));

// drain task: OwnedAnomaly → common::Alert → AlertReporter
runner.spawn(publish::drain_anomalies(anom_rx, reporter, sensor_id));
```

(Method/type names verified against netring `docs/API_OVERVIEW.md` "Anomaly sinks"
/ "Builder surface"; re-confirm against the pinned version.)

## 3. Anomaly → `Alert` mapping (`sink.rs`)

`publish::drain_anomalies` reads each `OwnedAnomaly` from the `ChannelSink` mpsc
(carrying `kind`, severity, and a payload of flow/IP/domain detail) and maps it:

```rust
let alert = Alert::new(sensor_id, Protocol::Netring, AlertKind::Anomaly,
        anomaly.kind(),                         // "PortScanDetector" etc.
        map_severity(anomaly.severity()),
        human_summary(&anomaly))                // "Port scan from 10.0.0.5 (37 ports)"
    .with_label("src", anomaly.src_ip())
    .with_label("detail", anomaly.detail());    // domain/ja4/ports — HIGH-cardinality in LABELS, not key
self.reporter.observe(alert, for_duration).await.ok();
```

| Detector | `rule` | summary | default severity |
|---|---|---|---|
| `PortScanDetector` | `port_scan` | "Port scan from `<ip>` (`<n>` ports)" | warning→critical by score |
| `BeaconDetector` | `beacon` | "Periodic beacon `<src>`→`<dst>` (C2-like)" | warning |
| `DgaScorer` | `dga` | "DGA-like DNS query `<domain>`" | warning |
| ECH/TLS downgrade | `tls_downgrade` | "TLS downgrade / ECH anomaly `<sni>`" | info |

**Cardinality discipline (from netring's own `METRICS.md`):** the offending IP /
domain / JA4 goes in `Alert.labels` and the summary — **never** in the
`alert_key` in a way that explodes a downstream Prometheus exporter. The
`alert_key` should bucket by `(rule, src)` not `(rule, full 5-tuple)` so a scan
across 1000 ports is *one* alert, not 1000.

## 4. Capture self-health alert

If `capture_drop_rate_alert > 0` (Plan 05 §4.6): when windowed `drop_rate`
exceeds it, `reporter.observe(Alert::new(.., AlertKind::SensorHealth, "capture_drop",
Critical, "Sensor dropping N% of packets"))`; resolve when it recovers. This makes
"my IDS is blind" itself an alert.

## 5. Lifecycle / debounce
- Anomalies are mostly **edge events** (a scan happened). They still go through
  `AlertReporter` so the UI can show + auto-expire them: fire immediately
  (`for: 0`), then auto-resolve after a quiet period (`reconcile` when the
  detector stops emitting for that `(rule, src)` for N seconds). Implement a
  TTL-resolve in the sink: track last-seen per `alert_key`, resolve on TTL expiry.
- Beacon/DGA may re-emit; dedup on `alert_key` so they update in place.

## 6. Tests
- `human_summary` + `map_severity` + `alert_key` bucketing: pure functions on
  synthetic `Anomaly` fixtures.
- Sink: feed a sequence of synthetic anomalies → assert the reporter publishes
  one firing alert per `(rule, src)` and resolves on TTL.
- No live capture needed (detectors tested by netring itself; we test the
  translation only).

## 7. Acceptance criteria
- Running a port scan against the monitored interface (e.g. `nmap` in a netns)
  produces a single `Anomaly` alert in the frontend Alerts view + toast, with the
  scanner IP in labels; it auto-resolves after the scan stops.
- DGA/beacon detectors likewise surface alerts under `zensight/netring/<id>/@/alerts/*`.
- Inducing capture drops raises a `SensorHealth` alert.
- Translation unit tests green; example config + README anomaly section added.
