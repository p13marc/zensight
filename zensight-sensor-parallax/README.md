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

- **Stream catalogue** — a GET on `@rpc/parallax/streams` →
  `Vec<StreamDescriptor>` (name, codecs, active flag, **native geometry, and the
  bandwidth tiers the stream offers**, description). Sources: enumerated
  `/dev/video*`, configured RTSP URLs, configured `VideoTestSrc` patterns
  (demo mirrors the real contract).
- **Demand-driven tiered simulcast (#494)** — a GET on `@rpc/parallax/stream/set`
  carries `Command<StreamControl>` (`open_stream {codec, tier}` /
  `close_stream {codec, tier}` / `request_keyframe {tier}`). The video plane is
  a *ladder* of tiers (`low`/`medium`/`high`), each an **independent** encoder
  published concurrently on its own `<tier>` key with its own
  resolution/fps/bitrate. A viewer subscribes to exactly the one tier its link
  can take, so two viewers on different links never fight over one encoder. Each
  (stream, tier) and the preview is refcounted per requester and reaped after
  `idle_timeout_secs` without viewers. Live re-tuning (bitrate seamless; GOP /
  resolution rebuild + IDR) runs on the pipeline's control handles — no teardown.
- **Media egress** — `zensight/v1/<origin>/@media/parallax/<stream>/video/h264/<tier>`
  (encoding `video/h264`) and `…/@media/parallax/<stream>/preview/jpeg`
  (encoding `image/jpeg`), every sample carrying a CBOR `FrameMeta` attachment
  (keyframe flag, pts/dts/duration, sequence, tier-encoded dimensions). Never a
  telemetry envelope.
  On the h264 path the keyframe flag is derived from the bitstream (IDR
  present) and every keyframe access unit is published self-contained —
  cached SPS/PPS are prepended when missing — so a fresh decoder can start
  at any advertised keyframe (`docs/streams.md`, "Self-contained
  keyframes").
  Video viewers subscribe to the **exact** `<tier>` key — keyspace v1.3 revoked
  the `…/video/h264/*` wildcard (RFC 07 §3): the catalogue advertises the tiers,
  so a `*` would pull every tier at once (see `docs/KEYSPACE.md`).
- **Status + stats** — per-stream LWW status docs on
  `state/parallax/stream/<stream>` (declared publisher; failed opens publish
  a definitive `open: false`; tombstoned on removal from config, not on
  close), and per-stream telemetry under
  `<stream>/stats/{fps,kbps,drops,viewers,encode_ms}` so existing charts
  light up for free.
- **Liveliness + health + alerts** — one `state/parallax/device/<stream>/alive`
  token per catalogue entry; per-stream health tracking; alert rules for
  disappeared cameras, RTSP connect failures, and encoder overrun on
  `state/parallax/alert/*`.

## Quick start

```bash
cargo build -p zensight-sensor-parallax --release   # builds openh264 from source (C++)
cargo run -p zensight-sensor-parallax --release -- --config configs/parallax.json5
```

The shipped config advertises one `test0` SMPTE test pattern (640x360@15), so
the sensor streams on any machine, camera or not. V4L2 capture is
unprivileged on most distros (`video` group).

## Documentation

- [docs/streams.md](docs/streams.md) — the stream/tier model, demand-driven
  simulcast, pipeline shapes per source kind, live control, keyframe control,
  teardown, limitations.
- [docs/configuration.md](docs/configuration.md) — every config key.
- [../docs/KEYSPACE.md](../docs/KEYSPACE.md) — the authoritative
  key-expression contract (the `@media` plane: RFC 04/07).
