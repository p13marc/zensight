# Plan 04 (enh) — netring GUI v2: flow explorer, L7 panels, JA4 inventory, NDR

**Crate:** `zensight` (frontend). **Depends on:** enh-03 (flow/L7 telemetry +
detector alerts + query/command channels). **Effort:** M–L. Pure frontend + the
query-channel client (shared with Plan 02).

Goal: turn `specialized/netring.rs` (flows + bandwidth) into a wire-analytics +
NDR console — flow explorer, DNS/HTTP/TLS panels, JA4 asset inventory, capture-
health, detector control — and enrich the cross-cutting Security view.

---

## 1. Tabbed sensor view (`specialized/netring.rs` + `NetringViewState`)

| Tab | Source | Notes |
|---|---|---|
| **Overview** | streamed aggregates | flow rate, bytes by proto, top-3 talkers, capture-health banner, active-detector summary |
| **Flows** | top-talkers stream + `@/query/flows` (§2) | streamed top-N; full/filtered flow table fetched on demand |
| **Top Talkers** | `bandwidth/*` + `talkers/top/*` | bars by app + src→dst by bytes |
| **DNS** | `dns/*` + `@/query/dns` | rtt p95, unanswered %, NXDOMAIN, top domains; recent lookups on demand |
| **HTTP** | `http/*` + `@/query/http` | status-class mix per host, methods, latency; recent requests on demand |
| **TLS / JA4** | `tls/*` + `@/query/tls` | versions, top SNI, ECH mix; **JA4 client asset inventory** table |
| **Capture Health** | `capture/*`, `monitor/*` | drop-rate gauge, active flows, handler/backend errors |
| **Detectors** | `@/status/detectors` | active detectors + counts + enable/disable/tune (§5) |

## 2. Flow explorer (the centerpiece, P2)
- **Streamed:** top-talkers (src→dst by bytes), flow rate, proto mix — always
  visible, low-cardinality.
- **On demand:** a filterable flow table fetched via
  `@/query/flows?ip=&port=&proto=` (reuse the shared `query_json` helper from Plan
  02 §3). Columns: 5-tuple, bytes↑/↓, packets, TCP-flag history, duration, app.
- **Drill-down:** click a flow → conversation rollup (`@/query/conversations?host=`).
- Sort by bytes/duration; filter chips. Nothing per-flow is streamed — this is the
  cardinality-safe explorer.

## 3. L7 panels
- **DNS:** rtt-p95 sparkline, unanswered-ratio gauge (a resolver-health signal),
  NXDOMAIN trend (DGA hint), top-domains table; "recent lookups" fetched on
  demand.
- **HTTP:** per-host status-class stacked bars (2xx/3xx/4xx/5xx), top hosts,
  method mix, latency p95; recent requests on demand.
- **TLS / JA4:** TLS-version split, ECH outcome mix, top SNIs, and a **JA4
  inventory** table (sni · ja4 · count · ech) fetched via `@/query/tls` — passive
  asset/client inventory, and the hook for "expected client absent" alerting.

## 4. Capture-health banner (honesty in the UI)
A persistent banner on the netring view: green when `drop_rate≈0`; **red** when
the sensor is dropping packets ("Sensor blind — N% packet loss; telemetry
incomplete"), plus active-flow and handler/backend-error counters. Surfaces the
enh-03 §A.6 self-health so users never trust lossy data silently.

## 5. Detector control panel
List active detectors (port_scan/beacon/dga/ech/exfil/lateral/dns_tunnel) with
live hit-counts (from `@/status/detectors`), each with enable/disable toggles and
key tunables (exfil min-bytes, lateral min-targets, dga entropy). Buttons send
`detectors set` commands (enh-03 §C) via the shared `send_command` helper, with a
host/fleet target. A runtime **BPF filter** input pushes `@/commands/filter`.

## 6. Security view enrichment (cross-cutting)
The existing Security view (v1) gains, from the richer detector suite:
- Anomalies grouped by **type** (scan/beacon/DGA/exfil/lateral/DNS-tunnel) with
  per-type counts, not just a flat list.
- **By-source rollup** ranked by anomaly count + total exfil bytes; each source
  expandable.
- **Pivot to evidence:** clicking an anomaly fetches the offending flows
  (`@/query/flows?ip=<src>`) so the analyst sees the packets-of-record behind the
  alert — the NDR "alert → evidence" loop.
- JA4 linkage: an anomaly's source cross-referenced to its JA4 client (asset
  identification).

## 7. Charts
Flow rate, per-app bandwidth, DNS rtt p95, HTTP 5xx rate, TLS handshake rate —
all from streamed aggregates via `view/chart.rs`.

## 8. Messages / state
- `NetringViewState`: selected tab, cached query detail per topic, detector
  status, filter input.
- `message.rs`: `Message::{SelectNetringTab, NetringDetailReceived{topic,json},
  RefreshNetringDetail, SetNetringDetectors{...}, NetringDetectorStatus(json),
  SetNetringFilter(String), PivotToFlows{src}}`.
- Route the netring specialized view through a side-state dispatcher (like syslog).

## 9. Testing (iced `Simulator`)
- Each tab renders from synthetic aggregates / fetched-detail JSON.
- Flow explorer: a fetched `@/query/flows` reply populates + sorts the table;
  filter chips alter the selector.
- Security pivot: clicking an anomaly emits `PivotToFlows{src}` and fetches flows.
- Detector toggle / filter input emit the right commands.
- Capture-health banner turns red above the drop-rate threshold.

## 10. Acceptance criteria
- Opening a netring sensor shows the tabs; the Flows explorer fetches a filtered
  flow table on demand (no per-flow on the bus).
- DNS/HTTP/TLS panels render; the JA4 inventory lists clients via `@/query/tls`.
- The capture-health banner reflects real drop-rate (red when lossy).
- Enabling/tuning a detector and swapping the BPF filter from the GUI take effect
  on the sensor with no restart.
- Security view groups anomalies by type + source and pivots an anomaly to its
  offending flows.
- Simulator tests green.
