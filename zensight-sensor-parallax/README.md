# zensight-sensor-parallax

Live video for the ZenSight observability platform, built on the
[parallax](https://github.com/p13marc/parallax) pipeline engine. Advertises
local **V4L2 cameras**, remote **RTSP cameras**, and synthetic **test
patterns** as a stream catalogue, and — only when a viewer asks — encodes and
publishes video onto Zenoh's opaque `@media` plane: H.264 access units for the
live view and low-fps JPEG previews for the GUI tile grid. No viewer, no
pixels: streams are opened by command, torn down on close or idle, and a
matching listener forces a keyframe the instant a subscriber appears.

## What it does

- **Stream catalogue** — `@/query/streams` → `Vec<StreamDescriptor>`
  (name, codecs, active flag, description). Sources: enumerated `/dev/video*`,
  configured RTSP URLs, configured `VideoTestSrc` patterns (demo mirrors the
  real contract).
- **On-demand pipelines** — `@/commands/stream` carries
  `Command<StreamControl>` (`open_stream` / `close_stream` /
  `request_keyframe`); each open profile is an independent parallax pipeline,
  refcounted per requester and reaped after `idle_timeout_secs` without
  viewers.
- **Media egress** — `@media/<stream>/video/h264/<profile>` (encoding
  `video/h264`) and `@media/<stream>/preview/jpeg` (encoding `image/jpeg`),
  every sample carrying a CBOR `FrameMeta` attachment (keyframe flag,
  pts/dts/duration, sequence, dimensions). Never a telemetry envelope.
  Video viewers subscribe with the profile chunk as a single-chunk wildcard
  (`…/video/h264/*`) — `video.profile` is sensor config the catalogue does
  not carry (see `docs/KEYSPACE.md` §3.3).
- **Status + stats** — `StreamStatus` transitions on `@/status/streams`
  (declared publisher; failed opens publish a definitive `open: false`), a
  queryable on the same key replying `Vec<StreamStatus>`, and per-stream
  telemetry under `<stream>/stats/{fps,kbps,drops,viewers,encode_ms}` so
  existing charts light up for free.
- **Liveliness + health + alerts** — one `@/devices/<stream>/alive` token per
  catalogue entry; per-stream health tracking; alert rules for disappeared
  cameras, RTSP connect failures, and encoder overrun on `@/alerts/*`.

## Quick start

```bash
cargo build -p zensight-sensor-parallax --release   # builds openh264 from source (C++)
cargo run -p zensight-sensor-parallax --release -- --config configs/parallax.json5
```

The shipped config advertises one `test0` SMPTE test pattern (640x360@15), so
the sensor streams on any machine, camera or not. V4L2 capture is
unprivileged on most distros (`video` group).

## Documentation

- [docs/streams.md](docs/streams.md) — the stream/profile model, pipeline
  shapes per source kind, keyframe control, teardown, limitations.
- [docs/configuration.md](docs/configuration.md) — every config key.
- [../docs/KEYSPACE.md](../docs/KEYSPACE.md) — the authoritative
  key-expression contract (§3.3 media plane).
