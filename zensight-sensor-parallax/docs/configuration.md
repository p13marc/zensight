# Configuration

JSON5, top-level blocks `zenoh` / `parallax` / `logging` (plus optional
`artifacts`). Shipped example: [`configs/parallax.json5`](../../configs/parallax.json5).
Minimal:

```json5
{ zenoh: { mode: "peer" }, parallax: {} }
```

## `parallax` block

| Key | Default | Meaning |
|-----|---------|---------|
| `source` | `"auto"` | Instance label in payloads; `"auto"` resolves the hostname (v1 keys are origin-scoped, so it no longer appears in key expressions). |
| `enumerate_v4l2` | `true` | Advertise local `/dev/video*` cameras. Headless hosts contribute nothing. |
| `rtsp` | `[]` | Remote RTSP cameras (see below). |
| `test_sources` | `[]` | Synthetic `VideoTestSrc` patterns (see below). |
| `preview.fps` | `2` | JPEG preview frame rate (thumbnails, not video). |
| `preview.quality` | `75` | JPEG quality 1–100. |
| `preview.max_height` | `360` | Aspect-preserving cap on the thumbnail height; `null` = source size. A 1080p camera's thumbnail is otherwise a 1080p JPEG. Applies to every preview that re-encodes; the V4L2 MJPG passthrough forwards the camera's JPEG verbatim, uncapped. |
| `video.gop_frames` | `60` | Keyframe (IDR) interval in frames, shared by every tier (a tier's `encoder.gop_frames` overrides it). |
| `video.encoder` | all unset | Encoder shaping shared by every tier; each tier may override any field (see below). |
| `video.default_tier` | `"medium"` | The tier an `open_stream` with no explicit `tier` resolves to. |
| `video.tiers` | low/medium/high (see below) | The bandwidth-tier ladder — the heart of demand-driven simulcast (#494). |
| `idle_timeout_secs` | `30` | Tear an open profile down after this long with no viewers and no explicit opens. |
| `stats_interval_secs` | `5` | Per-stream stats telemetry cadence. |

### `video.tiers` — the bandwidth ladder

Each tier is published concurrently on its own `@media/parallax/<stream>/video/h264/<tier>`
key with independent resolution/fps/bitrate. A viewer subscribes to exactly the
tier its link can take, so viewers on different links never fight over one
encoder (see [`streams.md`](streams.md)). The sensor owns the numbers; the wire
and the key carry the name.

```json5
video: {
  gop_frames: 60,          // keyframe (IDR) interval in frames (shared)
  default_tier: "medium",  // the tier an open with no explicit tier gets
  tiers: [
    { name: "low",    max_height: 240,  fps: 10, bitrate_kbps: 400  },
    { name: "medium", max_height: 480,  fps: 20, bitrate_kbps: 1200 },
    { name: "high",   max_height: null, fps: 30, bitrate_kbps: 4000 },  // null = native
  ],
}
```

- `name` — the `<tier>` key chunk; must be unique and contain no `/` or `*`.
- `max_height` — aspect-preserving height cap (`null` = source native). The
  scaler never upscales, so a tier whose cap exceeds a camera's native height is
  simply not *offered* for that camera in the catalogue (it would upscale).
- `fps` / `bitrate_kbps` — the tier's target framerate and encoded bitrate cap.
- `encoder` — optional per-tier encoder shaping (below).

### `video.encoder` — encoder shaping (#509)

The ladder says *what* a tier delivers; this says *how* the encoder gets there.
Set it once under `video.encoder` and/or per tier in that tier's own `encoder`
block — **the tier wins, field by field**, and a field neither sets is never
passed to the encoder at all, so it keeps OpenH264's own default rather than a
number this project guessed.

**Everything ships unset**, and a default build behaves exactly as it did
before these knobs existed.

```json5
video: {
  encoder: { complexity: "low" },          // shared by every tier
  tiers: [
    { name: "low",  max_height: 240, fps: 10, bitrate_kbps: 400,
      encoder: { profile: "baseline", gop_frames: 20 } },   // this rung only
    { name: "high", max_height: null, fps: 30, bitrate_kbps: 4000 },
  ],
}
```

| Key | Values | Meaning |
|-----|--------|---------|
| `profile` | `baseline` / `main` / `high` | H.264 profile. All three are verified to decode through the GUI's own OpenH264 decoder (`every_profile_the_ladder_can_name_decodes_like_the_gui`); unset lets the codec choose. |
| `complexity` | `low` / `medium` / `high` | CPU spent per frame. **`low` is the answer to a firing `encoder_overrun`** — cheaper than dropping resolution, and invisible to the receiver. |
| `usage_type` | `camera_realtime` / `screen_realtime` / `camera_non_realtime` / `screen_non_realtime` | What is being encoded. A property of the *source*, so set it on `video.encoder`, not per tier. Only camera/RTSP/test sources exist today. |
| `qp` | `0`–`51` | Target quantiser. Under `RateControlMode::Bitrate` with frame skipping on (what this sensor uses), the rate controller works in a ±4 band around it. |
| `gop_frames` | `> 0` | Per-tier keyframe interval, overriding `video.gop_frames`. A lossy low tier wants a short GOP (fast recovery and late-join); a high tier wants a long one (efficiency). |
| `max_slice_len` | `200`–`65535` bytes | Cap on each emitted NAL. **Off, and it buys nothing on today's egress** — see [`streams.md`](streams.md). |

Rejected at load: `qp > 51` (parallax clamps it silently, which would mean
something other than what the config says), `max_slice_len` outside
`200..=65535`, `gop_frames: 0`, and an `encoder` key naming no tier. An
unrecognised `profile`/`complexity`/`usage_type` spelling fails at *parse*, with
the offending field named.

### `rtsp` entries

```json5
{ name: "door",                     // stream id — unique, single key chunk
  url: "rtsp://cam.local:554/s1",   // rtsp:// URL (no inline credentials)
  username: "viewer",               // optional
  password: "secret",               // optional — never republished anywhere
  description: "front door" }       // optional; shown in the GUI catalogue
```

### `test_sources` entries

```json5
{ name: "test0",     // stream id — unique, single key chunk
  pattern: "smpte",  // smpte / checkerboard / ball / gradient / snow / solid / black / white
  width: 640, height: 360,
  fps: 15 }
```

Test sources ride the identical catalogue/command/encode/egress path as real
cameras, so they double as demo mode and CI fixtures.

## Validation

Startup fails (with a clear message) on: duplicate or empty stream names,
names containing `/` or `*`, `preview.fps == 0`, `preview.quality` outside
1..=100, `preview.max_height < 2`, zero test-source dimensions or fps,
`video.gop_frames == 0`, an empty tier ladder, a tier with an empty/`/`/`*`
name or duplicate tier names, a tier with `fps == 0` or `bitrate_kbps == 0` or
`max_height < 2`, a `default_tier` naming no tier, `idle_timeout_secs == 0`,
`stats_interval_secs == 0`.

## Environment overrides

The shared Zenoh block honors `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` like
every other sensor.
