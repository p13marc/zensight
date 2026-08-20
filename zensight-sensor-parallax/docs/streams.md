# Streams, tiers, and pipelines

## The model

One **catalogue entry** (= one `<stream>` key chunk) can be open in several
**profiles** at once, each an independent parallax pipeline with its own
publisher, egress task, and refcount:

| Profile | Key | Encoding | Purpose |
|---------|-----|----------|---------|
| video tier | `zensight/v1/<origin>/@media/parallax/<stream>/video/h264/<tier>` | `video/h264` | one bandwidth tier of the live view |
| `preview` | `zensight/v1/<origin>/@media/parallax/<stream>/preview/jpeg` | `image/jpeg` | low-fps GUI tiles |

**Demand-driven tiered simulcast (#494).** The video plane is a *ladder* of
bandwidth tiers (`low` / `medium` / `high` by default — see
`docs/configuration.md`). Each tier is a **separate** H.264 pipeline published
concurrently on its own `<tier>` key with its own resolution/fps/bitrate. A
viewer subscribes to exactly the one tier its link can take, so two viewers on
different links — one on `…/video/h264/low`, one on `…/video/h264/high` —
render independently and neither perturbs the other's bitrate. That is the
thing a single shared encoder structurally cannot do (one encoder,
last-writer-wins on resolution).

An `open_stream` command with `codec: "h264"` (or the sensor default) and an
optional `tier` opens that video tier; `codec: "mjpeg"` opens the preview.
An open with no `tier` resolves to the sensor's `default_tier`. Each
(stream, tier) and the preview is refcounted per open command and reaped when
its refcount is 0 **and** its publisher has had no matching subscribers for
`idle_timeout_secs` (the matching listener is the crash backstop for GUIs that
die without `close_stream`).

**Viewer keys are exact.** Keyspace v1.3 revoked the
`…/@media/parallax/<stream>/video/h264/*` wildcard licence (RFC 07 §3): the
catalogue advertises which tiers a stream offers (`StreamDescriptor.tiers`), and
a viewer subscribes to exactly one `<tier>` key. A `*` here would pull every
tier at once — the opposite of demand-driven. The per-tier matching listener
counts a subscriber against that tier alone (pinned in `tests/e2e.rs`:
*two viewers on distinct tiers stream independently*). See `docs/KEYSPACE.md`.

Separate pipelines per profile/tier — deliberately **no tee**: the preview must
keep its cadence whether or not any encoder runs, and closing one tier must not
disturb another. The cost (one device open per tier on V4L2) is a
documented limitation, see below; a shared-capture fanout (#508) is the
deferred optimisation.

## Pipeline shapes per source kind

The video path stamps geometry into the pipeline `Metadata` and inserts a
scaler + throttle before the encoder, so the tier's resolution and framerate are
enforced in-pipeline (parallax 0.6: encoders/scaler take **no** dimensions at
construction — geometry travels in the data):

| Source | video tier (h264/`<tier>`) | preview profile (jpeg) |
|--------|----------------------------|------------------------|
| Test | `VideoTestSrc` (Rgb24, live) → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `VideoTestSrc` (preview fps, live) → `VideoScale` → `JpegEncoder`(Rgb) → `AppSink` |
| V4L2 MJPG | `V4l2Src` → `JpegDecoder` → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `AppSink` (MJPG passthrough — no scaler, `preview.max_height` does not apply) |
| V4L2 YUYV | `V4l2Src` → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `VideoScale`(Yuyv) → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |
| RTSP H.264 | `RtspSrc` → `AppSink` (**passthrough** — no re-encode, no scale) | `RtspSrc` → `H264Decoder` → `Throttle` → `VideoScale`(I420) → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |

Notes:

- `VideoScale` is **aspect-preserving and never upscales** — a tier whose
  `max_height` exceeds the source stays at the source height (so the catalogue
  only *offers* a tier the camera can actually feed; a 360-high source offers
  `low` + `high`, not `medium`, since 480 would upscale).
- Framerate limiting to the tier fps uses parallax's `Throttle` element
  (drop-based). The delay-based `RateLimiter` is never used — it would
  backpressure a live source. Test-source previews are built directly at the
  preview fps.
- The H.264 encoder runs in `RateControlMode::Bitrate` with `skip_frames(true)`
  so the tier's `bitrate_kbps` is a real cap (OpenH264 silently overshoots a
  bitrate target with skip-frames off). Note *how* it is held: on input the
  encoder cannot compress further, it does not shrink each frame — it **sheds
  frames**, so the cap moves throughput rather than frame size. `tests/e2e.rs`
  pins this subscriber-side (*the ladder's bitrate cap bites on the wire*): two
  rungs identical but for `bitrate_kbps`, on per-pixel noise, emit ~48 kB and
  ~61 kB per access unit respectively — but 3 and 12 of them in the same window.
  `{stream}/stats/rc_drops` is what the shedding looks like from inside (#510).
- JPEG previews are always flagged `keyframe: true` in `FrameMeta` (every
  JPEG is independently decodable). The preview scaler caps the thumbnail at
  `preview.max_height` on every path that re-encodes; the one exception is
  the V4L2 MJPG passthrough, which forwards the camera's JPEG bytes verbatim
  at whatever size the camera produces. The scaler sits before the RGB
  convert (still YUYV/I420), so convert + JPEG run on the capped size.

## Encoder shaping per tier (#509)

The ladder's `max_height` / `fps` / `bitrate_kbps` say what a tier *delivers* —
they are on the wire, because a viewer picks a tier by them. How the encoder
reaches those numbers is a separate, **sensor-local** set of knobs:
`video.encoder` for the defaults, a per-tier `encoder` block to override any
field. See [`configuration.md`](configuration.md) for the table.

They are deliberately not on the wire. `TierSpec` rides the catalogue inside
`StreamDescriptor`, which is a derived entry in the fleet-wide `SchemaSet` every
producer serves on `@rpc/<producer>/describe` (RFC 08 §7) — so an encoder
implementation detail would become a bus contract, and `zensight-common` would
need serde mirrors of the codec crate's enums to carry it. Nobody subscribes by
entropy coder. The sensor owns the numbers; the wire carries the name.

**Everything ships unset**, and each knob is applied only when set, so an unset
knob is OpenH264's own default *by construction* rather than by a copy of it
that can drift. Two of them are worth reading before you reach for them:

- **`profile`.** The frontend decodes with OpenH264 too, so a profile its
  decoder cannot read would be a self-inflicted outage. All three profiles are
  pinned by a test that runs the encoder output through the GUI's exact decode
  path (`every_profile_the_ladder_can_name_decodes_like_the_gui`); keep that
  gate green before shipping a default.
- **`max_slice_len`** caps each NAL near the path MTU. The usual reason to want
  that is fragmentation: a large keyframe NAL split across IP fragments loses
  the whole IDR when one fragment drops, whereas MTU-sized slices lose one
  slice. **That reasoning does not apply to this egress.** The media plane
  publishes one *whole access unit* per Zenoh sample at `QosClass::LiveVideo`
  (best-effort, drop-on-congestion), so a lost sample costs the entire AU
  whether it was one slice or twenty — there is no RTP payloader in the path.
  What slicing does cost today is a slice header per ~1200 bytes on whichever
  tier has the tightest budget, plus OpenH264's `SM_SIZELIMITED_SLICE`
  threading constraint. It is wired and tested so it is *ready*; turn it on when
  something downstream packetises (an RTP/WebRTC gateway), or when a decoder
  doing slice-level concealment is on the other end.

`threads` and `sps_pps_strategy` are deliberately not exposed: parallax
auto-detects a thread count and a per-tier thread budget needs a host-wide
story, while OpenH264 writes the parameter sets into every IDR under every
strategy (the strategy only renumbers ids) and the egress's
self-contained-keyframe guarantee is derived from the bytes regardless.

## Why there is no live re-tune command

There isn't one, and that is the design — not an omission.

Per-viewer quality is expressed by **which `<tier>` key you subscribe to**
(#494), and redefining what a tier *means* is config-only (#513). `StreamControl`
carries `open_stream` / `close_stream` / `request_keyframe` and nothing else;
there is no `SetVideoParams`, no `tiers/set`, and no registry entry for either.
To change the ladder, edit `configs/parallax.json5` and restart the sensor.

The elements *do* expose live control handles, and `PipelineControls` clones
every one of them before the executor starts — that mechanism is real and
load-bearing, because it is the only way to reach an element after
`Executor::start()` moves it into its task, and it is how keyframe forcing works
at all (below). But the session actor drives **only the keyframe handle**. The
bitrate, scaler, throttle and preview-quality handles are cloned and unused.
They are kept because the clone must happen at construction or not at all, so a
future retune path cannot be added later without them.

Making one of them reachable is not a small change: it needs a new
`StreamControl` variant, a registry and schema entry for it, and a rate-limit
story — a bitrate change is seamless (OpenH264 `SetOption`), but GOP and
resolution rebuild the inner encoder and emit a fresh IDR, so a slider wired
straight through would re-key the stream on every pixel of travel. None of that
exists today.

*(This section previously described those knobs as working. They never did;
#504 corrected it — the same class of defect as #513's phantom `tiers/set` and
#479's phantom payload type.)*

## Keyframe control

The H.264 encoder's `KeyframeHandle` is cloned **before** the encoder is
consumed by the pipeline (it is unreachable once running). A keyframe is
forced when:

- the tier publisher's matching listener sees a viewer appear (rising edge), or
- a `request_keyframe` command arrives (explicit recovery). The command carries
  the `tier` to re-key; the sensor forces an IDR on that tier's encoder alone.

RTSP video is passthrough — the sensor cannot force a remote camera's IDR, so
`request_keyframe` logs and no-ops; viewers instead gate on the in-band IDRs
(`FrameMeta.keyframe`, GOP-rate).

### Self-contained keyframes (#435)

The video egress guarantees, at the byte level, that every access unit it
publishes with `keyframe: true` is a **self-contained decoder entry point**:

- the flag itself is derived from the bitstream (an IDR NAL is present), not
  from upstream pipeline metadata — raw sources flag every uncompressed frame
  as a sync point, and parallax < 0.1.3 leaked that through the encoder,
  sending fresh decoders into an unrecoverable `dsNoParamSets` loop;
- the egress caches the last SPS/PPS NAL units it has seen for the stream and
  prepends them to any keyframe AU that arrived without its own (relevant for
  RTSP passthrough cameras that announce parameter sets only out-of-band in
  the SDP; the OpenH264 encoder paths already inline SPS/PPS with every IDR).

An RTSP keyframe that arrives before *any* in-band parameter sets have been
seen is published as-is — there is nothing to prepend yet.

## Frame metadata

Every media sample carries a CBOR `FrameMeta` attachment
(`zensight-common::stream::FrameMeta`): keyframe flag, optional
pts/dts/duration (ns), per-stream sequence, width, height. On the h264 video
path width/height are the **tier's encoded (post-scale) dimensions**, so a low
tier reports 240-high frames while the high tier reports native height on the
same source. Sequence gaps mean dropped frames (LiveVideo QoS is best-effort by
design).

## Teardown

`close_stream` (with the tile's `codec` + `tier`) decrements that profile's
refcount; at 0 the profile enters the idle countdown (unless a viewer is still
subscribed). The idle reaper stops the pipeline, aborts the egress and
matching-listener tasks, and undeclares the publisher. A viewer *edge* (appear /
leave) also republishes the per-stream `StreamStatus` so the state plane
reflects each tier's live viewer count. Closing the last profile marks the
stream inactive in the catalogue and publishes a `StreamStatus{open: false}`
transition on `state/parallax/stream/<stream>` (the doc is only tombstoned when
the stream leaves the config, never on close).

> A codec-less `close_stream` resolves to the sensor's **default video tier**
> (see `resolve_profile`) — so a preview or a non-default tier must name its own
> `codec`/`tier`, or the wrong refcount is decremented. The GUI's per-tile
> `close_control` does exactly this.

A profile that was opened but never gets (or loses) its viewer is reaped by
the same countdown even if its refcount is non-zero — the matching listener
is the crash backstop for GUIs that die without `close_stream`, and an opener
that never subscribes is a zombie.

**Failed opens** publish a definitive `StreamStatus{open: false}` transition
(the GUI flags a still-waiting tile with it), record a device failure, and
drop the stream's stats entry — a leaked entry would publish phantom
zero-valued stats forever. All open-failure paths (pipeline build, media
publisher declare, matching listener declare, pipeline start, RTSP connect)
funnel through the same cleanup exit.

**Dead-profile reopen**: `open_stream` for a profile whose egress already
ended (its `EgressEnded` still queued behind the command) tears the dead
pipeline down and builds a fresh one instead of refcounting a corpse; the
queued stale `EgressEnded` is recognized by its epoch stamp and ignored, so
it cannot kill the replacement.

**Stopping a live pipeline** (parallax gotcha): the unified executor
runs source loops on blocking threads and ignores downstream channel closure,
so `PipelineHandle::abort()` alone cannot end a live source — the blocking
task would run forever. Every source the sensor builds is wrapped in a
`StoppableSource` whose `StopHandle` flips the next source pull to EOS; the
whole pipeline then unwinds cleanly within one frame period. Teardown always
triggers the stop handle first.

## Stats, health, alerts

Per-stream stats ride ordinary telemetry under
`zensight/v1/<origin>/telemetry/parallax/<stream>/stats/<metric>` every
`stats_interval_secs`, **aggregated over the stream's open profiles**:

| Metric | Kind | Meaning |
|--------|------|---------|
| `fps` | gauge | frames published per second (all open tiers + preview combined) |
| `kbps` | gauge | total media bandwidth published for the stream |
| `drops` | counter | frames lost between encoder and egress (video-tier sequence gaps; intentional preview throttling is never counted) |
| `rc_drops` | counter | frames the encoder's rate control swallowed to hold the tier's `bitrate_kbps` (omitted for streams with no rate-controlled encoder) |
| `viewers` | gauge | open profiles with matching subscribers (0 .. tiers + 1) |
| `encode_ms` | gauge | average wall time per encoder `process()` call (omitted when no encoder ran) |

Three things about that table are easy to get wrong, so they are written down
here (#510):

- **`drops` and `rc_drops` cannot overlap.** The H.264 element numbers its
  output from its own *emitted*-frame count, and a frame the rate controller
  swallows produces no buffer at all — so it leaves no sequence gap for the
  egress task to notice. `drops` is therefore always the `AppSink` shedding
  under a slow consumer; `rc_drops` is always the bitrate cap biting. A tier
  whose `rc_drops` climbs is being held to its `bitrate_kbps`, which is the
  ladder working, not a fault — but it is also the number to read before
  concluding a tier "looks soft".
- **`rc_drops` is omitted, not zeroed**, when a stream has no rate-controlled
  encoder — an RTSP passthrough (no encoder at all) or a preview-only stream
  (JPEG has no rate control). A `0` there would read as "the cap is not
  biting" when the truth is "there is no cap".
- **`fps` and `kbps` stay egress-sourced on purpose.** They count what actually
  crossed Zenoh — the SPS/PPS the egress injects into a bare keyframe included,
  and frames the sink shed excluded — which is the bandwidth an operator is
  paying for. The encoder's own `bytes_encoded` over-reports on both counts, and
  an RTSP passthrough has no encoder to ask. For the same reason `encode_ms`
  remains a mean over whole `process()` calls rather than the encoder handle's
  `last_encode_ns`, which is a single sample of the inner encode: the
  `encoder_overrun` rule compares a mean to a per-frame budget.

For per-tier **applied** resolution/viewers, read the `StreamStatus` doc's
`tiers[]` — that is what the GUI's per-tile bandwidth readout shows. Note that
`TierApplied`'s `fps` and `bitrate_kbps` are the tier's configured *targets*
read back, not measurements; only `width`/`height` come from the built
pipeline.

`streams/advertised` (catalogue size) is published every tick regardless of
open streams, so a parallax host appears on the dashboard before anything is
opened.

Health: each successful profile open records a device success for the
stream; pipeline build failures and egress errors record failures (3
consecutive → the stream's device flips Offline).

Alert rules on `state/parallax/alert/*` (auto-resolve on recovery):

- `camera_disappeared` — an advertised V4L2 device vanished from periodic
  re-enumeration.
- `rtsp_connect_failed` — an `open_stream` could not reach the RTSP camera.
- `encoder_overrun` — average `encode_ms` above the strictest open tier's
  per-frame budget (1000 / fps).

## Limitations

- **V4L2 multi-open**: each video tier and the preview open the device
  independently, so watching two tiers of one camera + its preview is three
  `V4l2Src` opens; most UVC cameras reject the second open (`EBUSY`), and the
  losing pipeline fails with an error status. On such cameras, watch one tier
  at a time, or use an RTSP/test source. A shared-capture fanout that opens the
  device once and scales to every tier (#508) is the planned fix; the current
  design trades that for genuinely independent per-tier pipelines.
- **RTSP is H.264-only and passthrough**: bitrate/GOP/resolution config does
  **not** apply and `max_height` cannot rescale a passthrough tier — the tiers
  a passthrough stream "offers" all carry the camera's own encoding. Each open
  profile makes its own RTSP connection. The connect runs in its own task,
  **never inside the session actor**: the profile slot is reserved as *pending*
  (refcounting opens/closes that race the connect; the stream reads `open` in
  status/catalogue meanwhile), so an unreachable camera (bounded by a 5 s
  timeout) stalls no stream commands, no `@rpc/parallax/streams` replies, and no
  `state/parallax/stream/<stream>` doc updates. If the SDP carries no video
  dimensions, `FrameMeta.width/height` are `0` (= unknown) and the JPEG preview
  cannot be opened (the advertised `FrameMeta`/catalogue size would be a lie).
- **`max_height` is now real** for encoder-backed sources (test patterns and
  V4L2): the inserted `VideoScale` caps the encoded height aspect-preserving
  (never upscaling) per tier. The cap is applied when the tier's pipeline is
  **built** — the scaler's `ScaleControl` is cloned and could retarget it live,
  but nothing does (see "Why there is no live re-tune command"). It is inert
  altogether on RTSP passthrough, where there is no encoder or scaler to drive.
