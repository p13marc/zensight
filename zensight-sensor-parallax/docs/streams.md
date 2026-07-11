# Streams, profiles, and pipelines

## The model

One **catalogue entry** (= one `<stream>` key chunk) can be open in up to two
**profiles**, each an independent parallax pipeline with its own publisher,
egress task, and refcount:

| Profile | Key | Encoding | Purpose |
|---------|-----|----------|---------|
| `video` | `@media/<stream>/video/h264/<profile>` | `video/h264` | full-rate live view |
| `preview` | `@media/<stream>/preview/jpeg` | `image/jpeg` | low-fps GUI tiles |

An `open_stream` command with `codec: "h264"` (or the sensor default) opens
the video profile; `codec: "mjpeg"` opens the preview profile. Profiles are
refcounted per open command and reaped when the refcount is 0 **and** the
publisher has had no matching subscribers for `idle_timeout_secs` (the
matching listener is the crash backstop for GUIs that die without
`close_stream`).

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

## Frame metadata

Every media sample carries a CBOR `FrameMeta` attachment
(`zensight-common::stream::FrameMeta`): keyframe flag, optional
pts/dts/duration (ns), per-stream sequence, width, height. Sequence gaps mean
dropped frames (LiveVideo QoS is best-effort by design).

## Teardown

`close_stream` decrements the profile refcount; at 0 the profile enters the
idle countdown (unless a viewer is still subscribed). The idle reaper stops
the pipeline, aborts the egress and matching-listener tasks, and undeclares
the publisher. Closing the last profile marks the stream inactive in the
catalogue and publishes a `StreamStatus{open: false}` transition.

A profile that was opened but never gets (or loses) its viewer is reaped by
the same countdown even if its refcount is non-zero — the matching listener
is the crash backstop for GUIs that die without `close_stream`, and an opener
that never subscribes is a zombie.

**Stopping a live pipeline** (parallax 0.1.1 gotcha): the unified executor
runs source loops on blocking threads and ignores downstream channel closure,
so `PipelineHandle::abort()` alone cannot end a live source — the blocking
task would run forever. Every source the sensor builds is wrapped in a
`StoppableSource` whose `StopHandle` flips the next `produce()` to EOS; the
whole pipeline then unwinds cleanly within one frame period. Teardown always
triggers the stop handle first.

## Limitations

- **V4L2 double-open**: video + preview profiles on the same camera open the
  device twice; most UVC cameras reject a second open (`EBUSY`). Opening the
  second profile then fails with an error status — close one profile first.
  (A shared-capture tee is a possible future enhancement.)
- **RTSP is H.264-only** and passthrough: bitrate/GOP config does not apply,
  and `max_height` is ignored for the video profile. Each open profile makes
  its own RTSP connection (two when video + preview are both open). The
  connect happens inside the session actor (bounded by a 5 s timeout), so an
  unreachable camera briefly serializes stream commands. If the SDP carries
  no video dimensions, `FrameMeta.width/height` are `0` (= unknown) on the
  video profile and the JPEG preview cannot be opened (the encoder needs a
  size).
- `open_stream.max_height` caps the encoded height for encoder-backed video
  profiles only (aspect preserved) and only where the sensor generates the
  frames (test patterns). For V4L2 the camera's negotiated size is used and
  a too-small `max_height` is logged and ignored (no in-pipeline rescale
  yet). Previews always use the source size.
