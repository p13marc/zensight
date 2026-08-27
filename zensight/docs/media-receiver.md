# The media tiles' receiver half (#716, #717, #718, #720)

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

## The health panel (#719)

The numbers above answer "what is happening"; the panel answers the question an operator
actually has, which is **"which stage is losing it?"**. It is the drill-down on a tile: click
a tile to expand it, and the panel sits between the caption and the picture.

It is a **chain**, not five gauges, because the verdict is always a comparison between
adjacent stages and five gauges make the reader do the subtraction:

```
Source ──▶ Encoder ──▶ Transport ──▶ Decoder        Presentation   Recovery
30 fps      12 fps       12 fps       12 fps         age 42/150ms   #54, 2.7 s
offered   −60%         measured     measured         0 sheds        2 requests
```

| | |
|---|---|
| offered 30, encoded 12 | the **encoder** cannot keep up |
| encoded 30, received 12 | the **transport** is losing frames |
| received 30, decoded 12 | the **decoder** is behind |

The worst hop is named in one sentence above the chain — that sentence is the feature; the
numbers are in service of it. A hop must lose ≥ 15 % before it is named: rates jitter by a
few percent between three-second windows, and a panel that shouts at 3 % teaches an operator
to ignore it.

Three honesty rules the panel is built around:

- **The first link is an offer, not a measurement.** Capture fps is on no key — the sensor
  publishes encoded fps (`stats/fps`) and nothing upstream of it. The chain starts at the
  tier's *applied* fps, labelled `offered` rather than `measured`, because presenting a
  config value as a measurement is how a panel lies.
- **A rate needs two reports.** The wire counters are cumulative (that is what makes a resend
  idempotent), so `TileState` keeps the previous report and the panel diffs it over the newer
  one's `interval_ms`. A counter that went *backwards* — a reopened tile — yields no rate at
  all rather than a negative one.
- **Missing inputs read as `not asked`** — the same vocabulary the fleet view uses (RFC 09
  §5.1 O4). An unstamped stream shows frame age unavailable, never `0 ms`; a preview tile
  shows `no queue`, never `0`.

Source columns come from the sensor's own `{stream}/stats/*` and the per-tier status doc;
`drops` and `rc_drops` stay separate rows because they are disjoint by construction and mean
different things — a pipeline drop leaves a sequence gap, a rate-control drop does not, and
folding them would erase the difference between "this box is too slow" and "you asked for
400 kbps".

## The tier controller (#720)

The report is also a controller tick. Every three seconds the tile has a fresh window of
evidence about its own link, and the question it answers is the one an operator was
answering by hand: **is this viewer on the right rung?**

**The viewer changes its own subscription. It never asks the sensor to re-tune an encoder.**
RFC 07 §1.2 is normative on that — two operators on different links watch the same camera,
and one of them asking for less must not degrade the other. The escape hatch, if a
deployment ever needs one viewer's private rate, is a tier of its own. So the controller's
only lever is which `<tier>` key this tile subscribes to, and it needs no new wire surface
at all.

### Why a lower tier is the right lever

The #713 measurement
([`docs/plans/adaptive-media/loss-measurement.md`](../../docs/plans/adaptive-media/loss-measurement.md))
found that loss is amplified by **access-unit size**: Zenoh fragments an access unit across
datagrams and defragmentation is all-or-nothing, so at 1 % packet loss an 842 B unit was
lost 1.5 % of the time, a 34 KB unit 20 %, a 136 KB unit 41 %. Halving the bytes per frame
roughly halves the chance a frame is lost at all. Downgrading is not merely cheaper; on this
plane it is *repair*.

The same measurement is why frame age is a first-class input rather than a secondary one: on
today's `tcp/` deployments congestion showed up as 3.5–9 s of frame age with the sensor's own
`stats/drops` at zero.

### The three inputs, and the shape of the decision

| input | from | downgrade above | upgrade below |
|---|---|---|---|
| loss | `lost_frames` Δ ÷ (received + lost) Δ | 4 % | 0.5 % |
| frame age | `frame_age_ms` | 0.75 × deadline | 0.30 × deadline |
| decode queue | `decoder_queue_depth` ÷ `DECODE_QUEUE_CAP` | 0.60 | 0.20 |

Any one of the three triggers a downgrade; **all three** must be healthy for an upgrade. The
band between the two columns is the hysteresis, and it is why every threshold is two numbers
and never one comparison flipped.

An input that is absent stays absent. Unstamped samples mean `frame_age_ms: None` — "not
asked", never zero — and the age test drops out rather than reading as either wonderfully
fresh or hopelessly late. An unset deadline means the operator asked for no latency policy,
and the controller does not invent one.

### Why the tile's own sheds are not a fourth input

`dropped_frames` looks like the most direct evidence there is — the tile saying "I threw this
away". It is deliberately not read, because every shed cause a *downgrade* could fix is
already one of the three inputs, and the shed is the symptom rather than the cause: a deadline
shed means frame age was over the limit (**age** says so, from the same report); a queue-full
shed means the decoder is behind (**queue depth** says so); an unsynced or backlog shed is
about the tile starting up, and a lower tier does not help.

The live proof is the zero-deadline case in `zensight/tests/media_receiver_live.rs`: 26 of 27
frames shed with a measured frame age of **0.4 ms**. That is a configuration saying "nothing
is ever fresh enough", not a link the ladder can rescue — and a controller reading sheds would
have walked that tile to the bottom rung for nothing.

### Anti-flapping is most of the design

A switch closes a profile, opens another, rebuilds a decoder and costs a keyframe. A
flapping controller is worse than none:

- **`MIN_DWELL` (12 s)** in a tier before any further move.
- **`COOLDOWN` (9 s, three report cadences)** after a switch during which reports are
  *discarded, not merely ignored*. The first reports after a switch describe the decoder
  rebuild and its resync keyframe; averaging them in teaches the controller that switching
  causes the problem switching just fixed.
- **Downgrade on the first degraded window; upgrade only after `UP_SUSTAIN` (30 s)** of
  continuous health. One middling window restarts that clock.
- **A move at the end of the ladder is not a move.** `next_tier` returns `None`, no switch is
  sent, and the dwell is *not* reset — resetting it is how a controller already on the bottom
  rung starves itself of the recovery window it is waiting for.

The ladder's rung order comes from `TierSpec::bitrate_kbps`, not from the order the catalogue
lists tiers in: rung order is a property of the tiers, and reading it off an array would make
a config file's formatting load-bearing.

### The human always wins

An explicit tier click **pins** the stream — the controller stops deciding until the operator
hands control back with the `Auto` button that appears beside the tier buttons while pinned.
A tier that moved itself back after a deliberate click would be indistinguishable from a bug.
There is no separate "adaptation off" switch: a pin *is* off, for the one stream the operator
pinned, and it is expressed by the thing they already did. Closing a tile drops the pin with
it, so a pin never outlives the tile it was about.

An automatic move says so, once: `"video0: link degraded — video dropped to medium"`. The
manual click's own pair of toasts ("Opened video", "Closed preview") is suppressed for an
automatic switch — they describe the mechanism, fire twice per move, and an operator who did
not ask for a quality change is owed the *reason*, not the plumbing.

The controller is created on the first measured window rather than at open, so a freshly
opened tile is effectively immovable for a report cadence longer than `MIN_DWELL`. That is
the conservative direction and is left as it is.

The decision runs **before** the reporting guards in `send_parallax_report`, deliberately:
choosing a rung is this viewer's own business, so a viewer that cannot *tell* the producer
how the stream is arriving must still be able to act on it. Tying adaptation to a reachable
`stream/report` would disable it exactly where the link is worst.

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
| `zensight/src/app.rs` | `parallax_stream_report_key`, `send_parallax_report`, `parallax_tier_decision` |
| `zensight/src/view/specialized/parallax_tier.rs` | the controller: signals, thresholds, dwell, the ladder (#720) |
| `zensight/src/view/settings.rs` | `max_live_latency_ms` |
| `zensight/src/view/specialized/parallax_health.rs` | the chain, the verdict, the panel (#719) |
| `zensight/tests/media_receiver_live.rs` | the live loop, `#[ignore]`d |
