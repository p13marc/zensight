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

1. ~~**Nothing measures latency.**~~ **Closed by #716.** RFC 07 §1.3 named the clock (the
   publisher's HLC sample timestamp); #714 put it in a wire type; `zensight_common::media`
   now states the rule once — unstamped is *not asked* and never zero, negatives shown
   unclamped — and the tiles act on it.
2. ~~**The H.264 path cannot shed.**~~ **Closed by #716/#717.** The tile enqueues access units
   on a bounded channel a blocking decode task drains, so the backlog is a number
   (`max_capacity() - capacity()` — the browser's `decodeQueueSize`, same field, same
   meaning) and the frame-age deadline is applied where that backlog is visible. Arena
   exhaustion, an oversize AU and a decode refusal are three named outcomes now, not one
   `Ok(None)`. See `zensight/docs/media-receiver.md`.
3. ~~**The producer never hears from the consumer.**~~ **Closed by #714/#715.** RFC 04 R6 makes
   the data planes producer→consumer only, so feedback had no home in the grammar; RFC 07 §1.1
   gave it one on `@rpc`, and the sensor serves it and publishes the per-tier aggregate.
4. **Adaptation is a human clicking a tier.** The clock, the report, a *reporter* and now a
   measured loss model all exist, so this is a controller waiting to be written — #720, and
   nothing gates it any more.

## What is already done — do not re-plan it

| | Where |
|---|---|
| The `@media` plane, exact tier keys, `frame` QoS | zenkey RFC 07 §1; parallax epic [#204](https://git.marcpardo.eu/marcpardo/parallax/issues/204) (32 issues, closed) |
| `FrameMeta` on the attachment, byte-compatible twins + CBOR corpus | `zensight-common/src/stream.rs:109`, `parallax/src/wire/frame_meta.rs:45`, #711 |
| Keyframe requests, four transports, coalesced | `StreamControl::RequestKeyframe`; parallax `ForceKeyUnit` / `KeyframeHandle` / matching edge |
| Concurrent quality tiers + per-viewer selector | RFC 07 §1, `TierSpec`, #497, #498, #502, #507 |
| Sequence-gap detect → resync → keyframe, with backoff | `zensight/src/view/specialized/parallax_h264.rs` |
| The receiver half: deadline, bounded queue, drop taxonomy, the reporter | `zensight/docs/media-receiver.md` (#716, #717, #718) |
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

**No retransmission and no FEC.** Drop-stale + request-keyframe, and nothing else. The rule and
the two conditions that would reopen it are written down in
[`recovery-policy.md`](recovery-policy.md) (#721), so the next NACK proposal can be answered
with a link.

## Two caveats that have now been settled — by measurement (#713)

**~~Best-effort is currently a no-op in flight.~~ Measured, and half of it was wrong (#713).**
Over `tcp/` best-effort still cannot lose a sample *in flight* — but the assumption that the
resulting sender-side drops are therefore *visible* does not survive contact. At 300 kbit
against ~1.7 Mbps offered, the receiver missed **83 % of sequence numbers while `stats/drops`
stayed at 0**, and frame age reached **3.5 s median**: the frames died in Zenoh's transport
queue, upstream of every counter the sensor publishes. A congested `tcp/` link and a lossy one
are, today, indistinguishable to the GUI. Follow-up:
[#801](https://git.marcpardo.eu/marcpardo/zensight/issues/801).

**~~A second, unmeasured effect.~~ Measured too, and the mechanism is real.** On
`quic/…?mixed_rel=1` best-effort does ride unreliable datagrams, Zenoh fragments across them,
and defragmentation is all-or-nothing — so loss is amplified by **access-unit size**: at 1 %
packet loss, a 34 KB access unit was lost 20 % of the time and a 136 KB one 41 %. The strict
`1-(1-p)^n` product is an upper bound (it over-predicts, increasingly with size; UDP GSO is the
likely reason and is named for the next person to test). `max_slice_len` (#509) changes nothing
— this sensor publishes a whole access unit as one sample, so zenkey #303's corollary holds,
and its stated condition, *while one sample is one access unit*, is the thing to re-check.
Numbers: [`loss-measurement.md`](loss-measurement.md).

**The `express` disagreement is settled, our way.** RFC 04 §3 M1 (v1.26) takes `express` off the
`frame` profile: transport batching engages only under back-pressure, which is exactly when
`frame`'s `drop` says to shed stale frames rather than spend per-message framing overhead.
`zensight-common/src/qos.rs` has always returned false and #733 pinned it with
`express_is_off_for_every_class` plus `zensight-sensor-parallax/docs/qos-express.md`. parallax's
own `@media` sink still sets it true, which is now upstream's deviation and not ours — and one
more reason we publish through our own egress rather than its `ZenohSink` (#710, closed).

## Issues

### Protocol — settled (zenkey RFC **v1.26**, 2026-08-25)

All four were ratified together, in the media-consumer amendment batch. Nothing
in this epic is waiting on a decision.

| Was | Now |
|---|---|
| zenkey #366 | **RFC 07 §1.3** — the frame-age clock is the publisher's HLC sample timestamp, read as *observed skewed latency*: negatives shown not clamped, unstamped counted separately and **never as zero**. `FrameMeta` gains no wallclock. |
| zenkey #367 | **RFC 07 §1.1** — `@rpc/<producer>/stream/report`, `write`, `MediaReceiverReport` → `Ack`, `idempotent = true`. Consumer id in the payload, never in the key. |
| zenkey #368 | **RFC 07 §1.2, normative** — a producer MUST NOT re-tune a shared tier from one consumer's report; aggregate action needs a stated arbitration rule that is not "the most recent report". The escape hatch is a tier of its own. |
| zenkey #304 | **RFC 04 §3 M1** — `frame` **loses** `express`; `alert` keeps it. We are the conformant side: `qos.rs` has always returned false, and #733 pinned it. |

### This repo — milestone *Adaptive media: feedback and control*

| Issue | Stage |
|---|---|
| #713 | **Done.** The measurement — both legs, the recipes, the numbers: [`loss-measurement.md`](loss-measurement.md) |
| #714 | **Done.** `MediaReceiverReport` + the `stream/report` RPC (type, registry, CBOR corpus) |
| #715 | **Done.** Sensor serves `stream/report`, keeps bounded per-consumer state, publishes the `{stream}/rx/{tier}/*` aggregate |
| #716 | **Done.** Frame-age deadline — shed instead of drifting behind live |
| #717 | **Done.** Bounded decode queue, real queue depth, and a drop taxonomy |
| #718 | **Done.** Both tile kinds publish `MediaReceiverReport` every 3 s |
| #719 | **Done.** Stream health panel — the chain, and which hop is losing the picture |
| #801 | New, from #713: count what the transport dropped — congestion is invisible to every counter we publish |
| #720 | Receiver-driven tier selection with hysteresis — **unblocked**; the loss model to cite is `loss-measurement.md` verdict 1 |
| #721 | **Done.** [`recovery-policy.md`](recovery-policy.md); the durable half is in `zensight-sensor-parallax/docs/streams.md` |

### Browser twins — milestone *Browser frontend for the @media plane* (#704)

| Issue | Note |
|---|---|
| #722 | Frame-age deadline + `decodeQueueSize` backpressure in the WebCodecs tile |
| #723 | A second zenoh-ts session for `@media` — one WebSocket has no priority lanes |

### The sender's half — parallax, milestone *Adaptive media: the sender's half*

| Issue | Note |
|---|---|
| [parallax #245](https://git.marcpardo.eu/marcpardo/parallax/issues/245) | The `@media` session leaves timestamping off — every frame unstamped. **Blocks nothing here**: our sensor publishes through `zensight_sensor_core::RawMediaPublisher`, a declared publisher on a `zensight_common::session` session that forces `timestamping/enabled = true`, and we do not adopt parallax's `ZenohSink` (#710). Upstream's problem for upstream's own users |
| [parallax #246](https://git.marcpardo.eu/marcpardo/parallax/issues/246) | Derived rate metrics — fps and kbps, not just counters |
| [parallax #247](https://git.marcpardo.eu/marcpardo/parallax/issues/247) | `plans/bandwidth-control.md` says "not started" and Phase A shipped |

## Order

```
zenkey #366 #367 #368            protocol, decided first
        │
        ├── #714 ✔ ─────────────► #715 ✔ ──► #718 ✔
        │
        └───────────────────────► #716 ✔, #717 ✔ ──► #719 ✔
                                                         │
        #713 ✔ (measure) ──────────────────────────────┴──► #720
```

`#722`/`#723` follow their own epic's order (#705 → #706 → #707 → these two).

## Ownership

- **parallax** owns capture, encoding, framing, the keyframe promise, sender metrics, and
  stamping what it publishes.
- **zensight** owns consumption, decode, playout, shedding, receiver metrics, the health
  surface, and the tier controller.
- **zenkey** owns the wire contract: descriptor semantics, timing semantics, the feedback
  schema's home, and what a producer may do with what it hears.
