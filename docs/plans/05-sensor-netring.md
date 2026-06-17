# Plan 05 — `zensight-sensor-netring` (built on `netring`)

**Goal:** a new sensor that streams **wire ground truth** from zero-copy capture
(AF_PACKET / AF_XDP) — real flow records (no NetFlow exporter device), passive
DNS/TLS/HTTP L7 metadata, per-app bandwidth — as `TelemetryPoint`s under
`zensight/netring/<sensor>/...`, plus self-health (capture drop rate). It is the
substrate for anomaly alerts (Plan 06).

**Depends on:** 01 (sensor-core), 02 (`AlertReporter`, used by Plan 06).
**Effort:** M–L. **Platform:** Linux (AF_PACKET kernel 3.2+, AF_XDP 5.4+).
**Privilege:** `CAP_NET_RAW` (+`CAP_IPC_LOCK` for AF_XDP).

---

## 1. Integration strategy — `ChannelSink` + typed subscriptions ([INDEX D10](00-INDEX.md))

netring already ships IPFIX/syslog/OTLP/EVE/Prometheus sinks; we don't chain
through those. **Correction to the first draft:** netring's real API does *not*
need a hand-rolled sink trait. Use the built-ins:

- **Anomalies →** `.sink(ChannelSink)` gives a `tokio::mpsc` of `OwnedAnomaly`.
  A drain task maps each to `common::Alert` and calls `AlertReporter` (Plan 06).
- **Telemetry →** typed handlers, no trait impl:
  - `.subscribe(flow::<Tcp>().to(closure))` / `.subscribe(session::<Tls>()
    .sni_glob("*").to(closure))` / `.subscribe(session::<Dns>()...)` etc.
  - `.export_flows(exporter)` + `.export_active_timeout(period)` for `FlowRecord`s.
  - `.tick(period, handler)` for periodic aggregates (bandwidth, capture health).
  - Each closure pushes a small owned struct into an `mpsc` drained by a
    **publisher task** — never publish inline (netring's run loop is zero-alloc;
    keep handlers cheap and non-blocking).

So the only ZenSight-specific code is (a) the closures that extract fields, (b)
the mpsc drain → `Publisher`, and (c) the anomaly drain → `AlertReporter`. The
netring `Monitor` runs in-process via `monitor.run_until_signal()` /
`run_for(..)` (the run future is `Send + 'static` since 0.23).

## 2. Crate scaffolding

```
zensight-sensor-netring/
├── Cargo.toml      # zensight-common, zensight-sensor-core, netring (features:
│                   #   tokio, flow, parse, http, dns, metrics, eve-sink as needed),
│                   #   tokio, serde, clap, tracing
├── src/
│   ├── main.rs     # SensorArgs → config → SensorRunner → build Monitor → run
│   ├── lib.rs
│   ├── config.rs   # NetringSensorConfig (impl SensorConfig)
│   ├── monitor.rs  # builds netring Monitor (subscriptions + ChannelSink) from config
│   ├── publish.rs  # mpsc drain tasks: TelemetryPoint → Publisher; OwnedAnomaly → AlertReporter
│   └── map.rs      # FlowRecord / L7 message / BandwidthReport / OwnedAnomaly → TelemetryPoint/Alert
```

`[[bin]] name = "zensight-sensor-netring"`. Linux-cfg gate like Plan 03 §7.

## 3. Config (`config.rs`)

```rust
pub struct NetringSensorConfig {
    pub zenoh: ZenohConfig,
    #[serde(default)] pub logging: LoggingConfig,
    pub netring: NetringConfig,
}
pub struct NetringConfig {
    #[serde(default="kp")] pub key_prefix: String,      // "zensight/netring"
    #[serde(default="auto")] pub sensor_id: String,     // host or sensor name = `source`
    pub interfaces: Vec<String>,                         // ["eth0"] (lo for demo)
    #[serde(default)] pub backend: Backend,             // AfPacket (default) | AfXdp
    #[serde(default)] pub bpf: Option<String>,          // optional kernel filter expr
    #[serde(default)] pub collect: CollectConfig,       // flows/dns/http/tls/bandwidth
    #[serde(default)] pub export: ExportConfig,         // flow export period, active-timeout
    #[serde(default)] pub capture_drop_rate_alert: f64, // 0.0 = off
    // Pillar A anomaly detector list (Plan 06 adds `anomalies`).
}
```

## 4. Telemetry mapping (`map.rs`)

`source = sensor_id`, `protocol = Protocol::Netring`. Mirror the existing
`netflow` key shape so the frontend treats flows uniformly.

### 4.1 Flows (netring `export_flows` / flow-tier handler → `FlowRecord`)
On flow end (and interim for long flows via `export_active_timeout`):
| metric | value | labels |
|---|---|---|
| `flow/<src>-<dst>/bytes` | Counter | `proto`,`sport`,`dport`,`dir` |
| `flow/<src>-<dst>/packets` | Counter | |
| `flow/<src>-<dst>/duration_ms` | Gauge | |
| `flow/<src>-<dst>/history` | Text (TCP flag history) | |

### 4.2 Per-app bandwidth (`on_bandwidth` + `BandwidthReport` + `LabelTable`)
`bandwidth/<app>/rx_bps` / `tx_bps` Gauge, label `app`.

### 4.3 DNS (`DnsUdpParser::with_correlation`)
`dns/<server>/rtt_ms` Gauge, `dns/<server>/unanswered` Counter,
`dns/<server>/queries` Counter.

### 4.4 HTTP (`HttpParser`)
`http/<host>/requests` Counter, `http/<host>/status_<class>` Counter
(2xx/3xx/4xx/5xx), `http/<host>/latency_ms` Gauge.

### 4.5 TLS / fingerprints (`on_fingerprint`)
`tls/<sni>/ja4` Text, `tls/<sni>/sessions` Counter; ECH outcome as label. (Passive
asset/client inventory.)

### 4.6 Capture self-health (netring `CaptureTelemetry` / `MonitorHealth`)
`capture/<iface>/packets`, `capture/<iface>/drops` Counter,
`capture/<iface>/drop_rate` Gauge, `monitor/active_flows` Gauge,
`monitor/handler_errors` / `backend_errors` Gauge. Feed `drop_rate` and the
error counters to the sensor's `SensorHealth` so the dashboard health bar reflects
capture loss — a sensor dropping packets is reporting incomplete data and must
say so. (If `capture_drop_rate_alert > 0`, also emit a `SensorHealth` alert via
Plan 06's reporter.)

## 5. `monitor.rs` — build the Monitor (real netring API)

```rust
use netring::prelude::*;
use netring::monitor::subscription::{flow, session, packet};

let (tel_tx, tel_rx) = tokio::sync::mpsc::channel::<TelemetryPoint>(8192);

let mut b = Monitor::builder();
b = b.interfaces(cfg.interfaces.clone()).name(cfg.sensor_id.clone());
if cfg.collect.dns  { b = b.protocol::<Dns>()
    .subscribe(session::<Dns>().to({ let tx=tel_tx.clone(); move |m,_| { tx.try_send(map::dns(m)).ok(); Ok(()) }})); }
if cfg.collect.http { b = b.protocol::<Http>()
    .subscribe(session::<Http>().to({ let tx=tel_tx.clone(); move |m,_| { tx.try_send(map::http(m)).ok(); Ok(()) }})); }
if cfg.collect.tls  { b = b.protocol::<Tls>()
    .subscribe(session::<Tls>().to({ let tx=tel_tx.clone(); move |m,_| { tx.try_send(map::tls(m)).ok(); Ok(()) }})); }
if cfg.export.flows { b = b.export_flows(ZensightFlowExporter::new(tel_tx.clone()))
                          .export_active_timeout(cfg.export.active_timeout()); }
if cfg.collect.bandwidth { b = b.tick(cfg.bw_period(), bandwidth_handler(tel_tx.clone())); }
b = b.tick(cfg.capture_period(), capture_health_handler(tel_tx.clone()));  // self-health
// Plan 06 adds: b = b.detect(...).sink(ChannelSink::new(anom_tx)).layer(MinSeverity::..);
let monitor = b.build()?;

runner.spawn(publish::drain_telemetry(tel_rx, publisher));   // mpsc → Publisher::publish_batch
runner.spawn(async move { monitor.run_until_signal().await.ok(); });   // Send + 'static (0.23)
```

`publish::drain_telemetry` batches from the mpsc and calls
`Publisher::publish_batch`; on channel-full the handlers `try_send` and **drop +
count** (emit `monitor/publish_dropped`) rather than block capture. The
`.subscribe(...)` filters (`sni_glob`, `dst_port`, `.expr("udp and dst port 53")`)
push down into the kernel BPF where possible — set them from `cfg.bpf`/collect
flags. Method names verified against netring `docs/API_OVERVIEW.md` (0.25 builder
surface) — re-confirm against the pinned netring version at implementation time.

## 6. main.rs flow

```rust
let args = SensorArgs::parse_with_default("netring.json5");
let config = NetringSensorConfig::load(&args.config)?;
let runner = SensorRunner::new_with_args("netring", config, Some(&args)).await?
    .with_status_publishing().with_format(Format::Json);
let publisher = runner.publisher();
let monitor = monitor::build(&cfg.netring, publisher /*, reporter */)?;
runner.spawn(async move { if let Err(e) = monitor.run().await { tracing::error!(?e); } });
runner.run_with_metadata(Some(metadata)).await
```

## 7. Backpressure & safety
- Capture → `mpsc(bounded)` → publisher task. On channel-full, **drop + count**
  (emit a `monitor/publish_dropped` counter) rather than block capture.
- Cap label/series cardinality: per-flow keys are high-cardinality by nature;
  keep them in the **flow** subtree (the frontend/exporters can sample/expire).
  Aggregates (bandwidth/dns/http per host) are the low-cardinality summary.
- Document `setcap cap_net_raw,cap_ipc_lock+ep` for the binary.

## 8. Tests
- `map.rs` pure functions on synthetic `FlowRecord`/`BandwidthReport`/DNS-message
  fixtures → assert `TelemetryPoint`s. No capture needed.
- `publish::drain_telemetry` / `drain_anomalies` translation via a fake publisher
  + fake `AlertReporter` capturing puts.
- Gate any live-capture test behind a `caps`/`live` feature (`lo` + synthetic
  traffic, like netring's `synthetic_traffic` example) so default CI needs no
  privileges.

## 9. Acceptance criteria
- `zensight-sensor-netring --config configs/netring.json5` on `lo` with synthetic
  traffic publishes flow + bandwidth telemetry visible in the frontend as a
  `netring` device.
- `Protocol::Netring` added to `zensight_common` (+ frontend icon/rendering).
- Inducing kernel drops raises `capture/<iface>/drop_rate` and degrades the
  sensor's health status in the dashboard.
- `map.rs` + sink unit tests green; example config + README added.
