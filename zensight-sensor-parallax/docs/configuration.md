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

## `discovery` block (#410) — opt-in, propose-only

**Absent means no discovery, ever.** The block's presence is the opt-in, the
same shape `snmp.discovery` uses (#541).

| Key | Default | Meaning |
|-----|---------|---------|
| `mdns` | `false` | browse `_rtsp._tcp` over mDNS |
| `ws_discovery` | `false` | send an ONVIF WS-Discovery `Probe` and collect `ProbeMatches` |
| `browse_secs` | `10` | how long one round listens |
| `interval_secs` | `3600` | seconds between rounds (floored at 60) |

Note that both probes default to **false even inside the block**: each is named
explicitly, so enabling discovery never turns on a protocol the operator did not
ask for. A block that enables nothing is refused at startup rather than
publishing an empty report forever — which would read as "there are no cameras"
instead of "you did not turn anything on".

**What it does, and the line it does not cross.** Responders that are not
already configured streams are published on `state/parallax/discovery` as a
`StreamDiscoveryReport`, each with a copy-pasteable JSON5 `rtsp[]` snippet. That
is all. A discovered camera does not enter the catalogue, gets no liveliness
token, and is never captured, encoded or published. **There is no `auto_add`
and there will not be one**: a camera is a device with a view of a room, and a
monitoring system that starts pulling video off hardware nobody configured has
done something categorically different from noticing that it exists.

Most responders arrive **without a URL**, and that is not a defect. mDNS gives a
service address, and only a `path` TXT record (RFC 6763 §6.5, which most cameras
omit) turns that into a stream URL. **WS-Discovery never gives one at all**: its
`XAddrs` is the device's *service* endpoint, and turning that into a stream URI
is an ONVIF Media `GetStreamUri` call — a different protocol surface, with
authentication. So the suggested snippet carries a visible
`rtsp://<address>/<path>` placeholder: an operator must be able to see that
something is missing, because a guessed path would look configured and fail at
connect time.

There is no `onvif-rs` dependency behind `ws_discovery`. The obvious library is
git-only and unreleased, this workspace has zero git dependencies, and
`deny.toml` sets `unknown-git = "deny"` — while WS-Discovery itself is one SOAP
datagram to a multicast group and a reply to parse. A camera's `ProbeMatch`
carries its ONVIF scopes, from which the name and hardware model are lifted
(percent-decoded) into the report; the raw scopes are kept beside them rather
than replaced by this crate's reading of them.

The probe joins **no multicast group**: it sends *to* the group from an
ephemeral port and devices answer unicast to that port, so receiving needs
nothing more. Joining would additionally subscribe the host to every other
WS-Discovery conversation on the segment. Multicast TTL is 1, because
WS-Discovery is link-local by design and a probe that escapes the segment is a
probe on somebody else's network.

**Everything in the report comes from an untrusted responder** (#1149). Nothing
on either probe authenticates the device on the other end, so every string the
round keeps — name, address, URL, and each attribute key and value — is passed
through one sanitiser before it is stored, rendered or pasted:

- **control characters are dropped**, not escaped. Escaping makes them safe to
  paste and still leaves a terminal or a label rendering them, and they carry
  nothing a camera name needs;
- the value is **truncated to 256 characters** and marked with `…`. The
  256-responder cap bounds how *many* devices a round keeps; this bounds how
  large one of them can make itself. A device answering with a megabyte of ONVIF
  scope otherwise inflated the LWW document every round.

The snippet's values are **serialised, not interpolated**. `url` is built from
the responder's own TXT `path`, so a device advertising
`path=/live",  name: "override` used to produce a snippet that is not the object
it appears to be — and the snippet exists precisely to be pasted into
`configs/parallax.json5` without being read closely. Quotes and backslashes are
escaped rather than stripped, so a legitimate path is not silently rewritten;
the keys stay unquoted, so the entry still looks like the rest of the file.

**Operational note.** Both probes are multicast on a network you may not own,
and they are traffic an IDS can flag — the same caution the SNMP subnet sweep
carries. Keep them to networks you operate. Rounds are capped at 256 responders: this document
is LWW state a GUI renders, not a log.

## `evidence` block (#413) — the cameras as hosts

A camera is a host on the network. Without a claim about it the correlator has
nothing to file its streams under. This block publishes **observer-role**
`HostEvidence` claims on `state/parallax/evidence/device/<stream>` — the same
shape `snmp`, `netring` and `netlink` use for devices they merely observe.

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `true` | publish claims at all |
| `refresh_secs` | `300` | liveness refresh: an unchanged claim is republished this often so the consumer's 900 s TTL never expires a camera that is still configured (must be > 0) |
| `include_discovered` | `true` | also claim responders the `discovery` block found (no effect without one) |

**What a claim carries, and what it cannot.** A configured RTSP target
contributes what its URL says about the host — an IP, a bare or `.local`
name (`hostname`), or a fully-qualified name (`fqdn`) — and `platform:
"rtsp"`. The credentials in the URL never leave the process. A discovered
responder contributes its address, its advertised name and the ONVIF
`hardware` scope as a display-only `vendor`. Neither probe yields a MAC or a
serial, so these claims sit on the ip / fqdn / hostname rungs of
`zensight-common/docs/identity-evidence.md` and no higher: they attach to a
host netring or netlink saw at that address, and they add no correlator
rule. `host_id` is always absent. Local V4L2 devices get no claim — they *are*
this host, which `evidence/self` already covers.

**Keys.** A configured target is keyed by its catalogue stream name; a
discovered one by the slug the discovery proposal suggests as a stream name,
so a camera the operator later adopts under that name keeps its key. When
both name the same slug the configured entry wins. Publishing is
change-driven with the refresh above: never per tick.

## `artifacts` block (#414) — stills and clips

The live `@media` plane is lossy and ephemeral by design. These two kinds
are its reliable complement, served over the artifact channel
(`@rpc/parallax/artifact/request` → `@blob/artifact`) with the progress,
status, cancel and TTL every artifact gets from the framework. They sit under
`parallax.artifacts` — the producers own their limits, as netring's capture
does; the top-level `artifacts` block stays the framework's report and
snapshot.

| Key | Default | Meaning |
|-----|---------|---------|
| `still.enabled` | `false` | serve one JPEG frame of a stream (`kind: "still"`, `stream`) |
| `still.quality` | `85` | JPEG quality 1–100 |
| `still.max_height` | none | aspect-preserving height cap; the source size otherwise |
| `still.max_bytes` | `8388608` | a larger frame fails the request rather than serving it |
| `still.cooldown_secs` / `ttl_secs` / `chunk_size` | `5` / `600` / `524288` | the channel's per-kind gap, retention and transfer chunk |
| `clip.enabled` | `false` | serve a bounded H.264 recording in an MP4 (`kind: "clip"`, `stream`, `duration_secs`, optional `tier`) |
| `clip.max_duration_secs` | `60` | a longer request is clamped, and the clip says so |
| `clip.max_bytes` | `67108864` | the recording stops early at this size, and the clip says so |
| `clip.cooldown_secs` / `ttl_secs` / `chunk_size` | `30` / `600` / `524288` | as above |

**One-shot pipelines, no tee.** A still or a clip opens its own pipeline —
the same graph a live preview or tier uses — and tears it down when done; see
`streams.md`. A test source and an RTSP camera are unaffected by an open
viewer; a **V4L2 device is exclusive**, so a request for a stream that is
already open is refused ("close it first"), never queued. **RTSP stills and
clips are a follow-up**: the one-shot pipeline has no async connect + SDP
path yet, and the request is refused by name. A clip records the requested
tier, or `video.default_tier`, with exactly the encoder shaping a live open of
that tier would get.

## Validation

Startup fails (with a clear message) on: `evidence.refresh_secs` of 0, `artifacts.still.quality` outside 1..=100, an enabled `artifacts.clip` with `max_duration_secs` of 0 or no video tier, duplicate or empty stream names,
names containing `/` or `*`, `preview.fps == 0`, `preview.quality` outside
1..=100, `preview.max_height < 2`, zero test-source dimensions or fps,
`video.gop_frames == 0`, an empty tier ladder, a tier with an empty/`/`/`*`
name or duplicate tier names, a tier with `fps == 0` or `bitrate_kbps == 0` or
`max_height < 2`, a `default_tier` naming no tier, `idle_timeout_secs == 0`,
`stats_interval_secs == 0`.

A `discovery` block is also validated: it must enable at least one probe (an
empty block that silently does nothing is worse than no block), `browse_secs`
must be > 0, and `interval_secs` must be at least `browse_secs` — otherwise the
next round would start before this one finished.

## Environment overrides

The shared Zenoh block honors `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` like
every other sensor.
