# Streams, profiles, and pipelines

## The model

One **catalogue entry** (= one `<stream>` key chunk) can be open in up to two
**profiles**, each an independent parallax pipeline with its own publisher,
egress task, and refcount:

| Profile | Key | Encoding | Purpose |
|---------|-----|----------|---------|
| `video` | `zensight/v1/<origin>/@media/parallax/<stream>/video/h264/<profile>` | `video/h264` | full-rate live view |
| `preview` | `zensight/v1/<origin>/@media/parallax/<stream>/preview/jpeg` | `image/jpeg` | low-fps GUI tiles |

An `open_stream` command with `codec: "h264"` (or the sensor default) opens
the video profile; `codec: "mjpeg"` opens the preview profile. Profiles are
refcounted per open command and reaped when the refcount is 0 **and** the
publisher has had no matching subscribers for `idle_timeout_secs` (the
matching listener is the crash backstop for GUIs that die without
`close_stream`).

**Viewer keys**: previews are watched on the exact
`…/@media/parallax/<stream>/preview/jpeg` key; video viewers subscribe with the
profile chunk as a single-chunk wildcard
(`…/@media/parallax/<stream>/video/h264/*`), because `video.profile` is
*sensor* configuration that the catalogue does not carry. The matching listener
fires for wildcard subscribers too (zenoh matching is intersection-based;
pinned in `tests/e2e.rs`). See `docs/KEYSPACE.md`.

Separate pipelines per profile — deliberately **no tee**: the preview must
keep its 2 fps cadence whether or not the encoder runs, and closing one
profile must not disturb the other. The cost (double capture on V4L2) is a
documented limitation, see below.

## Pipeline shapes per source kind

| Source | video profile (h264/`<profile>`) | preview profile (jpeg) |
|--------|----------------------------------|------------------------|
| Test | `VideoTestSrc` (Rgb24, live) → `VideoConvert`(→I420) → `H264Encoder` → `AppSink` | `VideoTestSrc` (preview fps, live) → `JpegEncoder`(Rgb) → `AppSink` |
| V4L2 MJPG | `V4l2Src` → `JpegDecoder` → `VideoConvert`(→I420) → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `AppSink` (MJPG passthrough) |
| V4L2 YUYV | `V4l2Src` → `VideoConvert`(→I420) → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |
| RTSP H.264 | `RtspSrc` → `AppSink` (**passthrough** — no re-encode) | `RtspSrc` → `H264Decoder` → `Throttle` → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |

Notes:

- `VideoTestSrc` produces **Rgb24** (its `PixelFormat` enum has no planar YUV),
  so the H.264 path converts Rgb24→I420 in-pipeline; the JPEG path feeds the
  encoder directly.
- Frame-rate limiting to the preview fps uses parallax's `Throttle` element
  (drop-based). The delay-based `RateLimiter` is never used — it would
  backpressure a live source. Test-source previews don't need either: the
  `VideoTestSrc` is built directly at the preview fps.
- JPEG previews are always flagged `keyframe: true` in `FrameMeta` (every
  JPEG is independently decodable).

## Keyframe control

The H.264 encoder's `KeyframeHandle` is cloned **before** the encoder is
consumed by the pipeline (it is unreachable once running). A keyframe is
forced when:

- the media publisher's matching listener sees a viewer appear (rising edge), or
- a `request_keyframe` command arrives (explicit recovery).

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
pts/dts/duration (ns), per-stream sequence, width, height. Sequence gaps mean
dropped frames (LiveVideo QoS is best-effort by design). On the h264 video
path the keyframe flag is bitstream-derived and keyframes are made
self-contained (see "Self-contained keyframes" above).

## Teardown

`close_stream` decrements the profile refcount; at 0 the profile enters the
idle countdown (unless a viewer is still subscribed). The idle reaper stops
the pipeline, aborts the egress and matching-listener tasks, and undeclares
the publisher. Closing the last profile marks the stream inactive in the
catalogue and publishes a `StreamStatus{open: false}` transition on the
stream's `state/parallax/stream/<stream>` doc (the doc is only tombstoned when
the stream leaves the config, never on close).

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

**Stopping a live pipeline** (parallax 0.1.1 gotcha): the unified executor
runs source loops on blocking threads and ignores downstream channel closure,
so `PipelineHandle::abort()` alone cannot end a live source — the blocking
task would run forever. Every source the sensor builds is wrapped in a
`StoppableSource` whose `StopHandle` flips the next `produce()` to EOS; the
whole pipeline then unwinds cleanly within one frame period. Teardown always
triggers the stop handle first.

## Stats, health, alerts

Per-stream stats ride ordinary telemetry under
`zensight/v1/<origin>/telemetry/parallax/<stream>/stats/<metric>` every
`stats_interval_secs`, **aggregated over the stream's open profiles**:

| Metric | Kind | Meaning |
|--------|------|---------|
| `fps` | gauge | frames published per second (video + preview combined) |
| `kbps` | gauge | total media bandwidth published for the stream |
| `drops` | counter | frames lost between encoder and egress (video-profile sequence gaps; intentional preview throttling is never counted) |
| `viewers` | gauge | profiles with matching subscribers (0–2) |
| `encode_ms` | gauge | average wall time per encoder `process()` call (omitted when no encoder ran) |

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
- `encoder_overrun` — average `encode_ms` above the strictest open profile's
  per-frame budget (1000 / fps).

## Limitations

- **V4L2 double-open**: video + preview profiles on the same camera open the
  device twice; most UVC cameras reject a second open (`EBUSY`). Opening the
  second profile then fails with an error status — close one profile first.
  (A shared-capture tee is a possible future enhancement.)
- **RTSP is H.264-only** and passthrough: bitrate/GOP config does not apply,
  and `max_height` is ignored for the video profile. Each open profile makes
  its own RTSP connection (two when video + preview are both open). The
  connect runs in its own task, **never inside the session actor**: the
  profile slot is reserved as *pending* (refcounting opens/closes that race
  the connect; the stream reads `open` in status/catalogue meanwhile), so an
  unreachable camera (bounded by a 5 s timeout) stalls no stream commands, no
  `@rpc/parallax/streams` replies, and no `state/parallax/stream/<stream>`
  doc updates. If the SDP carries
  no video dimensions, `FrameMeta.width/height` are `0` (= unknown) on the
  video profile and the JPEG preview cannot be opened (the encoder needs a
  size).
- `open_stream.max_height` caps the encoded height for encoder-backed video
  profiles only (aspect preserved) and only where the sensor generates the
  frames (test patterns). For V4L2 the camera's negotiated size is used and
  a too-small `max_height` is logged and ignored (no in-pipeline rescale
  yet). Previews always use the source size.
