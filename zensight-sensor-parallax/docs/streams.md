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
keep its cadence whether or not any encoder runs, and closing or re-tuning one
tier must not disturb another. The cost (one device open per tier on V4L2) is a
documented limitation, see below; a shared-capture fanout (#508) is the
deferred optimisation.

## Pipeline shapes per source kind

The video path stamps geometry into the pipeline `Metadata` and inserts a
scaler + throttle before the encoder, so the tier's resolution and framerate are
enforced in-pipeline (parallax 0.3.0: encoders/scaler take **no** dimensions at
construction — geometry travels in the data):

| Source | video tier (h264/`<tier>`) | preview profile (jpeg) |
|--------|----------------------------|------------------------|
| Test | `VideoTestSrc` (Rgb24, live) → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `VideoTestSrc` (preview fps, live) → `VideoScale` → `JpegEncoder`(Rgb) → `AppSink` |
| V4L2 MJPG | `V4l2Src` → `JpegDecoder` → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `AppSink` (MJPG passthrough) |
| V4L2 YUYV | `V4l2Src` → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |
| RTSP H.264 | `RtspSrc` → `AppSink` (**passthrough** — no re-encode, no scale) | `RtspSrc` → `H264Decoder` → `Throttle` → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |

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
  bitrate target with skip-frames off).
- JPEG previews are always flagged `keyframe: true` in `FrameMeta` (every
  JPEG is independently decodable). The preview scaler caps the thumbnail at
  `preview.max_height`.

## Live control (no teardown) — #496

Each tier's pipeline exposes control handles (`PipelineControls`), cloned from
their elements **before** the executor starts:

- **bitrate** — seamless: routed through OpenH264 `SetOption`, no encoder
  rebuild, no forced IDR.
- **GOP / rate-control mode / resolution (scaler target)** — these rebuild the
  inner encoder and start a fresh IDR (a clean decoder entry point). Rate-limit
  these knobs; bitrate can change every frame.
- **framerate** — the `Throttle`'s target rate.
- **preview quality / preview fps** — the JPEG encoder's quality and the preview
  `Throttle`.

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
| `viewers` | gauge | open profiles with matching subscribers (0 .. tiers + 1) |
| `encode_ms` | gauge | average wall time per encoder `process()` call (omitted when no encoder ran) |

For per-tier **applied** resolution/bitrate/viewers, read the `StreamStatus`
doc's `tiers[]` (each `TierStatus` carries the params the encoder was actually
built with) — that is what the GUI's per-tile bandwidth readout shows.

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
  cannot be opened (the encoder needs a size).
- **`max_height` is now real** for encoder-backed sources (test patterns and
  V4L2): the inserted `VideoScale` caps the encoded height aspect-preserving
  (never upscaling) per tier, and the cap is a live `ScaleControl` (a resolution
  change rebuilds the encoder → clean IDR). It is inert only on RTSP
  passthrough, where there is no encoder to drive.
