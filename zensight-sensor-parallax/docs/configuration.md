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
| `preview.max_height` | `360` | Aspect-preserving cap on the thumbnail height; `null` = source size. A 1080p camera's thumbnail is otherwise a 1080p JPEG. |
| `video.gop_frames` | `60` | Keyframe (IDR) interval in frames, shared by every tier. |
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
