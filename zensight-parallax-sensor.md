# ZenSight "parallax" Video Sensor — Gap Analysis & Roadmap

*Report date: 2026-07-04. Covers parallax @ master (6a61849), zensight @ master, zenoh 1.9.*

## Goal

A new `zensight-sensor-parallax` crate (living in the zensight repo, per decision) that:

1. **Autodetects** available video streams (local cameras via V4L2/libcamera, screen capture, RTSP — static config + optional ONVIF discovery)
2. **Advertises** them on the zensight zenoh keyspace
3. **Opens streams on request** (control plane), builds a parallax pipeline per stream
4. **Decodes/encodes** (H.264 sw, MJPEG preview, AV1; hardware encode later)
5. **Ships media over zenoh**, consumable by the zensight Iced frontend and by other parallax pipelines

## TL;DR

The building blocks are in much better shape than expected. **Parallax already has**: zenoh
elements (`ZenohSink`/`ZenohSrc`/`ZenohQueryable`, feature `zenoh`), full device enumeration
(V4L2 + libcamera + PipeWire + ALSA, with format/resolution introspection), RTP payloaders
decoupled from any socket, an RTSP client with codec introspection, per-pipeline
start/abort lifecycle, and `force_keyframe()` on the H.264 encoder. The pipeline
`V4l2Src → H264Encoder → RtpH264Pay → ZenohSink` compiles conceptually **today**.

The real work splits into three buckets:

1. **Parallax gaps** (~6 items): ZenohSink loses buffer metadata (PTS!) across the hop;
   no JPEG *encoder* (preview path); no udev hotplug; no hardware encode at all; no ONVIF;
   no `RtpOpusPay`.
2. **ZenSight gaps** (~5 items): `Protocol` is a closed enum (needs a `Parallax` variant in
   `zensight-common`); the telemetry bus (cached AdvancedPublisher) is wrong for video and
   the framework doesn't expose QoS; the Iced frontend has **zero** image/video display
   capability today (`image` feature not even enabled, no decoder dependency); systemd
   sandbox (`DynamicUser` + `ProtectSystem=strict`) blocks camera access.
3. **The sensor itself**: the daemon/session-manager layer (map of stream-id →
   `PipelineHandle`, driven by zenoh queryables) exists in neither project and is the core
   new code.

One **design recommendation** to review before implementation: ship **encoded access units
+ metadata attachment** as the native zenoh format, and keep RTP packetization as an
optional interop mode — see "RTP-in-zenoh vs. access units" below.

---

## 1. What already exists

### 1.1 Parallax (surprisingly complete)

| Need | Status | Where |
|---|---|---|
| Enumerate cameras + formats | ✅ `enumerate_video_devices()` merges libcamera + V4L2; `V4l2Src::query_supported_formats()` gives fourcc + resolutions | `src/elements/device/mod.rs`, `v4l2.rs:399` |
| Enumerate audio | ✅ PipeWire + ALSA merged | `src/elements/device/` |
| Screen capture | ⚠️ XDG portal (interactive consent; `restore_token` helps, headless awkward) | `screen_capture.rs` |
| RTSP ingest | ✅ `RtspSrc` via retina: TCP-interleaved/UDP, digest/basic auth, per-stream codec from SDP | `src/elements/rtp/rtsp.rs` |
| RTP packetization | ✅ Pay/depay for H264/H265/VP8/VP9 are plain `Element`s (Buffer→Buffer), fully decoupled from sockets; MTU-configurable | `src/elements/rtp/rtp_codecs.rs` |
| RTCP / jitter buffer | ✅ `RtcpHandler`, `RtpJitterBuffer` (transport-independent logic) | `rtcp.rs`, `jitter_buffer.rs` |
| Zenoh pub/sub | ✅ `ZenohSink`/`ZenohSrc` with congestion-control + priority knobs, `Arc<Session>` sharing | `src/elements/network/zenoh.rs` (feature `zenoh`, dep `zenoh = "1.0"`) |
| Zenoh RPC | ✅ `ZenohQueryable::recv_query()` / `ZenohQuery::reply()` / `ZenohQuerier::get()` — a ready-made control plane | same file |
| Encoders (sw) | ✅ H.264 (openh264, constrained-baseline, `force_keyframe()` at `h264.rs:222`), AV1 (rav1e), Opus, AAC | `src/elements/codec/` |
| Decoders (sw) | ✅ H.264, AV1 (dav1d), JPEG, symphonia audio; Vulkan H.264 decode scaffold | same + `src/gpu/` |
| Per-stream lifecycle | ✅ Independent `Pipeline` + `PipelineHandle::abort()/wait()/subscribe()`; N pipelines per process compose fine | `src/pipeline/unified_executor.rs` |
| App bridges | ✅ `AppSrc`/`AppSink` handles (push/pull, timeouts, bounded queues) | `src/elements/app/` |
| Codec probing | ✅ typefind registry; RTSP codec comes free from SDP | `src/pipeline/typefind.rs` |

### 1.2 ZenSight (solid framework, zero media)

- **Sensor pattern**: no `Sensor` trait — you implement `SensorConfig` (JSON5) and compose
  `SensorRunner` (zenoh connect, health ticker on `@/health`, liveliness `@/alive`, status,
  SIGINT/SIGTERM shutdown). Your collection loop is your own tokio task via `runner.spawn()`.
  Copy the shape of `zensight-sensor-sysinfo`.
- **Control plane conventions already exist and fit "open stream on request"**:
  `@/commands/<topic>` (subscriber), `@/status/<topic>` (queryable), `@/query/<topic>`
  (queryable with selector params), `Command<T>` envelope with correlation id
  (`zensight-common/src/command.rs`). Zenoh matches `@`-chunks verbatim, so `zensight/**`
  subscribers never see `@` keys — important below.
- **Telemetry**: `TelemetryPoint` (JSON/CBOR) through per-key cached zenoh-ext
  `AdvancedPublisher` (10-sample cache, heartbeat). Right for stream *stats*, wrong for frames.
- **Large binary**: only `zenoh-blob` — pull-based resumable file download. Not a live stream.
- **Frontend**: Iced 0.14 without the `image` feature; SVG + canvas only; no decoder deps
  anywhere in the workspace. Video display is greenfield work.

### 1.3 Ecosystem findings (web research)

- **Nobody ships RTP packets over zenoh.** The canonical demos and production bridges all
  send whole encoded frames per zenoh sample: zcam (JPEG per put; the Rust version publishes
  raw frames in zenoh SHM with an rkyv `FrameMeta` attachment), **your own gst-plugin-zenoh**
  (one GStreamer buffer per message, caps + PTS/DTS metadata preserved, CC/priority/express
  exposed), zenoh-plugin-ros2dds / rmw_zenoh (CDR `sensor_msgs` or `ffmpeg_image_transport`
  H.264 packets), EdgeFirst (CDR over GStreamer elements).
- **Modern precedent agrees**: Media-over-QUIC dropped RTP for a thin frame container (LOC);
  RTP-over-QUIC (draft-ietf-avtcore-rtp-over-quic) spends most of its text disabling RTP
  machinery the transport already provides. RTP survives at *edges* (RTSP-in via retina,
  RTP/UDP-out gateways).
- **Zenoh 1.x gives you**: per-publisher `CongestionControl::{Block,Drop}`, 7 priorities,
  `express` mode, automatic fragmentation (publish a whole IDR as one sample), per-keyexpr
  QoS overrides in config (since 1.1), implicit SHM promotion of large payloads between local
  peers (since 1.6 — free zero-copy to a co-located frontend), AdvancedPublisher caching for
  late joiners, liveliness tokens for presence, `Querier`/queryable RPC (rmw_zenoh pattern:
  attachment carries seq + client id).
- **Discovery crates**: `v4l` 0.14 (enumeration; no hotplug) + `udev` 0.9 (`video4linux`
  monitor) is the standard pair; libcamera-rs 0.7 has `subscribe_hotplug_events()`;
  `lumeohq/onvif-rs` is the most complete ONVIF/WS-Discovery implementation but is
  **git-only, unreleased**; `mdns-sd` for `_rtsp._tcp` as a cheap secondary probe.

---

## 2. Design decision to review: RTP-in-zenoh vs. access units

You asked for "send them on RTP through zenoh". That works mechanically today
(`RtpH264Pay → ZenohSink`), and it has one real advantage: **RTP headers carry
timestamps/seq/marker, which sidesteps the ZenohSink metadata-loss bug** (§3.1-P1).

But for the two consumers you chose (zensight frontend, parallax pipelines), RTP inside
zenoh double-pays: zenoh already provides framing, fragmentation (no MTU constraint → no
FU-A fragmentation needed), sequencing, keying, and QoS. Every ecosystem precedent —
including your gst-plugin-zenoh — sends **one encoded access unit per zenoh sample** with
metadata in an attachment, and reconstructs RTP only at a gateway to RTP-native peers.

**Recommendation**: make the native wire format *one AU per sample + rkyv-serialized
`parallax::Metadata` attachment* (mirroring zcam-rust's `FrameMeta` and gst-plugin-zenoh's
buffer-meta), QoS `BestEffort + Drop + Priority::InteractiveHigh` for live video. Keep an
`rtp = true` per-stream config switch that inserts `RtpH264Pay` before the sink for future
interop (e.g. a thin zenoh→UDP bridge so ffplay/VLC work). Both modes share everything
upstream of the sink. If you disagree, everything in this report still holds — only the
attachment format and the frontend depacketization step change.

---

## 3. Gap analysis

### 3.1 Parallax gaps

| # | Gap | Impact | Effort | Notes |
|---|---|---|---|---|
| **P1** | **`ZenohSink` drops buffer metadata.** Publishes payload only; `ZenohSrc` fabricates `Metadata::from_sequence(seq)` — PTS/DTS/flags/format are lost (`src/elements/network/zenoh.rs:236`). | Blocker for AU mode; degrades RTP mode (loses `BufferFlags`) | S–M | Serialize `Metadata` with rkyv into a zenoh **attachment**; deserialize in `ZenohSrc`. Also check/add `express` support while in there. |
| **P2** | **No JPEG encoder** (only `JpegDecoder`; `PngEncoder` exists). Needed for the MJPEG preview path when the camera doesn't emit MJPG natively. | Blocks preview for non-MJPG sources | S | Many cameras emit MJPG fourcc natively — V4L2 passthrough covers those with zero code. Add `JpegEncoder` (e.g. `jpeg-encoder` or `turbojpeg`) for the rest. |
| **P3** | **No hotplug.** Enumeration is scan-once; nothing watches `/dev/video*` appear/disappear. | Autodetect goes stale | S–M | `udev` crate monitor on subsystem `video4linux` (rustix ethos fits); libcamera-rs hotplug events as alternative. Emit add/remove events the sensor can forward as liveliness tokens. |
| **P4** | **No hardware encode.** `HwVideoEncoder` trait + `HwEncoderElement` wrapper exist but zero implementations; Vulkan module is decode-only; no VAAPI, no V4L2 M2M. openh264 is constrained-baseline CPU only. | Multiple HD streams on small boxes not viable | L | Two credible paths: **(a) V4L2 M2M** `H264` encoder element (kernel API, fits rustix/v4l stack, covers RPi/i.MX/Rockchip); **(b) VAAPI** via `cros-libva` (Intel/AMD desktops). Recommend (a) first — same ioctl family as existing `V4l2Src`. Vulkan encode is a bigger lift on an experimental scaffold. |
| **P5** | **No ONVIF/WS-Discovery, no mDNS.** | RTSP autodetect | M | `onvif-rs` (git-only — pin a rev) behind a config flag, per your decision; `mdns-sd` `_rtsp._tcp` browse as a cheap complement. Could live in the sensor crate instead of parallax — recommend sensor crate, since it's discovery policy, not pipeline machinery. |
| **P6** | Minor: **no `RtpOpusPay`** (depay only); no VP8/VP9/H.265 encoders (pay/depay exist but nothing to feed them); AV1 has no RTP payloader. | Only if audio/RTP-interop matter early | S (Opus pay) | Defer unless audio streams are in scope for v1. |
| **P7** | **No keyframe-request plumbing.** `force_keyframe()` exists on the encoder, but there's no standard upstream event (e.g. `Event::Custom`) routed from a sink/control surface to the encoder. | Late joiners wait up to a GOP for a decodable frame | S | Wire a custom upstream event, or let the sensor daemon call `get_element_mut::<H264Encoder>()` between... simpler: sensor holds a handle and calls `force_keyframe()` when the control plane sees a new subscriber (zenoh **matching listener** tells you when subscriber count changes). |
| **P8** | Housekeeping: `zenoh = "1.0"` in Cargo.toml — align explicitly on 1.9 to match zensight; verify feature interplay `zenoh` + `rtp` + `h264` + `v4l2` compiles together (feature-gated code isn't built by default). | Build hygiene | XS | |

Not gaps for this project (explicitly fine): no RTSP *server* (egress is zenoh), no live
element hot-swap (use one pipeline per stream, rebuild on change), no SRT.

### 3.2 ZenSight gaps

| # | Gap | Effort | Notes |
|---|---|---|---|
| **Z1** | `Protocol` is a **closed enum** hard-coded in ≥4 places. Add `Protocol::Parallax` in `zensight-common/src/telemetry.rs` + `as_str`/`from_str`/`display_name` + both matches in `keyexpr.rs`. | XS | Cross-cutting but mechanical. |
| **Z2** | **Framework exposes no QoS.** `Publisher` hard-wires cached AdvancedPublishers (telemetry) and plain puts (control). Video needs `CongestionControl::Drop` + priority + (maybe) express on a raw publisher. | S | Use `runner.session()` and declare publishers directly in the sensor — no core change needed for v1; consider upstreaming a `Publisher::raw_media_publisher()` later. |
| **Z3** | **Frontend cannot display video.** Iced `image` feature off; no decoder dep; no parallax view. | M–L | Path of least resistance: frontend depends on **parallax** (features `zenoh, h264`) and runs a receive pipeline `ZenohSrc → [RtpH264Depay →] H264Decoder → AppSink`, pulling RGBA frames into `iced::widget::image::Handle::from_rgba` via a subscription. Add `view/specialized/parallax.rs`. MJPEG preview needs only a JPEG decode (`zune-jpeg`/`image`) — ship that first. |
| **Z4** | **Systemd sandbox** (`DynamicUser`, `ProtectSystem=strict`) blocks `/dev/video*`, portals, and LAN discovery. | XS | Unit needs `SupplementaryGroups=video`, `DeviceAllow=char-video4linux`, network access for RTSP/ONVIF; document divergence in `packaging/systemd/PRIVILEGES` notes. |
| **Z5** | Docs: KEYSPACE.md + SENSORS.md must gain the media-plane region and the new sensor row; the `zensight/**` wildcard-vs-`@` rule is the load-bearing detail (below). | XS | |

### 3.3 New code: the sensor daemon itself

The orchestration layer exists in neither repo (~the core deliverable):

- **Catalog**: periodic + hotplug-driven device scan (P3) merged with static config streams
  (RTSP URLs) and ONVIF/mDNS discoveries → a `StreamDescriptor` table (id, kind, formats,
  state).
- **Session manager**: `HashMap<StreamId, StreamSession { PipelineHandle, publisher, refcount }>`.
  Open request → build pipeline (source per kind → decode if needed → encode per requested
  profile → optional RtpPay → ZenohSink with shared `Arc<Session>`), `executor.start()`,
  store handle. Close / zero matching subscribers → `handle.abort()`. Use zenoh **matching
  listeners** to stop encoding when nobody watches (and to trigger `force_keyframe()` on
  new watchers, P7).
- **Multi-profile**: a `tee` after decode feeding both a preview (MJPEG, low fps) and a full
  branch is programmatic-API territory (parse grammar has no tee branching — fine, don't
  use `Pipeline::parse` here).
- **Telemetry**: per-stream `TelemetryPoint`s (fps, bitrate, dropped, subscriber count,
  encode ms) on the normal cached bus — this is the part that makes it a *zensight* sensor
  rather than a standalone daemon, and it lights up existing GUI charts for free.
- **Health/alerts**: `SensorHealth` device tracking maps 1:1 to cameras (3 consecutive
  failures → Offline); `AlertReporter` for "camera disappeared", "RTSP auth failed",
  "encoder overrun".

---

## 4. Proposed keyspace (for review)

```
zensight/parallax/<host>/@/alive                       # liveliness (from SensorRunner)
zensight/parallax/<host>/@/health, @/errors, @/status  # standard, free from sensor-core
zensight/parallax/<host>/@/devices/<stream>/alive      # per-stream liveliness = "advertised & openable"

zensight/parallax/<host>/@/query/streams               # queryable: list StreamDescriptors (JSON)
zensight/parallax/<host>/@/commands/stream             # Command<OpenStream|CloseStream|RequestKeyframe>
zensight/parallax/<host>/@/status/streams              # queryable: current sessions + profiles

zensight/parallax/<host>/<stream>/stats/<metric>       # TelemetryPoint (fps, kbps, drops, viewers)

zensight/parallax/<host>/@media/<stream>/video/<codec>/<profile>   # MEDIA PLANE (raw puts,
zensight/parallax/<host>/@media/<stream>/preview/jpeg              #  BestEffort+Drop, attachment=Metadata)
```

The `@media` chunk is deliberate: zenoh matches `@`-prefixed chunks **verbatim only**, so
every existing `zensight/**` telemetry subscriber (GUI, exporters, storages) is structurally
incapable of accidentally ingesting the video firehose. Consumers subscribe to the explicit
media key. (Alternative: a separate `zensight-media/…` root — works too, but `@media` keeps
everything under one host subtree and reuses an established zensight convention.)

Open/close flow (rmw_zenoh-style): frontend `session.get("…/@/commands/stream", payload
= Command{ id, OpenStream{ stream, codec, max_height, rtp: bool } })` → sensor opens or
refcounts the session, replies with the concrete media key + negotiated format → frontend
subscribes → matching listener keeps the encoder honest; idle timeout or explicit
CloseStream tears down.

---

## 5. Suggested roadmap

**Phase 0 — prove the wire (days).** Parallax P1 (metadata attachment) + P8. A
`zensight-sensor-parallax` skeleton: Z1, config, `SensorRunner`, `@/query/streams` from
`enumerate_video_devices()`, static RTSP list. MJPEG-passthrough preview (native-MJPG
cameras) published on `@media/...`, consumed by a throwaway parallax CLI viewer. No GUI yet.

**Phase 1 — H.264 end-to-end + control plane (1–2 weeks).** Session manager, open/close
commands, `V4l2Src|RtspSrc → H264Encoder → ZenohSink` (+ optional RtpPay mode), per-stream
telemetry + liveliness, keyframe-on-subscribe (P7 via matching listener), P2 (JPEG encoder)
for universal preview, systemd unit (Z4). Consumer: parallax receive pipeline.

**Phase 2 — frontend (1–2 weeks, parallelizable with Phase 1).** Z3: iced `image` feature,
JPEG preview tiles first (cheap, no codec), then H.264 via parallax receive pipeline +
`AppSink`; `view/specialized/parallax.rs`; docs (Z5).

**Phase 3 — discovery & robustness.** P3 (udev hotplug), P5 (ONVIF behind config flag +
mDNS), reconnect/backoff for RTSP, idle-timeout teardown, AV1 profile option.

**Phase 4 — hardware encode (largest single item).** P4: V4L2 M2M encoder element first,
`cros-libva` VAAPI second; both slot behind the existing `HwVideoEncoder` trait /
`HwEncoderElement` so the sensor's pipeline builder just swaps the encoder node.

## 6. Open questions

1. **AU-native vs RTP-native** (§2) — needs your call; my recommendation is AU-native with
   RTP as an opt-in interop mode.
2. **Audio in v1?** Determines whether `RtpOpusPay` (P6) and ALSA/PipeWire sources enter
   scope now. Recommend: video-only v1.
3. **Screen capture in a systemd sensor**: XDG portal wants an interactive session.
   Recommend deferring screen-capture streams to a per-user (`systemd --user`) deployment
   variant, not the hardened system unit.
4. Should the parallax-side fixes (P1–P4, P7) land as a `zenoh`-feature polish PR in
   parallax first? P1 changes the ZenohSrc/Sink wire behavior — nicer to fix before anyone
   depends on the current format.

## 7. Key sources

- zenoh-demos zcam (JPEG puts / SHM raw + rkyv attachment): <https://github.com/eclipse-zenoh/zenoh-demos>
- gst-plugin-zenoh (your GStreamer bridge — buffer-per-message + meta, QoS knobs): <https://github.com/p13marc/gst-plugin-zenoh>
- rmw_zenoh design (service RPC pattern, liveliness graph, QoS mapping): <https://github.com/ros2/rmw_zenoh/blob/rolling/docs/design.md>
- Zenoh 1.1 (Querier, per-keyexpr QoS config): <https://zenoh.io/blog/2024-12-12-zenoh-firesong-1.1.0/> · 1.6 implicit SHM: <https://zenoh.io/blog/2025-10-20-zenoh-imoogi/>
- RTP-over-QUIC draft (what RTP-in-a-message-transport costs): <https://datatracker.ietf.org/doc/draft-ietf-avtcore-rtp-over-quic/> · MoQ/LOC rationale: <https://www.meetecho.com/blog/moq-webrtc/>
- onvif-rs (WS-Discovery, git-only): <https://github.com/lumeohq/onvif-rs> · libcamera-rs hotplug: <https://github.com/lit-robotics/libcamera-rs> · libv4l-rs: <https://github.com/raymanfx/libv4l-rs>
