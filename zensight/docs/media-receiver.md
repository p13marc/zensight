# The media tiles' receiver half (#716, #717, #718)

How a parallax tile measures its own stream, what it does about being late, and what it
tells the producer. The sender's half — capture, encoding, tiers, keyframes — is
[`zensight-sensor-parallax/docs/streams.md`](../../zensight-sensor-parallax/docs/streams.md);
the wire contract is zenkey RFC 07 §1.1–§1.3.

## Why

A tile that goes soft used to give an operator nothing. The camera could be starved, the
encoder dropping, the link lossy, the decoder behind, or the UI — and all five looked
identical, because the tile counted none of them and told nobody. Three specific holes:

- `decode_to_rgba` returned `Ok(None)` for *both* "the decoder is buffering" and "the
  arena had no free slot". A tile losing a third of its frames to a starved arena looked
  exactly like one losing them to the network.
- The H.264 path decoded every access unit serially with no backlog drain, so latency
  grew in the subscriber queue where nothing could see it.
- The publisher's HLC timestamp — the one clock that can answer "how late is this frame?"
  — was on every sample and read nowhere.

## The four numbers, and where each comes from

| | |
|---|---|
| **Frame age** | `zensight_common::media::observed_frame_age_ms` — publisher HLC minus local arrival |
| **Decode queue depth** | `max_capacity() - capacity()` of the tile's bounded decode channel |
| **Loss** | inferred from `FrameMeta.sequence` gaps |
| **Sheds** | counted at the point each one happens, by cause |

## The frame-age clock

RFC 07 §1.3, implemented once in `zensight-common/src/media.rs` so the iced tile, the
browser tile (#722) and any conformance judge read it the same way. Three rules, each
easy to break by accident:

1. **The clock is the publisher's HLC sample timestamp** — the one the middleware stamps
   when a session enables timestamping, which `zensight_common::session` forces on
   fleet-wide. Not `FrameMeta`'s `pts_ns`/`dts_ns`: those are pipeline-clock values with
   an arbitrary origin, deliberately not comparable across hosts.
2. **Unstamped is *not asked*, never zero.** A producer with timestamping off has no
   frame age. `Some(0.0)` there reads as "perfectly fresh" and silently disables every
   deadline built on it — so the deadline reports itself **inactive** instead.
3. **Negatives are shown, not clamped.** Frame age is *observed skewed latency*: an
   observation, never a verdict on the transport. A negative age is the clock-skew
   evidence, and clamping it destroys the only signal saying the number cannot be trusted.

## The deadline (#716)

For live monitoring a recent frame beats an old one decoded perfectly.

```
access unit arrives
   ├── age > max_live_latency and NOT a keyframe ──► shed; RequestKeyframe (backed off)
   ├── decode queue full                         ──► shed; RequestKeyframe (backed off)
   ├── out of sync and NOT a keyframe            ──► shed (undecodable, not lost)
   └── otherwise                                 ──► enqueue for the decoder
```

**A late keyframe is always taken.** Shedding those too would leave a tile on a genuinely
slow link showing nothing at all; taking them gives a slideshow that snaps back to live
the moment the link does. The report says the age was high either way.

Every path that drops sync asks for the fresh IDR through one gate, `RESYNC_MIN_INTERVAL`
(2 s), so no combination of them can add up to the keyframe storm #435 was about. The gate
is cleared by a **healthy** decode, not by any decode: under sustained lateness the tile
sheds a delta, asks for an IDR, and that IDR is always admitted — so clearing the backoff on
the keyframe it just asked for would pace requests by decoded keyframes instead of by time,
and the steady state of that is an all-intra stream pushed onto the link that was already
too slow. #435's storm, rebuilt out of #716's parts.

**A deadline no frame can ever meet is disarmed.** Frame age is *observed skewed latency*,
and a deadline turns an observation into a verdict — fine while the two clocks agree,
catastrophic when they do not. A fleet host whose clock trails the viewer's by three seconds
makes every one of its frames read as three seconds old; every delta would be shed, every
tile would degrade to a slideshow, and `dropped_frames` would blame the viewer for a clock.
The **smallest age ever observed** separates the cases: under a real backlog some frames
arrive fresh, so the floor is small; under a systematic offset the floor *is* the offset. A
deadline below that floor is disarmed (after 30 stamped samples, so it cannot fire on the
first late frame), logged with the floor it measured, and the tile keeps playing. The
reported age is **not** corrected — subtracting the floor would launder skew into a latency
number, which is exactly what RFC 07 §1.3 forbids. An operator reading "frame age 3000 ms"
on a LAN has been told precisely what is wrong.

**Configuring it.** `max_live_latency_ms` in `~/.config/zensight/settings.json5`, or the
Settings view's *Live video latency deadline*. Default 1500 ms; `0` disables it; anything
else must be 100–30000. It is per deployment on purpose — a LAN wall display and a
satellite operator want different numbers — and it is read when a tile opens, so a change
takes effect on the next tile rather than mutating a running one under its own accounting.

## The decode queue (#717)

An access unit is enqueued on a **bounded** channel (8 deep) that a long-lived blocking
task drains. The channel is the whole point: the backlog that used to grow invisibly in
the subscriber queue is now a number readable at any instant — the same quantity the
browser tile gets for free from `VideoDecoder.decodeQueueSize`, reported in the same field
with the same meaning.

It is deliberately shallow. `frame`'s QoS already declares a stale frame worthless, so
depth beyond "cover a scheduling hiccup" only converts a visible drop into invisible
latency, which is the exact failure the deadline exists to stop.

A decoder reset rides the *same* channel as the access units, because a reset is a point
in the stream and not a side channel: resetting out of band would rebuild the decoder
underneath frames that were fine. A reset the queue is too full to accept is retried
before the next enqueue rather than dropped — a decoder that missed its reset would smear
stale reference frames into the recovery keyframe.

**Arena slots** are 2 MiB × 8 per open video tile. They were 1 MiB, which a
native-resolution IDR on a high tier can exceed; an oversize access unit is a named
failure (`DecodeFailure::Oversize`), counted, and after three strikes ends the tile with a
stated reason — because an encoder producing a picture this tile cannot hold will produce
another one at the next IDR, and resyncing at it forever is not a diagnosis.

## The drop taxonomy

Every frame that enters a tile and does not reach the screen has exactly one cause. The
wire carries two fields; the sub-causes stay on the tile for the health surface (#719) and
the log line.

| Cause | Field | Meaning |
|---|---|---|
| sequence gap | `lost_frames` | **network** loss, or a sender-side drop — not our doing |
| deadline miss | `dropped_frames` | too old on arrival |
| queue full | `dropped_frames` | the decoder is behind |
| unsynced delta | `dropped_frames` | the reference chain is gone; waiting for an IDR |
| preview backlog | `dropped_frames` | superseded by a newer JPEG in the latest-wins drain |
| unreadable metadata | `dropped_frames` | the sample carried no readable `FrameMeta` |
| arena full | `dropped_frames` | no free slot; the frame never reached the decoder |
| oversize AU | `dropped_frames` | larger than one decoder slot |
| decode failure | `dropped_frames` | the decoder refused it |

Merging the two fields would tell a producer its link is bad when the truth is that the
viewer's box is too slow — the diagnosis epic #712 exists to make possible.

## The report (#718)

Every open tile — video **and** preview — sends its producer a `MediaReceiverReport` on
`@rpc/parallax/stream/report` every **3 s**.

- **Cadence.** The registry declares a *ceiling* of one report per second per
  `(consumer_id, stream, tier)`; RFC 07 §1.1's reference cadence is one per few seconds.
  3 s sits inside the ceiling with enough headroom that a scheduling hiccup cannot turn a
  well-behaved tile into an `error/busy`. The tick runs on its own clock, not off arriving
  frames: **a tile receiving nothing still reports**, and a report saying "nothing is
  arriving" is the most useful one there is.
- **Addressed, never broadcast.** The write goes to the tile's own origin. There is no
  fleet fallback — the registry entry omits `fanout` precisely so a fleet-wide report is
  unrepresentable, and a report we cannot address is a report we drop.
- **`consumer_id` is `zs-<pid>-<generation>`.** In the payload, never in a key (RFC 07
  §1.1), which is why it can be this cheap: nothing keys on it, so it has no cardinality
  budget to blow. It is stable for one tile incarnation and regenerated on reopen.
- **Stops with the tile.** The cadence lives inside the tile's own subscriber task, which
  the abort handle kills on close, so reports stop within one cadence and before the
  subscriber is undeclared. A report from a replaced incarnation is dropped rather than
  forwarded: it would keep a dead `consumer_id` alive in the sensor's per-tier map for a
  whole idle window, and `rx/{tier}/consumers` counts exactly those.
- **A refusal is said once.** A tile reports every few seconds for as long as it is open, so
  a producer that refuses — an older sensor with no `stream/report` queryable — would put a
  red toast on screen every 3 s per tile, forever. `ParallaxReportOutcome` toasts the first
  refusal and stays quiet until reports work again.
- **Previews report too.** A preview tile's numbers are simpler — every JPEG is a
  keyframe, so there is no reference chain and no decode queue (`decoder_queue_depth` is
  omitted, not zero) — but an operator comparing them against a video tile's is how you
  tell "the camera is fine, H.264 is not".

What the producer does with a report is **nothing that touches an encoder**: RFC 07 §1.2
is normative that a producer must not re-tune a shared tier from one consumer's feedback.
It folds reports into the per-tier `{stream}/rx/{tier}/*` aggregate and publishes that.
The sanctioned adaptation is this end changing which tier key it subscribes to — #720.

## Verifying it against a live sensor

The pure parts — the frame-age rule, the playout policy, the drop taxonomy — are unit tests
in the modules below. What they cannot show is that the loop **closes**, so that has its own
test, `#[ignore]`d because it needs a running sensor and h264-gated because it decodes:

```sh
ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:17451 ZENSIGHT_ZENOH_SCOUTING=false \
  ZENSIGHT_ZENOH_GOSSIP=false \
  cargo run -p zensight-sensor-parallax -- --config configs/parallax.json5 &

ZENSIGHT_MEDIA_LIVE_ENDPOINT=tcp/127.0.0.1:17451 \
  cargo test -p zensight --features h264 --test media_receiver_live -- --ignored --nocapture
```

`configs/parallax.json5` ships a synthetic SMPTE `test0` stream, so this runs on a headless
box with no camera. Two cases:

- **The loop closes.** A real tile subscribes, decodes, measures itself, and the report it
  produces is accepted by the producer and shows up in `{stream}/rx/{tier}/consumers`. It
  prints the report, because the point of running it by hand is to *look* at the numbers —
  a green assertion that frame age is `Some` says nothing about whether it is 40 ms or 4 s.
  On loopback it reads ~0.6 ms median, ~2.5 ms max.
- **A tile that cannot meet its deadline still plays.** A zero-millisecond deadline is the
  cheapest way to reproduce a permanently late tile without netem: every delta arrives "too
  old". Observed: 8 received, 7 shed, **1 keyframe decoded**, `lost_frames` 0, and two
  keyframe requests rather than seven — the backoff holding.

Neither runs in CI and neither could: the `features` job only `cargo check`s `h264`, and no
CI job stands a camera up. `scripts/conformance-verify.sh` is where a live deployment gets
judged.

## Where the code is

| | |
|---|---|
| `zensight-common/src/media.rs` | the frame-age rule, and only that |
| `zensight/src/view/specialized/parallax_receiver.rs` | `ReceiverStats`, the drop taxonomy, the snapshot |
| `zensight/src/view/specialized/parallax_h264.rs` | the bounded queue, the decode task, the deadline |
| `zensight/src/view/specialized/parallax_detail.rs` | the preview tile's half; `TileState::last_report` |
| `zensight/src/app.rs` | `parallax_stream_report_key`, `send_parallax_report` |
| `zensight/src/view/settings.rs` | `max_live_latency_ms` |
| `zensight/tests/media_receiver_live.rs` | the live loop, `#[ignore]`d |
