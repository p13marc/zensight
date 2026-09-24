# zensight-web

A browser client for the `@media` plane (epic #704). This package is the
**control half** (#706): the parallax catalogue, per-stream status, and
open / close / keyframe — everything except the pixels, which are #707's
WebCodecs tile. It speaks real zenoh from the browser through
[`zenoh-ts`](https://github.com/eclipse-zenoh/zenoh-ts) 1.10.1 and the
remote-api bridge (#705, `just remote-api`), with **no new server-side
machinery**: the sensor's existing `@rpc` procedures and state documents
are the whole API.

```bash
cd web
npm ci
npm run dev          # http://localhost:5173 → connect to ws://localhost:10000
npm run check        # tsc, generated-types drift, vitest
npm run smoke        # the control plane against a real bus (see below)
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

## The smoke

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

## Not here

Video (#707: the WebCodecs H.264 tile and the JPEG preview tile, keyframe
gating, resync, the frame-age deadline); a second session for `@media` so a
saturated tile cannot delay control (#723); anything that writes without a
host.
