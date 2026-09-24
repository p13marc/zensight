# zensight-web

A browser client for the `@media` plane (epic #704): the parallax
catalogue, per-stream status and open / close / keyframe (#706), and the
pixels — a WebCodecs H.264 tile and a JPEG preview tile (#707). It speaks
real zenoh from the browser through
[`zenoh-ts`](https://github.com/eclipse-zenoh/zenoh-ts) 1.10.1 and the
remote-api bridge (#705, `just remote-api`), with **no new server-side
machinery**: the sensor's existing `@rpc` procedures, state documents and
`@media` keys are the whole API.

```bash
cd web
npm ci
npm run dev          # http://localhost:5173 → connect to ws://localhost:10000
npm run check        # tsc, generated-types drift, vitest
npm run smoke        # the control plane against a real bus (see below)
npm run smoke:media  # the media plane through the bridge, no decoding
```

## What it does, and where each half lives

| | key | module |
|---|---|---|
| which hosts run parallax | `v1/*/state/parallax/alive` (liveliness) | `src/origins.ts` |
| catalogue | GET `v1/<origin>/@rpc/parallax/streams` → `StreamDescriptor[]` (JSON) | `src/catalogue.ts` |
| per-stream status | SUB `v1/<origin>/state/parallax/stream/*` → `StreamStatus` (JSON) | `src/catalogue.ts` |
| open / close / keyframe | GET `v1/<origin>/@rpc/parallax/stream/set` with a `Command<StreamControl>` body | `src/control.ts` |
| the write's answer | empty reply = executed; `reply_err` = `RpcError` in the sensor's own words | `src/rpc.ts` |
| key spellings | transliterated from `zensight_common::keyexpr` and the parallax registry | `src/keys.ts` |
| the zenoh session | a narrow `Bus` interface over zenoh-ts, faked in the tests | `src/bus.ts`, `src/fake.ts` |
| video | SUB `v1/<origin>/@media/parallax/<stream>/video/h264/<tier>` — the EXACT tier key — into WebCodecs | `src/tile.ts`, `src/webcodecs.ts` |
| preview | SUB `…/preview/jpeg` into `createImageBitmap` | `src/preview.ts` |
| the sidecar | the CBOR `FrameMeta` attachment on every media sample | `src/cbor.ts` |
| the codec string | `avc1.<profile-level-id>` from the first keyframe's SPS | `src/h264.ts` |
| what a tile measures | `ReceiverStats`: loss, sheds by cause, frame age, jitter, the deadline floor | `src/receiver.ts` |
| what it tells the producer | `MediaReceiverReport` every 3 s on `@rpc/parallax/stream/report` | `src/report.ts` |

Keys are **base-less**: the deployment namespace is set on the bridge
(`configs/router-remote-api.json5`) and never in the browser.

## The two traps, as code

**Close must be profile-correct.** A `CloseStream` without `codec` resolves
to the sensor's *default* video tier and decrements the wrong refcount.
`Profile.closeCommand()` is the only way this package spells a close and it
always names codec and tier (`mjpeg`, no tier, for the preview). Switching
tiers (`ControlPlane.switchTo`) captures the outgoing profile's close before
building the open, and commands on one `ControlPlane` are sent **serially,
each awaited**, so the sensor's actor sees close-then-open in arrival order.

**Origin resolution.** A browser has no fleet model, so it cannot spell a
single key until it knows which host it is talking to. `Origin` accepts
exactly `h-<12 hex>` and nothing else: the `*` origin is unrepresentable,
because RFC 07 §3 forbids wildcarding the origin on `@media` (every matching
host ships the full payload) and this client applies the same rule to the
control plane — a stream control is a statement to one host's sensor, and
the fleet spelling would open, close or retune on every host (#1261's rule
in the iced GUI). The page resolves origins from the parallax liveliness
token and refuses a hand-typed origin that is not one.

## What the sensor does with a close

A close decrements the profile's refcount. At zero the profile enters the
sensor's **idle countdown** (`idle_timeout_secs`, 30 s in
`configs/parallax.json5`) and is reaped — and its status published
`open: false` with `last_end.reason = closed` — when the countdown expires
with no subscriber on the media key. So a close is not immediately visible
in the status document, by contract: the subscriber's falling edge (#707
undeclares its subscriber on close) is the other input, and a refcount
alone must not tear a pipeline down while a viewer may still be on it. See
`zensight-sensor-parallax/docs/streams.md`.

## The pixels (#707)

`src/tile.ts` is the iced tile's receive loop (`parallax_h264.rs`,
written up in `zensight/docs/media-receiver.md`) driven by samples instead
of a select loop, with the decoder behind an interface so the loop is
unit-tested without WebCodecs (`src/tile.test.ts`, a fake decoder and a
fake clock). The rules it keeps, and the constants it shares with the iced
twin so both clients' reports mean the same thing:

- **Keyframe gate.** Nothing reaches the decoder before the first
  `FrameMeta.keyframe`. The codec string comes from that keyframe's SPS
  (`profile_idc`, constraint flags, `level_idc` are the six hex digits), and
  `VideoDecoder` is configured **without** a `description`, which is what
  tells WebCodecs the bitstream is Annex-B. A profile no browser decodes
  (High 10 / 4:2:2 / 4:4:4) ends the tile with that sentence.
- **Sequence gap ⇒ resync**: drop sync, reset and reconfigure the decoder,
  ask for an IDR — through one gate, `RESYNC_MIN_INTERVAL` 2 s, cleared only
  by a *healthy* decode (nothing shed since the last picture). A regression
  past `SEQ_RESTART_GAP` (300) is the sensor's pipeline counter restarting:
  re-anchor, do not freeze.
- **The frame-age deadline** (`maxLiveLatencyMs`, default 1500, clamped
  100…30000, 0 off): a late delta is shed and an IDR asked; a late keyframe
  is **never** shed. The clock is the publisher's HLC sample timestamp;
  unstamped is *not asked*, never zero. A deadline no frame has ever met
  (after 30 stamped samples) is a clock offset, not a backlog, and disarms
  itself — the reported age is never corrected.
- **`decodeQueueSize` is the backpressure** at `DECODE_QUEUE_CAP` 8: shed,
  drop sync, ask — never block.
- **Three end conditions with a reason**: no sample in 10 s, samples but no
  decode in 12 s, the sensor's `StreamStatus{open:false}` while the tile has
  no picture.
- **The JPEG preview** has no gate and no queue; it has the latest-wins
  drain (a superseded JPEG is a `backlog` shed), which the video path never
  copies because every AU is a reference.
- Every `VideoFrame` is `close()`d on every path; the subscriber is
  undeclared on close — that falling edge is the sensor's teardown signal.

Reports carry `lost_frames` (sequence gaps — the network) apart from
`dropped_frames` (everything shed on purpose); the causes stay local.
`decoder_queue_depth` is omitted, not zero, for a preview.

**Two sessions** (#723's shape): the page opens a second WebSocket for
`@media` by default, so a saturated tile cannot queue in front of an alert
on the control session — a WebSocket is one ordered stream and has none of
the bus's priority lanes. The measurement #723 asks for (control RTT under
a saturated tile) is not done here.

## Generated types

`src/types.gen.ts` is **generated, not hand-written**, from
`schemas/*.json`, which `zensight-common/tests/web_schemas.rs` dumps from
the fleet type table — the same JSON Schemas every producer serves on
`@rpc/<producer>/describe`. That test fails when a `#[serde]` change is not
reflected in the checked-in schema; `npm run gen -- --check` (in
`npm run check`) fails when the TypeScript is behind the schema. Regenerate
both with:

```bash
UPDATE_WEB_SCHEMAS=1 cargo test -p zensight-common --test web_schemas
cd web && npm run gen
```

## The smokes

`npm run smoke` is the acceptance of #706 against a real bus: it connects
to the bridge, resolves an origin from liveliness, reads the catalogue,
opens the first h264 tier of a stream, waits for the status document to say
open, asks for a keyframe, closes profile-correctly, and waits out the idle
countdown for the document to say closed. `just web-smoke` stands the
topology up on isolated ports (a `zenohd` on 17447, the parallax sensor
with its synthetic test source, the bridge on 10000) and runs it. It runs
under Node with `--experimental-websocket --experimental-wasm-modules`:
zenoh-ts wants a global `WebSocket` and imports its key-expression checker
as an ESM wasm module, which is also why `vite.config.ts` carries
`vite-plugin-wasm` and an `esnext` target.

`npm run smoke:media` is #707's half against the real sensor, minus the
decoder: it subscribes to the exact tier key through the bridge and proves
that every sample crosses with its CBOR `FrameMeta` attachment and its HLC
timestamp intact, that the first admitted access unit is a keyframe whose
SPS yields a codec string, that the admitted sequence is contiguous, and
that the receiver reports are accepted. On loopback it reads a frame age
under a millisecond.

## Not here

The tier controller (#720's twin — the report it would read is produced);
#723's saturation measurement; anything that writes without a host.
