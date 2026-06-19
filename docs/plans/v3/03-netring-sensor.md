# Plan v3‑03 — netring sensor

Today: flow lifecycle + volume (RED) aggregates, flow‑duration percentiles,
per‑app bandwidth, TCP resets/refused, flow‑detail ring (`@/query/flows`),
TRW port‑scan anomaly → alerts, TLS fingerprint inventory (`on_fingerprint`,
JA4) + `@/query/tls`, capture self‑health (`on_capture_stats`).

> APIs verified against the pinned netring checkout (`monitor/mod.rs` builder
> hooks, `examples/{l7,anomaly,monitor}/`) + flowscope 0.16
> (`detect/patterns/`, `dns/`, `http/`). Research: RITA beaconing, JA4, DNS RED,
> Suricata scan thresholds. All netring features need CAP_NET_RAW (live capture);
> pcap replay verifies most.

---

## A. ICMP error telemetry **[Wave 1] — (re‑add, live‑gated)**

`.protocol::<Icmp>()` + `.on_icmp_error(handler)` → `IcmpError { kind,
correlated_flow }`. Stream `icmp/{unreachable_total,time_exceeded_total,
by_kind/*}` (Counter); flag a flow killed by ICMP as an anomaly.
**Honest note:** this was backed out in v2 because a *synthetic pcap* couldn't
trigger `on_icmp_error` (kernel‑correlation requirements). The hook is correct
per the API + `examples/monitor/net_diagnostic.rs`; it fires on **live capture**
with real ICMP (`nc -uvz` port‑unreachable). Ship live‑gated: unit‑test the map,
verify the handler integration with a crafted‑but‑complete UDP+ICMP pcap or live.
**Why:** PMTU black holes, firewall rejections, dead routes — high‑signal path
failures.

## B. Per‑protocol + connection‑state breakdown **[Wave 1, trivial]**

Register `FlowEnded<Udp>`/`FlowEnded<Icmp>` alongside the existing TCP handler:
- `flow/by_l4/{tcp,udp,icmp}/{bytes,flows}_total` (Counter) — network composition;
  an unusual UDP spike = DNS/NTP amplification abuse.
- `tcp/closed_{fin,rst,idle}_total` — bucket the `FlowEnded.reason` (`EndReason`)
  we already receive. High RST% = firewall/IDS drops / instability.

*Pcap‑verifiable.* Three atomics + bucketing — lowest‑effort, real value.

## C. RITA‑style beaconing detector (C2) **[Wave 1] — highest‑SNR NDR gap**

`flowscope::detect::patterns::BeaconDetector<K>` **(verified present in 0.16,
alongside `DgaScorer`/`DgaScore`/`is_dga`)**. Wrap it in the existing
`pattern_detector!` path on `FlowEnded<Tcp>`:
gate at >20 connections per (src,dst), score time‑delta regularity (Bowley
skewness + MADM) and data‑size consistency, flag **≥0.80** → anomaly alert
`Beacon` with score/interval observations. Pair with a **known‑good allowlist**
(NTP/update/telemetry agents) to kill the dominant false positives.
**Why:** beaconing is the highest signal‑to‑noise, evasion‑resistant flow‑only
detection (jitter still leaves symmetric distributions). *Pcap‑verifiable* with a
crafted periodic‑flow pcap.

## D. More NDR detectors **[Wave 2/3]**

All via `pattern_detector!` + `TimeBucketedCounter`:
- **Connection flood** (per‑dst / per‑port): `FlowStarted<Tcp>` rate > threshold —
  distinct from port‑scan (many ports) vs flood (many conns to one port).
- **SYN‑rate metric** (not a detection): per‑dst SYN rate + established‑vs‑SYN
  ratio as telemetry — research says treat volumetric as a rate, not an alert.
- **DGA / DNS‑tunneling** (`flowscope::detect::patterns::DgaScorer`,
  bigram log‑likelihood): **blocked on DNS integration (E)** — apply to each
  query SLD once DNS lands.

## E. L7 DNS — RED analytics **[Wave 2] — feasibility‑flagged**

DNS is the highest‑signal *fully passive* L7 (unencrypted on the wire).
flowscope has `DnsUdpParser::with_correlation()` (query‑RTT + `Unanswered` via
`on_tick`) and `DnsMessage::{Query,Response,Unanswered}` with `rcode`.

| metric (RED) | type |
|---|---|
| `dns/queries_total` | Counter |
| `dns/responses_by_rcode/{noerror,nxdomain,servfail,refused}_total` | Counter |
| `dns/query_rtt_p{50,95,99}_ms` (from correlation) | Gauge |
| `dns/unanswered_total` (resolver loss, distinct from rcode) | Counter |
| top SLDs / top‑NXDOMAIN → `@/query/dns` | — |

**Feasibility (honest, verified):** DNS is **datagram‑tier**, not flow‑tier.
`datagram_stream`/`datagrams` live on the netring **pcap/capture source, not the
`Monitor` builder** (confirmed in `pcap_flow.rs`; the builder only has
`protocol::<P>()` for flow‑tier protocols). So integrating DNS into our
Monitor‑based sensor needs **new plumbing**: run a parallel
`AsyncCapture::datagram_stream(DnsUdpParser::with_correlation())` task on a second
capture handle (or a tee) that feeds the same drain + anomaly sink. Scope this as
a **design spike** first — it's the one feature here that isn't a clean
builder‑hook one‑liner. *Verifiable with flowscope's `dns_queries.pcap`* once
wired. Until then, DGA (D) stays blocked.

## F. L7 HTTP — RED (cleartext) **[Wave 3]**

`.protocol::<Http>()` + `on_ctx::<HttpMessage>` (or `session_stream(HttpParser)`)
→ `http/{requests_total, status_{2xx,3xx,4xx,5xx}_total, latency_p95_ms,
methods/*}`, top hosts → `@/query/http`. **Scope to cleartext**; for TLS/QUIC
degrade to connection‑level RED (don't over‑claim — passive can't read encrypted
URIs). *Pcap‑verifiable* with `http_session.pcap`.

## G. Top‑talkers + elephant flows (on‑demand detail) **[Wave 2/3]**

A per‑destination histogram (state slot updated on `FlowEnded`) → `@/query/talkers
?top=N` ({dst, bytes, packets, flows}); a recent‑large‑flows ring →
`@/query/elephant_flows`. **Why:** "who are the major backends / biggest
transfers?" — operational visibility distinct from per‑app bandwidth. Mirrors the
proven `@/query/flows`/`@/query/tls` pattern.

---

## Testing & sequencing

> Follow [Plan 05](05-architecture-and-conventions.md): capture‑path `on_*`
> handlers stay allocation‑free + lock‑light (atomics; defer formatting to the
> drain); **bound** the telemetry channel with a dropped‑count (never the alert
> channel); detectors are generic over the flow key.

- Pure `map.rs` functions per family unit‑tested (the established pattern).
- Live: pcap replay verifies B, C, D, F, G (flowscope test pcaps:
  `http_session.pcap`, `dns_queries.pcap`, + crafted periodic/flood pcaps). A
  (ICMP) and E (DNS) carry the documented trigger/plumbing caveats — design‑spike
  + honest verification before shipping.
- Wave 1 = A+B+C (per‑proto + beaconing are easy, high‑value; ICMP re‑add).
  Wave 2 = D + E(spike) + G. Wave 3 = F + DGA (after E).
- Anomaly alerts ride the existing `AlertReporter`/`@/alerts` path; allowlist
  every noisy detector.
