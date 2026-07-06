# netring telemetry

Every metric is a serialized `TelemetryPoint` published under
`zensight/netring/<sensor>/<metric>` (JSON or CBOR per the `serialization`
config), via a zenoh-ext `AdvancedPublisher` (per-key cache + late-joiner
history). `<metric>` is a `/`-separated path, so a key can carry more than four
chunks.

The design discipline throughout: **the telemetry bus stays low-cardinality.**
Streamed series are bounded, closed sets (per-L4, per-rcode, per-status-class,
per-detector); high-cardinality detail (individual flows, talkers, assets,
per-process bandwidth) is **served on demand** from the `@/query/*` queryables in
[§ On-demand detail](#on-demand-detail--query), never streamed. Sources: this
crate's `src/map.rs`, `../../docs/KEYSPACE.md`, and `docs/KEYSPACE.md`.

Collector gating is noted per family — a family is only published when its
`collect.*` (or feature/config) switch is on. Defaults are in
[`docs/configuration.md`](configuration.md).

---

## Flow lifecycle & volume (`collect.flows`)

| Metric | Type | Notes |
|---|---|---|
| `flow/started_total` | Counter | TCP/flow starts |
| `flow/ended_total` | Counter | flow ends |
| `flow/active` | Gauge | `started − ended` |
| `flow/bytes_total` | Counter | bytes across completed flows |
| `flow/packets_total` | Counter | packets across completed flows |
| `flow/retransmits_total` | Counter | retransmits across completed flows |

### Flow RED (from netring `red()`)

| Metric | Type | Notes |
|---|---|---|
| `flow/red/rate` | Gauge | request rate (flows/sec) |
| `flow/red/error_ratio` | Gauge | reset + parse-error share |
| `flow/red/p50_ms` `p95_ms` `p99_ms` | Gauge | flow-lifetime percentiles |

Percentiles are **omitted** when the window held no flows (the cached gauge keeps
its last meaningful value instead of being clobbered to zero). This replaced the
pre-#369 bespoke `flow/duration_p*` gauges.

## Per-L4 composition & connection state (issue #16)

| Metric | Type | Notes |
|---|---|---|
| `flow/by_l4/tcp/bytes_total`, `flow/by_l4/tcp/flows_total` | Counter | TCP split |
| `flow/by_l4/udp/bytes_total`, `flow/by_l4/udp/flows_total` | Counter | UDP split (a spike flags DNS/NTP amplification) |
| `flow/by_l4/icmp/bytes_total`, `flow/by_l4/icmp/flows_total` | Counter | ICMP split |
| `tcp/closed_fin_total` | Counter | clean FIN close |
| `tcp/closed_rst_total` | Counter | RST abort/refused (a high share = firewall/IDS drops or instability) |
| `tcp/closed_idle_total` | Counter | idle timeout / other (evicted, buffer_overflow, parse_error fold here) |

## TCP resets (`collect.tcp_resets`)

| Metric | Type | Notes |
|---|---|---|
| `tcp/resets_total` | Counter | RST count |
| `tcp/refused_total` | Counter | connection refusals |

## DNS RED (`collect.dns`, opt-in, cleartext UDP/53)

| Metric | Type | Notes |
|---|---|---|
| `dns/queries_total` | Counter | queries seen |
| `dns/unanswered_total` | Counter | unanswered (resolver loss) |
| `dns/responses_by_rcode/<slug>_total` | Counter | per-rcode: `noerror`/`nxdomain`/`servfail`/`refused`/`other` |
| `dns/query_rtt_p50_ms` `p95_ms` `p99_ms` | Gauge | query-RTT percentiles (omitted on an empty window) |

Top-SLD detail rides `@/query/dns` (not streamed).

## HTTP RED (`collect.http`, opt-in, cleartext TCP/80,8080 — TLS is opaque)

| Metric | Type | Notes |
|---|---|---|
| `http/requests_total` | Counter | requests |
| `http/status_2xx_total` … `status_5xx_total` | Counter | per status class |
| `http/methods/<verb>_total` | Counter | per HTTP verb (closed set) |
| `http/latency_p50_ms` `p95_ms` | Gauge | request→response latency (omitted on an empty window) |

Top-host detail rides `@/query/http`.

## TLS / QUIC / SSH fingerprints

| Metric | Type | Gate | Notes |
|---|---|---|---|
| `tls/handshakes_total` | Counter | `collect.tls` | ClientHellos fingerprinted |
| `tls/distinct_fingerprints` | Gauge | `collect.tls` | asset-inventory size (JA3/JA4) |
| `tls/pq_ratio` | Gauge | `collect.tls` | share of handshakes offering a post-quantum (hybrid) key-share group; `0.0` when none |
| `quic/distinct_sni` | Gauge | `collect.quic` | distinct (SNI, version) pairs (passive QUIC Initial, UDP/443) |
| `ssh/distinct_hassh` | Gauge | `collect.ssh` | distinct HASSH fingerprints (TCP/22) |

Per-fingerprint detail rides `@/query/{tls,quic,ssh,ja4h}` (see below).

## Encrypted DNS (`collect.encrypted_dns`, netring 0.29 / #326)

Classifies DoT/DoQ/DoH from the TLS/QUIC handshake:

| Metric | Type | Notes |
|---|---|---|
| `dns/encrypted/dot` | Counter | DNS-over-TLS sessions |
| `dns/encrypted/doq` | Counter | DNS-over-QUIC sessions |
| `dns/encrypted/doh` | Counter | DNS-over-HTTPS sessions |
| `dns/encrypted/unknown_resolver` | Counter | sessions to a resolver not classed as known-public (the policy-bypass signal) |
| `dns/encrypted/distinct` | Gauge | distinct destinations |

Per-destination detail rides `@/query/encrypted_dns`. Pairs with the
`encrypted_dns_bypass` anomaly (see [detectors.md](detectors.md)).

## ICMP errors (`collect.icmp`, opt-in, live-gated)

Synthesised from the embedded inner packet; needs live capture with real kernel
ICMP (a synthetic pcap rarely triggers it — degrades to silent zero counters
under replay).

| Metric | Type | Notes |
|---|---|---|
| `icmp/unreachable_total` | Counter | Destination-Unreachable family |
| `icmp/time_exceeded_total` | Counter | TTL expired in transit / reassembly |
| `icmp/mtu_signal_total` | Counter | frag-needed / packet-too-big (PMTU black-hole risk) |
| `icmp/by_kind/<slug>_total` | Counter | per stable ICMP-error slug (≈8 bounded classes) |

An ICMP error that terminates a live flow also raises an `IcmpFlowError` anomaly
alert (bucketed by `dst`).

## Bandwidth top-talkers (`collect.bandwidth`)

| Metric | Type | Notes |
|---|---|---|
| `bandwidth/<app>/bytes_per_sec` | Gauge | per-application rolling rate; the application name rides as the `app` label |

Emitted on the `bandwidth_period_secs` cadence. This is the *per-application*
stream; the opt-in *per-process wire* tier is query-only on `@/query/bandwidth`
(see below).

## Passive asset inventory (`collect.assets`)

| Metric | Type | Notes |
|---|---|---|
| `assets/discovered` | Gauge | distinct assets (MACs) currently held |

Per-asset detail (MAC/IPs/hostname/vendor/platform/role/fingerprints/seen-via)
rides `@/query/assets`. Discovery sources: ARP / NDP / LLDP (+ CDP via
`collect.asset_cdp`).

## Per-detector anomaly counters (#254)

| Metric | Type | Notes |
|---|---|---|
| `anomaly/<kind>/total` | Counter | monotonic count of anomalies a detector has fired since start (e.g. `anomaly/BeaconRita/total`, `anomaly/DnsTunnel/total`) |

The `<kind>` slug equals the alert `rule`, so the streamed counter correlates
with the alerts on `@/alerts/*`. Lets the GUI Overview anomaly strip roll up
per-detector activity without a Security-view round-trip.

## Capture self-health (`collect.capture_stats`)

Live-only — the kernel ring has no drops to report under pcap replay. Non-zero
drops are the honest "the sensor's *other* telemetry is incomplete" signal.
`<source>` is the capture-leg index.

| Metric | Type | Notes |
|---|---|---|
| `capture/<source>/packets` | Counter | packets seen |
| `capture/<source>/drops` | Counter | packets dropped |
| `capture/<source>/drop_rate` | Gauge | windowed drop fraction |
| `capture/<source>/freezes` | Counter | AF_PACKET (TPACKET_v3) ring freezes |
| `capture/<source>/xdp/<cause>` | Counter | AF_XDP per-cause drops: `rx_dropped`, `rx_invalid_descs`, `rx_ring_full`, `rx_fill_ring_empty_descs`, `tx_invalid_descs`, `tx_ring_empty_descs` |
| `capture/backend` | Text | resolved backend (or `pcap-replay`) — #227 |

The windowed drop-rate feeds the `capture-overload` SensorHealth alert (hysteresis
enter 5% / recover 1% × 3 windows). See [detectors.md](detectors.md) and the
`overload` config.

### Load-shedding (`overload.shed.enabled`, opt-in / #224)

| Metric | Type | Notes |
|---|---|---|
| `capture/<source>/shed/new_flows_total` or `.../sampled_total` | Counter | deliberately-shed flows (leaf per policy) |
| `capture/<source>/shed/active` | Gauge | `1` while shedding |

## Capture focus (`capture_focus.enabled`, opt-in / #225)

| Metric | Type | Notes |
|---|---|---|
| `capture/focus/packets` | Counter | packets seen by the reloadable focus tap |
| `capture/focus/bytes` | Counter | bytes seen — narrowing the runtime BPF filter slows these for non-matching traffic (the visible effect of a hot-swapped focus) |

## Capture-to-disk (`capture.to_disk.mode != off`, opt-in / #327)

| Metric | Type | Notes |
|---|---|---|
| `capture/disk/mode` | Text | `off` / `rotating` / `triggered` |
| `capture/disk/ring_packets`, `capture/disk/ring_bytes` | Gauge | pre-trigger ring occupancy |
| `capture/disk/retained_files`, `capture/disk/retained_bytes` | Gauge | on-disk retention usage |
| `capture/disk/dropped` | Counter | frames dropped from the ring |
| `capture/disk/evictions` | Counter | retention evictions |
| `capture/disk/triggers` | Counter | triggers fired |
| `capture/events` | Text | lifecycle feed (trigger fired / capture ready / mode switch), with an `event` label |

The finished-capture file index rides `@/query/captures`.

---

## On-demand detail — `@/query/<topic>`

High-cardinality detail is served on request, never streamed. Parameters are
Zenoh selector params (e.g. `?top=20`). netring uses the host-less
`command::query_key` form (`zensight/netring/@/query/<topic>`).

| Topic | Reply | Gate |
|---|---|---|
| `flows` | `Vec<FlowRecord>` — 5-tuple, bytes/packets, per-direction split, close reason, Community ID, `dst_names` (passive DNS) | `collect.flows` |
| `elephant_flows` | `Vec<ElephantRecord>` — heaviest flows | `collect.flows` |
| `talkers?top=N` | `Vec<TalkerRecord>` — top source rates, with passive-DNS `names` | `collect.talkers` |
| `matrix?top=N` | `Vec<MatrixRecord>` — `(src,dst)` byte/packet/flow pairs (the "who talks to whom" service map, #122) | `collect.talkers` |
| `tls` | `Vec` of TLS fingerprint records | `collect.tls` |
| `dns?top=N` | `Vec<DnsRecord>` — top SLDs (queries + NXDOMAIN) | `collect.dns` |
| `http?top=N` | `Vec<HttpHostRecord>` — top hosts (requests + errors) | `collect.http` |
| `quic` | QUIC Initial SNI/ALPN/version/JA4 inventory | `collect.quic` |
| `ssh` | SSH banner + client/server HASSH + KEXINIT inventory | `collect.ssh` |
| `encrypted_dns` | per-destination DoT/DoQ/DoH inventory | `collect.encrypted_dns` |
| `ja4h?top=N` | `Vec<Ja4hRecord>` — JA4H HTTP fingerprints | `collect.http_fp` **and** `--features ja4plus` |
| `assets` | `Vec` asset records (MAC/IPs/hostname/vendor/platform/role/first-seen/fingerprints/seen-via) | `collect.assets` |
| `captures` | `Vec<CaptureRecord>` — capture-to-disk file index; triggered files carry the firing detector + `artifact_id` while their serve TTL lives | `capture.to_disk.mode != off` |
| `bandwidth?top=N` | `Vec<BandwidthRecord>` — per-process **wire-L2** bandwidth, tagged `bw.source=netring` / `bw.semantics=wire-l2` / `bw.proto=all` | `bandwidth_attribution` |

`ja4h` is absent on a build without `--features ja4plus` (FoxIO License 1.1 —
non-OSI); JA4SSH is not yet available upstream, so SSH is fingerprinted via HASSH
on the `ssh` channel.

### Traffic matrix / service map (#122)

Alongside the per-destination talker histogram, the `(src,dst)`-keyed
byte/packet/flow matrix on `@/query/matrix?top=N` answers "who talks to whom" for
the GUI service-map view. TCP initiator inference (`collect.infer_initiator`,
default on) labels each pair client → server regardless of capture endpoint
order.

### Passive DNS enrichment (#308)

With `collect.dns` + `names` (default on), DNS answers are parsed into a
client-scoped IP↔name cache (flowscope `NameMap` — follows CNAME chains,
glue-poisoning-safe, PTR-aware). Flow records gain `dst_names` and talker/matrix
records gain `names`, provenance-ranked (forward DNS > CNAME > SNI > mDNS > DHCP
> PTR), top-3. The cache also keys the FQDN-pivoted beacon detector.

### Per-process wire bandwidth (#318, opt-in)

`bandwidth_attribution` joins netring's live per-flow wire bandwidth against the
kernel socket table (a periodic sock_diag dump + `/proc` fd scan, off the capture
hot path) to attribute full-frame (wire-L2, undirected — the whole rate is
`tx_bps`) throughput to owning processes. **Best-effort**: a flow whose socket
isn't in the current dump (or whose process already exited) lands in an explicit
unattributed bucket (`pid = -1`). It costs `/proc` walks, hence opt-in; reads are
unprivileged. The tag triplet keeps the GUI from blending it with netlink's
TCP-goodput or systemd's wire-L3 numbers.

---

## Alerts — `@/alerts/<alert_key>`

Detector, threat-intel, and capture-health alerts publish as a lifecycle
(firing → resolved → tombstone) on `zensight/netring/@/alerts/<alert_key>`, where
`<alert_key>` is a stable hash of `source + rule + sorted-labels`. The current
firing set is seeded to late joiners via the `@/query/alerts` queryable. Offending
IP/domain/5-tuple detail lives in alert **labels** (with MITRE ATT&CK `technique`
and cross-tool `community_id` where the 5-tuple is whole), never in a metric
series name — see [detectors.md](detectors.md) for the full detector surface.

Capture-health alerts (`AlertKind::SensorHealth`): `capture-overload` (silent
packet loss) and `capture-leg-asymmetry` (#226 — a flow whose two directions
arrived on mismatched capture legs; tap miswire or asymmetric routing).

## Identity evidence — `zensight/_meta/evidence/**` (#307)

With `evidence` on (default), netring republishes third-party identity claims for
the correlator: observed-asset `HostEvidence` (`observer=netring`, from the asset
inventory) and passive-DNS `NameObservation`s (one per IP). Rate-limited
(per-source min-interval + per-tick cap) and TTL-aged. **Gating:** asset evidence
needs `collect.assets`; name evidence needs `collect.dns` — with those collectors
off (the shipped default) netring emits no evidence even though `evidence.enabled`
is true. See `docs/KEYSPACE.md` for the evidence/entity contract.
