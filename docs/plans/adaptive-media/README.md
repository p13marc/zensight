# Adaptive media (epic #712)

Working notes for the epic that closes the loop on the `@media` plane: measurement, receiver
feedback, and receiver-driven adaptation.

Live plan — corrected against what shipped. Archived rationale graduates to
[`docs/design/`](../../design/) and to `zensight-sensor-parallax/docs/streams.md`.

## Why

The `@media` plane works. What it cannot do is explain itself.

A tile that goes soft gives an operator no way to tell whether the camera is starved, the
encoder is dropping, the link is lossy, the decoder is behind, or the UI is. And a viewer whose
link degrades has exactly one recovery move: a human clicking a lower tier.

Four gaps, in dependency order:

1. **Nothing measures latency.** `FrameMeta.pts_ns` is a producer-local pipeline clock and the
   GUI never reads it. The clock is already on the wire — zenoh stamps every sample and
   `zensight-common/src/session.rs:89` enables timestamping fleet-wide — and nothing is allowed
   to use it. (zenkey #366; parallax #245 for the sender that doesn't stamp.)
2. **The H.264 path cannot shed.** It decodes every access unit serially with no backlog drain,
   unlike the preview's latest-frame-wins loop (`parallax_detail.rs:424`). Latency grows in the
   subscriber queue, invisibly. Arena exhaustion returns `Ok(None)` uncounted
   (`parallax_h264.rs:149`).
3. **The producer never hears from the consumer.** Feedback has no home in the grammar —
   RFC 04 R6 makes the data planes producer→consumer only. (zenkey #367.)
4. **Adaptation is a human clicking a tier.** With a clock and a report it can be a controller.
   (zenkey #368.)

## What is already done — do not re-plan it

| | Where |
|---|---|
| The `@media` plane, exact tier keys, `frame` QoS | zenkey RFC 07 §1; parallax epic [#204](https://git.marcpardo.eu/marcpardo/parallax/issues/204) (32 issues, closed) |
| `FrameMeta` on the attachment, byte-compatible twins + CBOR corpus | `zensight-common/src/stream.rs:109`, `parallax/src/wire/frame_meta.rs:45`, #711 |
| Keyframe requests, four transports, coalesced | `StreamControl::RequestKeyframe`; parallax `ForceKeyUnit` / `KeyframeHandle` / matching edge |
| Concurrent quality tiers + per-viewer selector | RFC 07 §1, `TierSpec`, #497, #498, #502, #507 |
| Sequence-gap detect → resync → keyframe, with backoff | `zensight/src/view/specialized/parallax_h264.rs:286-347` |
| Sender-side stats | `telemetry/parallax/{stream}/stats/{fps,kbps,drops,rc_drops,viewers,encode_ms}` (#407, #503) |
| Every runtime encoder handle | `zensight-sensor-parallax/src/pipeline.rs:289` — cloned, reachable, deliberately undriven (#504, #513) |
| Codec identification for a decoder | parallax `h264_profile_level_id` (#215); zenkey #303 — the codec string is derived from the SPS, deliberately **not** carried on the wire |
| Browser playback (zenoh-ts + WebCodecs) | #704 milestone, in flight |
| The protocol specification | zenkey RFC 04 §1/§3, 05 §3/§5, 07 §1, 11; `parallax/docs/zenoh-wire.md`. Amendments, not a new document |

## Two things this epic deliberately does not do

**No sender-side rate control.** Concurrent tiers exist so two viewers on different links do
not fight over one encoder — RFC 07 §1's own words. Both trees reached this independently:
parallax `plans/bandwidth-control.md:298` ("Deliberately out of scope: Automatic ABR") and this
repo's #504/#513. zenkey #368 makes it normative. The escape hatch, if a deployment ever needs
one viewer's private rate, is a **tier of its own** — never mutating a shared one.

**No retransmission and no FEC.** Drop-stale + request-keyframe first; #721 writes down the rule
for when repair is worth anything, so the next NACK proposal can be answered with a link.

## Two caveats to settle before anyone tunes a threshold

**Best-effort is currently a no-op in flight.** `QosClass::LiveVideo` sets
`Reliability::BestEffort` + `CongestionControl::Drop`, but every config in `configs/` is a
`tcp/` peer and `mixed_rel` / `rel=0` appear nowhere in the tree. Over TCP, best-effort only
permits the *sender* to drop under congestion — nothing is lost in flight. So every sequence gap
the GUI has ever recovered from was a sender-side drop or a pipeline restart. A controller
tuned on that is tuned on the wrong distribution. Hence #713, which blocks #720.

There is a second, unmeasured effect. Zenoh fragments to give the illusion of an unlimited MTU
and defragmentation is all-or-nothing, so on a QUIC link with `mixed_rel=1` a 50 KB IDR is ~45
datagrams at a 1200 B MTU and one lost fragment loses the whole keyframe — at 1 % packet loss,
~64 % of keyframes. zenkey #303's "MTU slicing buys nothing on this plane" holds only while one
sample is one access unit; it stops holding if slices become samples. #509 already added
`max_slice_len` as a tier knob and nothing has ever exercised it on a lossy link.

**The two implementations disagree on `express`.** parallax's `@media` sink sets it true
(`zenoh.rs:1417`), matching RFC 04 §3's `frame` profile; `zensight-common/src/qos.rs:87` returns
false for every class and pins it with a test. That is zenkey #304 — now shipping on both sides
of one wire, and it gates any latency claim this epic makes.

## Issues

### Decide first — zenkey (they block code)

| Issue | What it settles |
|---|---|
| [zenkey #366](https://git.marcpardo.eu/marcpardo/zenkey/issues/366) | Name the frame-age clock: `Sample.timestamp()`, with #119's observed-skewed honesty; require an `@media` publisher to enable timestamping |
| [zenkey #367](https://git.marcpardo.eu/marcpardo/zenkey/issues/367) | Receiver feedback is an `@rpc` write (`stream/report`), not a publication; records the rejected viewer-origin-telemetry alternative |
| [zenkey #368](https://git.marcpardo.eu/marcpardo/zenkey/issues/368) | Tiers are the quality knob; bound what a producer may do with feedback |
| [zenkey #304](https://git.marcpardo.eu/marcpardo/zenkey/issues/304) | The `express` divergence (pre-existing, now load-bearing) |

### This repo — milestone *Adaptive media: feedback and control*

| Issue | Stage |
|---|---|
| #713 | Measure — what does `@media` loss actually look like? **Blocks #720** |
| #714 | `MediaReceiverReport` + the `stream/report` RPC (type, registry, CBOR corpus) |
| #715 | Sensor serves `stream/report`, keeps bounded per-consumer state, publishes the aggregate |
| #716 | Frame-age deadline — shed instead of drifting behind live |
| #717 | Bound the decode queue and count the drops |
| #718 | Publish `MediaReceiverReport` from the parallax tiles |
| #719 | Stream health panel — attribute degradation to a stage |
| #720 | Receiver-driven tier selection with hysteresis |
| #721 | The latency-aware recovery policy (docs) |

### Browser twins — milestone *Browser frontend for the @media plane* (#704)

| Issue | Note |
|---|---|
| #722 | Frame-age deadline + `decodeQueueSize` backpressure in the WebCodecs tile |
| #723 | A second zenoh-ts session for `@media` — one WebSocket has no priority lanes |

### The sender's half — parallax, milestone *Adaptive media: the sender's half*

| Issue | Note |
|---|---|
| [parallax #245](https://git.marcpardo.eu/marcpardo/parallax/issues/245) | The `@media` session leaves timestamping off — every frame unstamped. **Blocks #716** |
| [parallax #246](https://git.marcpardo.eu/marcpardo/parallax/issues/246) | Derived rate metrics — fps and kbps, not just counters |
| [parallax #247](https://git.marcpardo.eu/marcpardo/parallax/issues/247) | `plans/bandwidth-control.md` says "not started" and Phase A shipped |

## Order

```
zenkey #366 #367 #368            protocol, decided first
        │
        ├── parallax #245 ──┐    a stamped sample
        │                   │
        ├── #714 ───────────┼──► #715 ──► #718
        │                   │
        └───────────────────┴──► #716, #717 ──► #719
                                                  │
        #713 (measure) ───────────────────────────┴──► #720
```

`#722`/`#723` follow their own epic's order (#705 → #706 → #707 → these two).

## Ownership

- **parallax** owns capture, encoding, framing, the keyframe promise, sender metrics, and
  stamping what it publishes.
- **zensight** owns consumption, decode, playout, shedding, receiver metrics, the health
  surface, and the tier controller.
- **zenkey** owns the wire contract: descriptor semantics, timing semantics, the feedback
  schema's home, and what a producer may do with what it hears.
