# Plan 03 (enh) — netring sensor v2: flows, L7, detectors, dynamic config

**Crate:** `zensight-sensor-netring`. **Depends on:** v1 sensor + `AlertReporter`
(shipped), and **[Plan 05](05-keyspace-redesign.md)** (land first — netring gets
its control plane "for free" from the `ControlPlane`, so §C/§D below stop being
hand-rolled). **Effort:** L (split A–E). **Verification:** pcap replay (no
privileges), as in v1.

> **Keys (Plan 05):** telemetry → `zensight/telemetry/netring/<sensor>/<metric>`;
> alerts → `zensight/sensor/<host>/netring/alerts/<key>`; commands/queries →
> `zensight/sensor/<host>/netring/{cmd,query,status}/<topic>` (+ `fleet/netring/
> cmd/<topic>` for broadcast).

Goal: go from "flow counts + per-app bandwidth + one port-scan detector" to a
real flow + L7 + NDR sensor — IPFIX-grade flow analytics, DNS/HTTP/TLS visibility,
a detector suite (beacon, DGA, exfil, lateral movement, ECH), capture-health
self-monitoring — all runtime-configurable, with flow/L7 detail served on demand.

`netring` API used below is verified against the pinned checkout's
`examples/monitor/` (net_diagnostic: `on_icmp_error`/`on_tcp_reset`/`on_bandwidth`;
`session::<Dns|Http|Tls>` subscriptions; `on_fingerprint`; `export_flows`;
`detect(pattern_detector!(BeaconDetector|DgaScorer))`; `ech_adoption`; capture
metrics) and netring `docs/METRICS.md`.

---

## A. More metrics (aggregate stream + on-demand detail)

**Cardinality rule (P2) is paramount here** — per-flow is the textbook
high-cardinality firehose. Stream **aggregates**; serve flow/L7 detail via the
query channel (§D) and (optionally) export the firehose to IPFIX/Zeek (§E).
Metrics follow **RED** (P3).

### A.1 Flow analytics (streamed aggregates)
From flow lifecycle + `on_bandwidth` + a `tick` rollup:
| metric | type | RED |
|---|---|---|
| `flow/rate` (flows/sec started) | Gauge | rate |
| `flow/active`, `flow/started_total`, `flow/ended_total` (v1) | Gauge/Counter | rate |
| `flow/duration_p50_ms`, `duration_p95_ms` | Gauge | duration |
| `flow/by_proto/<tcp\|udp\|icmp>/bytes` | Counter | utilization |
| `bandwidth/<app>/bytes_per_sec` (v1, via `LabelTable`) | Gauge | utilization |
| `talkers/top/<rank>` (top-N src→dst by bytes, label-carried) | Gauge | utilization |
Full/recent flow records (5-tuple, bytes/pkts per dir, TCP-flag history,
duration) → `@/query/flows` (§D). The NetFlow/IPFIX field set
([RFC 3954](https://datatracker.ietf.org/doc/html/rfc3954)) is the model.

### A.2 L7 — DNS (`session::<Dns>`)
| metric | type |
|---|---|
| `dns/queries_total`, `responses_total` | Counter |
| `dns/<server>/rtt_p95_ms` | Gauge |
| `dns/unanswered_ratio` (windowed) | Gauge |
| `dns/nxdomain_total` | Counter |
| `dns/top_domains/<rank>` | Gauge (label = domain) |
Recent lookups → `@/query/dns`. (`DnsUdpParser::with_correlation` gives RTT +
unanswered.)

### A.3 L7 — HTTP (`session::<Http>`)
`http/requests_total`, `http/<host>/status_<2xx\|3xx\|4xx\|5xx>` (Counter),
`http/<host>/latency_p95_ms` (Gauge), `http/methods/<method>` (Counter); top
hosts streamed, full request log on demand → `@/query/http`.

### A.4 L7 — TLS / fingerprints (`on_fingerprint`)
`tls/handshakes_total`, `tls/version/<1.2\|1.3>` (Counter),
`tls/top_sni/<rank>` (Gauge), `tls/ech_<grease\|real\|none>` (Counter). **JA3/JA4
client inventory** (passive asset inventory) → `@/query/tls` (sni, ja4, count).
ECH outcome feeds the downgrade detector (§B).

### A.5 ICMP errors + TCP resets (`on_icmp_error` / `on_tcp_reset`)
`icmp/unreachable_total`, `icmp/time_exceeded_total`, `icmp/by_kind/<…>`
(Counter, flow-joined); `tcp/resets_total`, `tcp/refused_total` (zero-payload RST
= connection refused vs mid-transfer abort). These are high-signal health/security
indicators.

### A.6 Capture self-health (netring `METRICS.md`) — honesty
`capture/<iface>/packets`, `drops` (Counter), `capture/<iface>/drop_rate`
(windowed Gauge), `monitor/active_flows`, `monitor/handler_errors`,
`monitor/backend_errors` (Gauge). Feed `drop_rate` + error counters to the
sensor's `SensorHealth` so the dashboard knows when the sensor is **lossy** (and
therefore its other telemetry is incomplete). Threshold → a `SensorHealth` alert.

---

## B. More alerts — the NDR detector suite

Each detector → `ChannelSink` → drain → `common::Alert` (v1 pattern). Enable/tune
per detector via §C. **All anomaly alerts get auto-resolve (P5):** a TTL sweep in
the drain resolves a `(rule, src)` alert after a quiet period (fixes the v1 "never
clears" gap).

| detector | technique | rule | source |
|---|---|---|---|
| Port scan (v1) | TRW | `PortScanTRW` | shipped |
| **Beacon / C2** | period-variance on repeated dst | `BeaconCv` | `BeaconDetector` (beacon_detector example) |
| **DGA** | DNS bigram-entropy + use-and-discard | `DgaScorer` | `DgaScorer` (dga_query example) |
| **ECH downgrade** | ECH offered→absent | `EchDowngrade` | `EchOutcome` (ech_adoption example) |
| **Data exfiltration** | large outbound bytes to external (RFC1918→public, volume anomaly) | `ExfilVolume` | custom flow detector (flow bytes + direction + dst not-private) |
| **Lateral movement** | one internal src → many internal dst/ports (internal scan) | `LateralScan` | custom flow detector keyed on src, internal-only |
| **DNS exfil/tunnel** | high-entropy/long qnames, high query volume to one domain | `DnsTunnel` | custom on `session::<Dns>` |
| **Capture-blind** | windowed `drop_rate` > threshold | `CaptureDrop` (SensorHealth) | §A.6 |

The custom detectors (exfil/lateral/DNS-tunnel) follow netring's
`pattern_detector!` shape (a stateful detector with `feed`/`verdict`), grounded in
standard NTA techniques (volume anomalies, internal-scan, use-and-discard DNS —
see Sources in the INDEX). RFC1918/private-range classification decides
internal-vs-external. Keep high-cardinality detail (IP, domain, JA4) in alert
**labels**, bucket `alert_key` by `(rule, src)` (v1 discipline).

---

## C. Dynamic configuration (runtime)

Add a command channel `zensight/netring/@/commands/<topic>` (netring had none in
v1; the netlink sensor's command module is the template):

- **`detectors`** — `{ "type":"set", "detectors":{ "beacon":true, "dga":true,
  "exfil":{"min_bytes":10485760}, ... } }`. Enable/disable/tune each detector.
- **`collection`** — toggle flows / dns / http / tls / bandwidth / icmp / rst /
  capture-metrics at runtime.
- **`filter`** — swap the kernel BPF filter at runtime (netring supports runtime
  filter swap / `.expr("…")`), e.g. focus capture on a subnet/port without
  restart. Status queryable returns the active filter.

Detectors/collection toggles live behind `Arc<RwLock<…>>`; the monitor's handlers
check the flag (a disabled detector's drain just no-ops). **Caveat:** adding/
removing a *capture interface* at runtime may require rebuilding the `Monitor`
(netring builds the source set at `.build()`); treat interface changes as a
supervised restart, not a hot-swap — document this.

## D. On-demand flow/L7 detail (query channel, P2)

| queryable | reply |
|---|---|
| `@/query/flows?ip=&port=&proto=` | recent/top flow records (5-tuple, bytes/pkts/dir, flags, duration) |
| `@/query/conversations?host=` | per-host conversation rollup |
| `@/query/dns?domain=` | recent DNS lookups (qname, rtt, answered) |
| `@/query/tls` | JA4 client inventory (sni, ja4, count, ech) |

The sensor keeps a bounded ring of recent flows/L7 messages (size-capped) to
answer queries; nothing per-flow is streamed.

## E. Optional firehose export (downstream SIEM)
For teams that want full records elsewhere: enable netring's `export_flows`
(IPFIX/NetFlow v10) and/or `EveSink` (Suricata EVE JSON) / Zeek `conn.log` to a
file or socket, independent of the Zenoh aggregate stream. Off by default;
configured under `netring.export`.

## F. Config schema (additions)
```json5
netring: {
  collect: { flows:true, bandwidth:true, dns:true, http:true, tls:true,
             icmp:true, rst:true, capture_metrics:true },
  anomalies: { port_scan:true, beacon:true, dga:true, ech_downgrade:true,
               exfil:{min_bytes:10485760}, lateral:{min_targets:15},
               dns_tunnel:{entropy:3.8} },
  capture_drop_rate_alert: 0.01,
  detail_ring: { flows: 5000, dns: 2000 },   // on-demand query buffers
  export: { ipfix: null, eve: null },        // optional firehose
  // bpf: "tcp or udp",                       // runtime-swappable via @/commands/filter
}
```

## G. Testing
- Pure `map.rs` for every aggregate + the new `AnomalyView` decompositions (DNS/
  HTTP/TLS message → telemetry; flow record → aggregate) — unit-tested on
  synthetic structs.
- Custom detectors (exfil/lateral/dns-tunnel): pure `feed`/`verdict` logic tested
  on synthetic flow/DNS sequences (scanner pattern, big-outbound, NXDOMAIN burst).
- Auto-resolve TTL: synthetic anomaly stream → fires then resolves after quiet.
- **pcap replay** end-to-end (privilege-free): a crafted pcap with a DNS lookup,
  an HTTP request, a TLS handshake, and a scan pattern → assert the corresponding
  telemetry + alerts appear on a subscriber (extend the v1 `/tmp/genpcap.py`).
- Command channel: `detectors set` enables a detector live (in-proc peer test).

## H. Acceptance criteria
- Flows/L7/ICMP/RST/capture-health telemetry published; per-flow/L7 detail
  fetched only via §D (no per-flow on the bus).
- Beacon/DGA/exfil/lateral/ECH detectors each produce a `common::Alert` (verified
  via pcap replay where feasible; custom detectors via unit tests) and
  **auto-resolve** after quiet.
- Capture drop-rate over threshold degrades sensor health + fires a `SensorHealth`
  alert.
- Detectors/collection/filter all changeable at runtime via §C with no restart
  (filter swap verified live; interface change documented as restart).
- pcap-replay integration test green; example config + README updated.

## I. Sequencing
A.1 (flows) + D (query) + A.6 (capture health) first; then A.2–A.5 (L7 + ICMP/
RST); then B (detector suite, starting with the shipped beacon/dga/ech, then the
custom exfil/lateral/dns-tunnel); then C (dynamic config); E optional last.

## J. Risks
- **Hot path:** all handlers stay cheap (push to mpsc, drain off-path) — netring's
  zero-alloc run loop must not block (v1 discipline).
- **Cardinality:** the detail ring + query channel are mandatory; never stream
  per-flow. Review-gate it.
- **Capture privileges:** live needs `CAP_NET_RAW`; CI verifies via pcap replay.
  Custom detectors are unit-tested independent of capture.
- **`ja4plus`** (JA4S server fingerprints) is FoxIO-licensed (non-MIT) — keep the
  default JA3/JA4-client surface (royalty-free); gate `ja4plus` behind an opt-in
  feature, off by default.
