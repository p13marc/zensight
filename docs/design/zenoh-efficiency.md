> **Archived design doc.** Historical rationale; the core (QoS, CBOR default, declared
> publishers, media plane) shipped in 0.7.0. For current documentation see
> [`docs/KEYSPACE.md`](../KEYSPACE.md) and [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md).

# ZenSight Zenoh Efficiency — QoS, Advanced Pub/Sub, Declarations, Media & Keyspace

*Analysis / design report for low-bandwidth & unreliable links. Companion to
[`KEYSPACE.md`](KEYSPACE.md) (the key-expression contract), [`SENSORS.md`](SENSORS.md),
and the parallax video-sensor report (`zensight-parallax-sensor.md`).*

> **Status (0.7.0, epic #352):** the core is **SHIPPED** — R1 QoS (#353), R2
> heartbeat + R3 correlator publisher (#354), R4 CBOR default (#355), R5
> declare-all + `session.put` CI guard (#356), R6 exporter subscription scope
> (#357). **Deferred:** R6 frontend `link_profile` (#364), R7 log-events keyspace
> (#358), R9 media/video plane (#359), R8 (optional, wholesale advanced-pub
> reconsideration).

## Context

ZenSight must run well over **low-bandwidth and unreliable links** (field sensors on
cellular/satellite/mesh backhaul, a GUI or exporter reaching a remote site) **and grow a
video plane** (the incoming parallax sensor; the frontend must display streams). Directives:

1. Use **AdvancedPublisher/AdvancedSubscriber only when needed** — they are cost-heavy
   (per-key cache, sequence numbers, miss-detection heartbeat, history bursts).
2. Use **Zenoh QoS flags** deliberately — `reliability`, `congestion_control`, `priority`,
   `express`.
3. **Never `session.put()`** — always **declare** a Publisher/Subscriber first (interns the
   keyexpr to a numeric id *and* primes router routing tables — a bandwidth win, not just a
   local one).
4. **Improve the keyspace**, incl. a **media plane** so ZenSight can carry/display video —
   opaque payload (no imposed serialization), unreliable/drop QoS.

The whole workspace was mapped and Zenoh 1.x semantics cross-checked against the docs.
**Headline:** the *keyspace design is already sound* and extends cleanly to media; the wins
are (a) per-traffic-class QoS (currently **zero** QoS is set anywhere), (b) fixing an
AdvancedPublisher heartbeat bug + trimming two over-provisioned advanced paths, (c) CBOR by
default for the telemetry envelope, (d) **declaring every pub/sub and banning raw
`session.put()`**, (e) taming the consumer-side `zensight/**` firehose, and (f) an opaque
`@media` plane for video.

---

## TL;DR — prioritized

| # | Change | Why (link constraint) | Effort | Risk |
|---|---|---|---|---|
| **R1** | **Per-class QoS**: telemetry `BestEffort+Drop+DataLow`; alerts/commands/evidence/entities `Reliable+Block+InteractiveHigh/Data`; live video `BestEffort+Drop+InteractiveHigh` | Correctness + freshness on lossy links; **fixes silently-droppable alerts** | S–M | Low |
| **R2** | **Fix the `cache_only` heartbeat bug** (honor `miss_detection`/`publisher_detection`); drop miss-detection heartbeat from cache-only paths | Kills a 500 ms/key background heartbeat firehose | XS | Low |
| **R3** | **Downgrade the correlator entity publisher** to a plain **declared** `put`/`delete` | Advanced machinery there is unused (consumer seeds from queryable) | XS | Low |
| **R4** | **Default serialization → CBOR** (telemetry/control envelope only) | ~1.4–1.6× smaller payloads, near-zero effort, decode auto-detects | XS | Low |
| **R5** | **Declare every pub/sub; ban raw `session.put()`/`session.delete()`** — route all publication through a declared-`Publisher` registry | Interns keyexpr → numeric id **and primes router routing tables**; no full-key-string-per-sample anywhere | M | Low |
| **R6** | **Tame the firehose consumer-side**: exporters subscribe selectively; GUI gains protocol/source scoping + a low-bandwidth mode (no `history()/recovery()`) | The GUI + both exporters hard-wire `zensight/**` (+ a history burst at connect) | M | Med |
| **R7** | **Move high-cardinality log events off the streamed bus** (`events/<uid>` → pull/queryable or an opt-out subtree) | Violates the KEYSPACE "detail served on request" rule; dominates a slow link | M | Med |
| **R9** ✅ *(enabler landed, #359)* | **Media/video plane**: opaque `@media` keyspace, no serialization envelope, `QosClass::LiveVideo` (`BestEffort+Drop+InteractiveHigh`), plain per-stream pub/sub with keyframe-on-subscribe, stream control on `@/commands\|query\|status/stream(s)`; frontend gains video display | Lets ZenSight carry/show video **without** the firehose ever touching telemetry consumers | L | Med |
| **R8** | *(deeper, optional)* Reconsider AdvancedPublisher for telemetry entirely — serve GUI late-join history from the local store / a queryable | Removes per-key caches + recovery from hundreds of keys | L | Med–High |

R1–R4 are the quick, safe, high-leverage wins. R5–R7 are the firehose + declarations. R9 is
the video track (paired with the parallax sensor). R8 is a design fork to decide, not do now.

---

## 1. Current state (what the sweeps found)

**QoS: none, anywhere.** Not a single `.congestion_control()`, `.priority()`, `.express()`,
or `.reliability()` call exists in the workspace — every publish/put/get/queryable runs on
library defaults. The session config (`zensight-common/src/session.rs:7-60`) sets only
`mode`/`connect`/`listen`; no transport/batching/scouting tuning. Zenoh's `put` default is
`CongestionControl::Drop` — **fine for telemetry, wrong for alerts/control** (see R1).

**Raw `session.put()` is used widely — undeclared.** Two publish patterns coexist:
declared AdvancedPublishers (netlink/netring/systemd telemetry) vs. **one-shot undeclared
`session.put()`** on
- five+ telemetry sensors (sysinfo `collector.rs:1470` ~60 sites, modbus, netflow, logs,
  gnmi; snmp disputed — verify),
- the **entire control plane** via `Publisher::publish_raw`/`publish_json`/`delete`
  (`publisher.rs:118-147`) → health / liveness / errors / **alerts** / commands / status,
- the **frontend** command sends (`app.rs:2498,2723`, `artifact_fetch.rs:338`),
- the **zenoh-blob** store/tree puts, and the **correlator** entity publisher's siblings.

Every one of those sends the full key *string* on the wire per message and skips the router
routing-table optimization that a declared publisher primes (see R5).

**Serialization: JSON by default.** `Format::default() = Json`
(`zensight-common/src/serialization.rs:8-15`); every shipped config sets
`serialization: "json"`. CBOR is fully implemented and `decode_auto` already sniffs format by
first byte — a drop-in. A representative SNMP `TelemetryPoint` is ~175 B JSON vs ~110–125 B
CBOR (bigger win for numeric-heavy points).

**AdvancedPublisher is the telemetry path — with a bug.** `AdvancedPublisherRegistry`
(`zensight-sensor-core/src/advanced_publisher.rs`) declares each key with `cache(10)` +
`sample_miss_detection(heartbeat 500ms)` + `publisher_detection()`. **The builder
(`:168-176`) never reads the `miss_detection`/`publisher_detection` config bools** — so
`cache_only(1)` publishers (netring evidence/names, runner SensorInfo/self-evidence) *still*
run a 500 ms heartbeat + miss/publisher detection on every key. Heartbeats ride
`CongestionControl::Block` (won't drop) → a guaranteed background stream of ~2 msgs/key/sec
that a low-bandwidth link cannot shed.

**The firehose is consumer-side.** Three high-volume consumers hard-wire `zensight/**`:
- **Frontend** (`zensight/src/subscription.rs:198-213`) — an **AdvancedSubscriber** with
  `history().detect_late_publishers()` + `recovery()`. At connect, every AdvancedPublisher
  replies its cache (≤10 samples × N keys) → a **history burst** (documented deadlock-risk,
  `:183-197`), then the full all-source/all-protocol/all-metric live stream. No scoping knob.
- **Prometheus & OTel exporters** (`.../subscriber.rs:16,112`) — plain `zensight/**`, then
  discard `/@/` and `_meta` **client-side** (`is_telemetry_key`). They pull more than they
  need over the wire, then throw it away. Both already expose a `with_key_expr` override —
  just not wired to config.

**No media path today.** The frontend cannot display video (iced `image` feature off, no
decoder dep); `Publisher` is hard-wired to the telemetry envelope (no raw/opaque publish, no
QoS surface). This is the greenfield in R9 (and the parallax report).

**The good news (keep as-is):** alerts already avoid the *advanced* path (plain put/delete +
a `@/query/alerts` queryable seed — they just need to become *declared* + `Reliable`); the
correlator is metadata-scoped and touches **zero** telemetry — the model low-bandwidth
consumer; all high-cardinality detail is pull-only via `@/query/<topic>`; and the
`@`-verbatim rule (`*`/`**` never cross a chunk starting with `@`) already partitions
telemetry / control / (future) media by key expression alone.

---

## 2. Recommendations

### R1 — Per-traffic-class QoS (the core change)

Set QoS explicitly at each declared publisher by traffic class. Default `Drop` is right for
telemetry but silently wrong for alerts — this is the highest-correctness item.

| Class | Keys | Reliability | Congestion | Priority | Express |
|---|---|---|---|---|---|
| **High-freq telemetry** | `zensight/<proto>/<source>/<metric>` | BestEffort | **Drop** | DataLow | off |
| **Health / liveness / errors** | `@/health`, `@/devices/*/liveness`, `@/errors` | BestEffort | Drop | Data | off |
| **Alerts (state change)** | `@/alerts/<key>` put+**delete** | **Reliable** | **Block** | InteractiveHigh | off |
| **Commands / status** | `@/commands/*`, `@/status/*` | **Reliable** | **Block** | InteractiveHigh | off |
| **Evidence** | `_meta/evidence/**` | **Reliable** | **Block** | Data | off |
| **Entities** | `_meta/entity/**` | **Reliable** | **Block** | Data | off |
| **Queryable replies / artifacts** | `@/query/*`, `@/artifact/*` | Reliable | Block | **DataLow** (bulk) | off |
| **Live video (inter-frames)** | `@media/<stream>/video/…` | **BestEffort** | **Drop** | **InteractiveHigh** | off¹ |
| **Video keyframes / JPEG preview** | `@media/<stream>/…` | Reliable² | Block² | InteractiveHigh | off |

¹ Express is the one per-*stream* video toggle: it cuts latency at the cost of bandwidth, so
default it **off** for constrained links and expose it for low-latency-on-capable-links.
² Or keyframe-on-subscribe instead of reliable keyframes (R9) — pick one.

Rationale:
- **Telemetry `Drop`+`BestEffort`+low priority**: a lost sample is superseded by the next;
  never back-pressure a sensor because the link is slow, and never let telemetry starve
  control traffic. (Mostly matches today's implicit default — the value is making it explicit
  *and* low-priority so control wins under congestion.)
- **Alerts/commands `Reliable`+`Block`+high priority**: rare, small, and **must arrive**.
  Today an alert `put` (or its resolve-tombstone `delete`) inherits `Drop` → on a lossy link
  it can vanish, stranding a live GUI in a stale firing/resolved state (the `@/query/alerts`
  seed only rescues *new* joiners). A genuine reliability bug on unreliable links.
- **Video `Drop`+`BestEffort`**: a dropped inter-frame is corrected by the next GOP; never
  block the encoder because the link is slow. Keyframes are the loss-fragile exception (R9).
- **`express` = off by default everywhere** (incl. video on constrained links). Express
  disables batching to cut latency **at the cost of bandwidth/overhead** — the wrong trade on
  a low-bandwidth link. Priority already orders alerts/video ahead of telemetry without the
  express tax.

**Where:** thread a `QosClass → (reliability, congestion, priority, express)` helper in
`zensight-common` into the declared-publisher builders (R5) so every call site is one enum,
not four knobs — the AdvancedPublisher builder (`advanced_publisher.rs:168-176`, telemetry
Drop/DataLow), the plain `Publisher` control path, the alert emit path (`alert.rs:228-233` +
tombstones `:169,:220`), the correlator, and the media publisher (R9). Optionally expose
per-key-expression QoS overrides in the Zenoh session config (supported since 1.1) as an ops
escape hatch.

### R2 — Fix the `cache_only` heartbeat bug

In `advanced_publisher.rs:168-176`, honor the config: only call `.sample_miss_detection(...)`
when `config.miss_detection`, only `.publisher_detection()` when
`config.publisher_detection`, and gate/raise the heartbeat. Then set the cache-only paths
(netring evidence/names `main.rs:224-249`, runner SensorInfo/self-evidence `runner.rs:367`)
to genuinely cache-only. Separately, **reconsider the 500 ms heartbeat** even on real
telemetry publishers: for periodic telemetry, Zenoh's guidance is that sample-recovery is
*useless when the publish period ≤ the recovery period* — the next sample supersedes the lost
one — so miss-detection + heartbeat buys little for fast metrics while costing a constant
per-key background stream. Recommend heartbeat **off** (or ≥ several seconds) for
high-frequency telemetry; keep tight recovery only where a gap genuinely matters.

### R3 — Downgrade the correlator entity publisher to a plain **declared** `put`/`delete`

`zensight-correlator/src/publisher.rs:43-53` declares an AdvancedPublisher (cache 1 +
miss-detect + pub-detect) for `HostEntity` docs, but the sole consumer — the frontend —
subscribes with a **plain** subscriber (`subscription.rs:74-78`) and seeds from the
`entities_query_key()` queryable (`:167-180`). The advanced cache/recovery is never consumed.
Replace with a **declared** plain `Publisher` doing `put`/`delete` (+ keep the queryable
seed) — exactly the alerts pattern, and consistent with R5 (declared, not `session.put`).
Pure cost removal, no behavior change.

*(Keep the correlator's evidence **subscribers** advanced — their `history()` rebuilds entity
state across restarts, and the evidence **publishers'** cache is legitimately paired with it.
That's the "when needed" case.)*

### R4 — Default serialization → CBOR (envelope only)

Flip `Format::default()` to `Cbor` (`serialization.rs:10`) and the shipped `configs/*.json5`
to `"cbor"`. `decode_auto` already sniffs per-message, so mixed-version fleets keep
interoperating during rollout. Tighten the unit test (`serialization.rs:105-117`) from
`cbor.len() < json.len()` to a ratio floor. **Scope:** `serialization::Format` governs the
**structured telemetry/control envelope only** (`TelemetryPoint`, alerts, evidence,
entities). The media plane (R9) is deliberately **exempt** — it carries opaque bytes, no
serde envelope. Serialization is per-plane, never imposed on binary media.

### R5 — Declare every publisher/subscriber; ban raw `session.put()`/`session.delete()`

**Rule:** every publication goes through a **declared** `Publisher` (plain or advanced) and
every subscription through a **declared** `Subscriber`/queryable. Raw `session.put()`,
`session.delete()`, and one-shot publishes are forbidden. Declaring registers the keyexpr as
a numeric id **in the routers' routing tables** and signals intent so Zenoh sets up
forwarding optimizations; an undeclared `put` re-resolves the full key string on every
message and skips that optimization — the cost lands squarely on a low-bandwidth hop.

**Mechanism:** generalize the lazy-registry already in `AdvancedPublisherRegistry` into a
plain `PublisherRegistry` (`HashMap<OwnedKeyExpr, Publisher>` with the QoS class from R1),
and route through it:
- **Control plane** — rework `Publisher::publish_raw`/`publish_json`/`delete`
  (`publisher.rs:118-147`) to declare-and-cache per control key (health/liveness/errors/
  alerts/commands/status) instead of `session.put`/`delete`.
- **Telemetry sensors on the plain path** — sysinfo/modbus/netflow/logs/gnmi (+ snmp if
  applicable): declare a publisher per metric key (the verbose gnmi keys benefit most).
- **Correlator** (R3), **frontend** command sends (`app.rs:2498,2723`,
  `artifact_fetch.rs:338`), and **zenoh-blob** store/tree puts.
- **Dynamic keyspaces** (per-`alert_key`, per-device liveness): declare-on-first-use + cache;
  undeclare on retire only if the key space is unbounded (alert keys are bounded — fine).

**Enforcement:** add a CI grep guard (mirroring the existing design-system color guard) that
fails the build on `session.put(` / `session.delete(` / bare `.put(` outside the
publisher-registry modules. Locks the rule in so it can't silently re-break.

### R6 — Tame the firehose (consumer-side)

- **Exporters:** wire the existing `with_key_expr` override to config and default it to a
  **telemetry-only** subscription (a per-protocol union, or at minimum exclude `_meta`), so
  the `is_telemetry_key` discard happens at the router, not after the bytes cross the link.
  Keep the separate `@/alerts/*` sub.
- **Frontend:** add subscription scoping — per-protocol / per-source / control-only — and a
  **low-bandwidth mode** that (a) drops `history()`+`recovery()` on the main subscription
  (rely on the local redb store + selective `@/query` seeds for back-fill) and (b) narrows
  `zensight/**` to the protocols the current view needs. The store already persists
  telemetry, so the GUI does not need per-key network history to show recent data after a
  reconnect.

### R7 — Move high-cardinality log events off the streamed bus (keyspace)

Per-line log events keyed `zensight/logs/<source>/events/<uid>` (KEYSPACE.md:64-76) are a
high-cardinality, high-volume stream riding the same `zensight/**` firehose — which
contradicts the keyspace's own "high-cardinality detail is served on request, never streamed"
principle that flows/sockets/processes already follow. On a slow link this stream alone can
dominate. Options (pick during design): (a) serve log events via a `@/query/logs` queryable +
a low-rate rollup on the streamed bus (mirrors netring/netlink), or (b) put them under an
opt-out subtree the GUI can exclude by default. The one telemetry-side keyspace fix with real
teeth.

### R9 — Media/video plane (opaque, request-driven; parallax)

> **Status — zenoh-side enabler landed (#359).** Shipped: `media_video_key()` /
> `media_preview_key()` on the `@media` verbatim sibling (guard tests pin that
> `zensight/**` and `zensight/*/@/**` both miss it); `QosClass::LiveVideo`
> (`BestEffort+Drop+InteractiveHigh`, express off); `Publisher::raw_media_publisher()`
> → `RawMediaPublisher` (plain publisher; `put(bytes, Encoding, attachment)`;
> `matching_listener()` for **keyframe-on-subscribe** — option (a) below); stream
> control types (`StreamControl`/`StreamDescriptor`/`StreamStatus`) carried in
> `Command<T>` on `@/commands/stream` + `@/query|status/streams`; both exporters'
> `is_telemetry_key` now reject **any** `@`-prefixed chunk (regression-tested); an
> e2e test drives catalogue-query → OpenStream → matching-listener keyframe. The
> **frontend** gained the iced `image` feature (JPEG-only codec, no AVIF/ravif),
> `Protocol::Parallax`, and `view/specialized/parallax.rs` (placeholder preview +
> a `preview_handle_from_jpeg` decode seam). Still out of scope: the
> H.264/parallax encoder daemon and the live media-subscription pipeline feeding
> real frames into the GUI.

ZenSight must carry and display video (the parallax sensor). Video is opaque, high-rate, and
loss-tolerant — a different plane from telemetry. Add it as a first-class, invisible sibling
to `@/`, per the parallax report §4.

**Keyspace — a general `@media` verbatim sibling** (invisible to `zensight/**` telemetry *and*
`zensight/*/@/**` control, by the `@`-verbatim rule → the video firehose can never leak into
telemetry/exporter/GUI-firehose consumers):
```
zensight/<proto>/<source>/@/query/streams              # queryable: list StreamDescriptors
zensight/<proto>/<source>/@/commands/stream            # Command<Open|Close|RequestKeyframe>
zensight/<proto>/<source>/@/status/streams             # queryable: current sessions/profiles
zensight/<proto>/<source>/@/devices/<stream>/alive     # per-stream advertise (openable)
zensight/<proto>/<source>/<stream>/stats/<metric>      # normal telemetry: fps/kbps/drops/viewers
zensight/<proto>/<source>/@media/<stream>/video/<codec>/<profile>   # MEDIA: one AU per sample
zensight/<proto>/<source>/@media/<stream>/preview/jpeg              #        opaque, Drop QoS
```
(For parallax, `<proto>=parallax`, `<source>=<host>`.) Stream **control** stays on `@/`;
stream **stats** ride normal CBOR telemetry so existing charts light up for free; only the
**pixels** ride `@media`.

**No imposed serialization.** A media sample is the raw encoded access unit (opaque bytes)
carrying a Zenoh `Encoding` hint (`video/h264`, `image/jpeg`, …) + an **attachment** with
frame metadata (PTS/DTS/flags/format) — **not** the `TelemetryPoint`/`Format` envelope. This
needs a framework media-publisher API (`raw bytes + encoding + attachment + QoS`), the
`raw_media_publisher()` from the parallax report (Z2). `serialization::Format` never touches
this plane.

**QoS (R1 rows):** live video `BestEffort + Drop + InteractiveHigh`, express off (per-stream
toggle). Zenoh fragments a whole IDR into one sample — no RTP/MTU packetization needed. **The
keyframe exception:** a lost keyframe corrupts until the next GOP, so either (a)
**keyframe-on-subscribe** — a Zenoh **matching listener** fires when a subscriber appears and
the sensor calls `force_keyframe()` (parallax P7), the cheapest fix; or (b) publish keyframes
on a `Reliable`/`Block` sub-key. Prefer (a). Between co-located peers, Zenoh 1.6+ **implicit
SHM promotion** gives the frontend zero-copy frames for free.

**Declared + request-driven (R5).** The sensor **declares** a Publisher on the concrete
`@media/<stream>/…` key when a stream opens (Open command → session-manager builds the
pipeline); the frontend **declares** a Subscriber on that exact key on request — **never** a
wildcard, never the firehose. Both declared → routing optimized.

**Frontend display.** iced `image` feature + a decode path: a parallax receive pipeline
(`ZenohSrc → [depay] → H264Decoder → AppSink` → RGBA → `iced::widget::image`), JPEG preview
first (cheap, no codec). Zenoh side is in scope here; the decode pipeline is the parallax
sensor's deliverable (see `zensight-parallax-sensor.md` Z3). Add `view/specialized/parallax.rs`.

### R8 — *(optional, decide later)* Reconsider AdvancedPublisher for telemetry wholesale

The deepest lever: if GUI late-join history is served from the local store + queryables (R6),
telemetry publishers could become **plain declared** publishers (no cache, no recovery, no
heartbeat) across the board — removing the advanced machinery from hundreds of keys. Bigger
change, needs a decision on how much network-side history the GUI should retain vs.
reconstruct locally. Flagged, not recommended for the first pass.

---

## 3. Keyspace verdict

**Keep:** the family split (telemetry `zensight/<proto>/<source>/…` / control
`zensight/<proto>/@/…` / metadata `zensight/_meta/…`), the `@`-verbatim separation rule, the
pull-only `@/query/*` and `@/artifact/*` model, and the low-cardinality alert-key hashing.
Selective-consumption builders already exist (`protocol_wildcard`, `source_wildcard`, the
control-only wildcards) — just unused by the big consumers (R6). **Add two things, both
additive and both preserving the `@`-verbatim invariant:**
- **`@media/<stream>/…`** (R9) — a new opaque, verbatim sibling for binary streams. Generic
  beyond parallax (any high-rate opaque feed); invisible to every existing wildcard consumer.
- Move **log events** off the streamed bus (R7).

No restructuring of existing keys is needed.

---

## 4. Phased rollout

- **Phase A — safe quick wins (R2, R3, R4):** fix the heartbeat bug, downgrade the entity
  publisher (declared plain), default to CBOR. Small, low-risk, immediately cuts background +
  payload bytes. Ship first.
- **Phase B — QoS (R1):** add the `QosClass` helper and apply per class; the alert-reliability
  fix is the correctness centerpiece. Regression-test that alerts publish `Reliable`+`Block`.
- **Phase C — declarations + firehose (R5, R6):** build the plain `PublisherRegistry`, route
  every publish through it, add the `session.put` CI guard; wire exporter selective
  subscriptions + GUI scoping/low-bandwidth mode. (R5 is cross-cutting — do the registry once,
  then migrate call sites incrementally; the guard flips on when the last raw put is gone.)
- **Phase D — telemetry keyspace (R7):** move log events to pull/rollup.
- **Video track (R9):** paired with the parallax sensor — media publisher API + `@media`
  keyspace + stream control/stats + frontend video display. Independent of A–D; depends on
  parallax P1 (metadata attachment) and Z3 (frontend decode). Start with JPEG preview.
- **Later — R8** if measurements justify it.

Each phase is independently shippable and independently valuable.

## 5. Verification

- **Declarations / no raw put:** the CI grep guard (R5) is the durable check; plus assert the
  `PublisherRegistry` reuses one declared `Publisher` per key across N publishes.
- **Bytes-on-wire:** a throwaway `zenohd` + a counting subscriber (or `z_sub` with stats)
  against one sensor before/after each phase — compare sample size (CBOR vs JSON), background
  rate (heartbeat on/off), connect-burst size (history on/off), and declared-vs-undeclared key
  overhead. Fits the repo's existing peer-scouting throwaway-subscriber practice.
- **Correctness:** unit-test the `QosClass → QoS` mapping; regression-assert alerts publish
  `Reliable`+`Block` and telemetry `Drop`; assert `decode_auto` round-trips CBOR↔JSON. Keep
  green: the exporter `alerts_need_their_own_subscription` intersection tests, `serialization`
  round-trip.
- **Media plane:** assert `@media/**` does **not** intersect `zensight/**` or `zensight/*/@/**`
  (a keyexpr test, mirroring the alerts guard); assert an opaque AU round-trips with its
  `Encoding` + attachment and **without** touching `serialization::Format`; a declared
  per-stream pub/sub + `Open`→subscribe→keyframe-on-subscribe flow (mock, then pcap/live).
- **Link emulation:** exercise sensor→GUI (and a video stream) under `tc netem` (loss + rate
  limit) — alerts survive, telemetry degrades gracefully, video drops frames without blocking.
- Full workspace gate per change: `cargo test --workspace`, `clippy -D warnings`, `fmt --all`
  (exclude snmp/gnmi *builds* — no openssl/protoc — but all mains must fmt).

## 6. Risks / open questions

1. **Verify snmp's actual publish path** (advanced vs plain — the sweeps disagreed) before R5.
2. **CBOR default is a wire change** — mixed old/new fleets are fine because `decode_auto`
   sniffs, but confirm no consumer hard-assumes JSON (grep direct `serde_json::from_slice`).
3. **R5 dynamic-key declarations** — per-`alert_key`/per-device publishers must be cached and
   (for unbounded key spaces) undeclared on retire, or the registry grows unbounded. Alert
   keys are bounded; audit any per-request key before declaring it.
4. **GUI low-bandwidth mode UX** — dropping `history()` changes reconnect behavior (back-fill
   from store, not network); confirm store retention covers the expected reconnect gap.
5. **R9 media framework work** — needs a `raw_media_publisher()` (opaque bytes + encoding +
   attachment + QoS) on `Publisher`, the parallax receive pipeline, and iced `image` +
   decoder deps in the frontend; keyframe-loss handling (matching-listener force-keyframe) is
   the correctness crux; implicit SHM only helps co-located peers.
6. **R7 is a keyspace/GUI contract change** — needs the logs view reworked to seed from a
   queryable; larger than R1–R4.
7. Should low-bandwidth behavior be a **config profile** (`link_profile: "constrained"`
   flipping CBOR + QoS + no-history + scoped subs together) rather than scattered flags?
   Recommended — one switch for field deployments.

## Sources
- Zenoh QoS (congestion control incl. block/drop/block-first, 7 priorities, express,
  reliability): [zenoh::qos](https://docs.rs/zenoh/latest/zenoh/qos/index.html) ·
  [Reliability & congestion control](https://zenoh.io/blog/2021-06-14-zenoh-reliability/) ·
  [Config (per-keyexpr QoS overrides, since 1.1)](https://docs.rs/zenoh/latest/zenoh/config/struct.Config.html)
- Declare a Publisher to optimize routing (numeric-id keyexpr in routing tables; declaring
  pub/sub/queryable signals repeated intent):
  [Publisher](https://docs.rs/zenoh/latest/zenoh/pubsub/struct.Publisher.html) ·
  [Session management](https://zenoh-cpp.readthedocs.io/en/stable/session.html)
- AdvancedPublisher/Subscriber (cache, heartbeat, miss-detection, recovery):
  [AdvancedPublisher](https://docs.rs/zenoh-ext/latest/zenoh_ext/struct.AdvancedPublisher.html) ·
  [zenoh-ext](https://crates.io/crates/zenoh-ext)
- Video over zenoh (one encoded frame per sample + metadata attachment; QoS knobs; whole-IDR
  fragmentation; implicit SHM): [gst-plugin-zenoh](https://crates.io/crates/gst-plugin-zenoh/0.2.0) ·
  parallax report `zensight-parallax-sensor.md` (§2, §4) · payload as opaque `ZBytes`:
  [zenoh-cpp commons](https://zenoh-cpp.readthedocs.io/en/stable/commons.html)
- Precedent for low-bandwidth tuning:
  [Zenoh in Industrial IoT (perf study)](https://www.sciencedirect.com/science/article/pii/S1570870525000320) ·
  [PX4 rmw_zenoh QoS](https://docs.px4.io/main/en/middleware/zenoh)
