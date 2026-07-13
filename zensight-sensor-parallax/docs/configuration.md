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
| `video.bitrate_kbps` | `2000` | H.264 target bitrate. |
| `video.gop_frames` | `60` | Keyframe (IDR) interval in frames. |
| `video.profile` | `"main"` | Profile name used in the `@media/parallax/<stream>/video/h264/<profile>` key chunk. |
| `idle_timeout_secs` | `30` | Tear an open profile down after this long with no viewers and no explicit opens. |
| `stats_interval_secs` | `5` | Per-stream stats telemetry cadence. |

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
1..=100, zero test-source dimensions or fps, `video.bitrate_kbps == 0`,
`video.gop_frames == 0`, `idle_timeout_secs == 0`, `stats_interval_secs == 0`.

## Environment overrides

The shared Zenoh block honors `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` like
every other sensor.
