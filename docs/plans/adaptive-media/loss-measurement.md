# What `@media` loss actually looks like (#713)

Measured 2026-08-27 on one host, `zensight` @ `acfbd23`, `scripts/media-loss-lab.sh` +
`zensight-sensor-parallax/examples/media_loss_probe.rs`. Raw CSVs are not checked in; the
lab regenerates them and `scripts/media-loss-report.py` re-derives every table below.

This is the measurement epic [#712](https://git.marcpardo.eu/marcpardo/zensight/issues/712)
asked for before anyone tunes a threshold. Every knob in it — the frame-age deadline (#716),
the report's loss field (#714), the controller's downgrade point (#720) — is a number aimed at
a loss distribution nobody had produced.

## The two findings, first

**1. Over `tcp/`, congestion loses frames and no counter says so.** The epic assumed that over
TCP "best-effort only permits the *sender* to drop", and that such drops would be visible as
`stats/drops`. The first half is right. The second is **wrong**, and it is the more important
half. At 300 kbit with ~1.7 Mbps offered, the receiver saw **83 % of sequence numbers missing
while `stats/drops` stayed at 0** — and frame age reached **3.5 seconds median, 4.4 s max**. At
100 kbit: 93 % missing, **9.1 s median age**. The frames were discarded inside Zenoh's own
transport queue by `CongestionControl::Drop`, which is upstream of `stats/drops` (that counter
is derived at egress from gaps in what the AppSink handed on —
`zensight-sensor-parallax/src/egress.rs:158`). From the receiver, a congested TCP link is
indistinguishable from a lossy one.

That the frames went to congestion and not to the network is not an inference. Received
fraction tracks the link's share of the offered rate almost exactly:

| link rate | offered | link/offered | frames received |
|---|---|---|---|
| 10 Mbit | 1.54 Mbps | headroom | **260 / 260** |
| 300 kbit | 1.69 Mbps | 17.8 % | **44 / 255 = 17.3 %** |
| 100 kbit | 1.69 Mbps | 5.9 % | **14 / 193 = 7.3 %** |

**2. Over `quic/…?mixed_rel=1`, in-flight loss is real, and it is amplified by access-unit
size — but by less than the fragment model predicts.** Best-effort genuinely rides unreliable
QUIC datagrams (`zenoh-link-quic`'s `LinkUnicastQuicDatagram`), Zenoh fragments across them,
and defragmentation is all-or-nothing. At **1 % packet loss**:

| access unit | ⌈bytes / 1222⌉ | 1−(1−p)ⁿ predicts | **measured** | amplification |
|---|---|---|---|---|
| 842 B (A: `ball` 720p delta) | 1 | 1.0 % | **1.5 %** | 1.5× |
| 34 054 B (B: `snow` 320×180) | 28 | 24.5 % | **20.4 %** | 20× |
| 135 873 B (C: `snow` 640×360) | 112 | 67.6 % | **40.7 %** | 41× |

So the mechanism the issue predicted is real: **a 34 KB access unit is lost twenty times as
often as the link loses packets.** The strict product over-predicts, and increasingly so with
size — the effective independent-unit count is 1.5, 22.7 and 52.0 against a geometric 1, 28 and
112. The most likely cause is UDP **GSO**: quinn hands several datagrams to the kernel in one
`sendmsg`, netem impairs *skbs*, so datagrams inside one segmented batch share a fate. That is
consistent with the numbers but was not proved here — it is named so the next person tests it
rather than re-deriving the discrepancy. Treat `1−(1−p)ⁿ` as an **upper bound**.

## Method

Two network namespaces joined by one veth pair; the impairment sits on the sensor's egress, so
it acts on the media direction and not on acknowledgements. No qdisc is ever attached to `lo`
or to a real interface, and the EXIT trap removes everything.

```bash
# Leg 1 — today's deployment, tbf-throttled below the offered bitrate.
scripts/media-loss-lab.sh --leg tcp  --pattern snow --width 320 --height 180 \
    --bitrate 8000 --rate 300kbit --seconds 45
# Leg 2 — best-effort on unreliable QUIC datagrams, netem loss.
scripts/media-loss-lab.sh --leg quic --pattern snow --width 320 --height 180 \
    --bitrate 8000 --loss 1 --seconds 45
scripts/media-loss-report.py target/media-loss/*/
```

The netem/tbf recipes the script applies, verbatim:

```bash
ip netns exec zs-tx tc qdisc add dev zst root netem loss 1%          # leg 2
ip netns exec zs-tx tc qdisc add dev zst root tbf \
    rate 300kbit burst 32kbit latency 50ms                            # leg 1
ip -n zs-tx link set zst mtu 1280                                     # leg 2
```

**MTU is 1280, not the 1200 the issue names.** QUIC mandates a path carrying a 1200-byte UDP
*payload*, so an interface MTU of 1200 cannot carry a QUIC handshake at all — the first
attempt failed exactly there. 1280 is the IPv6 minimum and the smallest honest floor; it leaves
~1222 bytes of QUIC payload, which is the divisor in every table above.

The probe is deliberately **not** a viewer: it never decodes, never sheds and never asks for a
keyframe. What it costs is realism; what it buys is that the CSV is the wire rather than the
wire plus a recovery policy. It is also the QUIC *listener*, because
`zensight_common::session` sets connect-side TLS material only (in a deployment the listener is
the zenohd router) — the sensor under measurement keeps its normal config and its normal
`ZENSIGHT_ZENOH_TLS_CA` knob.

Access-unit size is moved with the test pattern and the quantiser rather than with a real
camera: `ball` at 720p gives ~0.85 KB deltas and 3.7 KB keyframes, `snow` at 320×180 gives
34 KB / 46.6 KB, `snow` at 640×360 gives 136 KB / 148 KB. Size is the independent variable, and
a synthetic pattern is the only way to hold everything else still.

## Leg 2 in full

| run | loss | recv | missing | `drops` | `rc_drops` | longest burst | age med/max ms |
|---|---|---|---|---|---|---|---|
| A `ball` 720p | 0 % | 270/270 | 0 | 0 | 0 | — | 0.4 / 6.2 |
| A | 0.1 % | 270/270 | 0 | 0 | 0 | — | 0.4 / 2.9 |
| A | 1 % | 266/270 | 4 (1.5 %) | 0 | 0 | 1 | 0.3 / 1.4 |
| A | 5 % | 256/270 | 14 (5.2 %) | 0 | 0 | 1 | 0.3 / 0.8 |
| B `snow` 320×180 | 0 % | 260/260 | 0 | 0 | 10 | — | 0.8 / 19.5 |
| B | 0.1 % | 255/260 | 5 (1.9 %) | 0 | 10 | 1 | 0.8 / 8.5 |
| B | 1 % | 207/260 | 53 (20.4 %) | 0 | 10 | 3 | 0.8 / 1.7 |
| B | 5 % | 112/260 | 148 (56.9 %) | 0 | 10 | 10 | 0.8 / 2.6 |
| C `snow` 640×360 | 1 % | 51/86 | 35 (40.7 %) | 0 | 249 | 3 | 2.3 / 3.4 |
| B + `max_slice_len=1200` | 1 % | 217/261 | 44 (16.9 %) | 0 | 10 | 3 | 0.8 / 1.8 |

Two things to read off it that are not the headline.

**Loss is bursty in frames, and the burst grows with the loss rate**: length 1 at 1 % on config
A, up to 3 at 1 % on B, up to **10 consecutive frames** at 5 % on B. Under TCP congestion it is
worse still — bursts of **9** at 300 kbit and **27** at 100 kbit. A controller fed a mean loss
rate cannot see the difference between 5 % as isolated frames and 5 % as one 10-frame hole, and
only the second is a visible glitch.

**`rc_drops` is not `drops` and must never be added to it.** `drops` is derived from egress
sequence gaps, so it is exactly the sender's share of what the receiver misses. `rc_drops` is
the encoder skipping a frame *before* a sequence number exists: it lowers the frame rate and
creates no gap. Adding them was this analysis's own first bug, and it turned a clean run into
negative wire loss — config B at 0 % loss has `rc_drops = 10` and a perfectly contiguous
0..259. `scripts/media-loss-report.py` now subtracts only `drops`.

## The three verdicts #713 asked for

### 1. The loss model #720 may assume

```
P(access unit lost) ≈ min(1, k · p · ceil(bytes / 1222))      k ≈ 0.5 … 1, decreasing with size
```

with `1 − (1−p)^n` as the upper bound. Two consequences for the controller:

- **Its input must be gap burst length, not a loss rate.** The bursts above are the whole
  reason: the same mean rate is a different picture depending on its shape, and
  `MediaReceiverReport` already carries what is needed to distinguish them
  (`lost_frames` alongside `last_keyframe_sequence` / `since_last_keyframe_ms`).
- **Its most effective lever is access-unit size, not tier bitrate as such.** Loss scales with
  the datagram count of an AU, so a tier that halves the bytes per frame roughly halves the
  loss probability — which is what the tier ladder already does, and is a better justification
  for it than bandwidth alone.

### 2. `max_slice_len` (#509) changes nothing on this plane

B at 1 % loses 20.4 % of frames; the same run with `max_slice_len = 1200` loses 16.9 % — one
run apart, inside the spread of the burst statistics, and with no mechanism to explain a real
difference. There is none: **this sensor publishes a whole access unit as one Zenoh sample**, so
losing a sample loses the AU whether it was one NAL or twenty. `configs/parallax.json5` already
says exactly this next to the knob, zenkey #303 says it normatively, and the measurement now
says it too. The RFC 07 §1.4 corollary holds — and its stated condition, *while one sample is
one access unit*, is the thing to re-check if slices ever become samples.

### 3. `express` (zenkey #304) — a footnote, as expected

RFC 04 §3 M1 already removed `express` from the `frame` profile and `zensight-common/src/qos.rs`
has always returned `false` (#733 pinned it). Nothing was measured that would change that:
`express` governs whether a message bypasses transport batching, and on the leg where batching
matters — the congested TCP link — the damage was multi-second **queueing**, which per-message
framing does not touch. No verdict to revisit.

## What this changes elsewhere

- **The epic's second caveat was half right and is now corrected in place.** "Best-effort is
  currently a no-op in flight" holds — but its implied comfort ("so the only losses are
  sender-side drops we can see") does not. See finding 1.
- **#716's deadline is doing more work than it was designed for.** It was aimed at a decoder
  backlog. The measured failure mode of today's shipped configuration is a *transport* backlog
  of 3.5–9 s, which the deadline catches for the same reason and reports as exactly what it is:
  observed frame age.
- **~~There is a counter missing.~~ There was a *distinction* missing, and the numbers above
  supply it (#801, closed).** Nothing in the sensor reports what Zenoh's transport dropped, and
  nothing can: those drops are counted only under Zenoh's `stats` cargo feature and only per
  **link**, never per publisher, so a link-level number under a stream's key would be
  unattributable. But the two failure modes are already separable from what the tile reports —
  frame age is 3 502 / 9 085 ms under congestion and 0.77 / 0.78 ms under in-flight loss, three
  orders of magnitude apart with `stats/drops` at zero in both. #719's health panel now names
  the cause, not just the hop.

## Reproducing

```bash
cargo build --release -p zensight-sensor-parallax \
    --bin zensight-sensor-parallax --example media_loss_probe
scripts/media-loss-lab.sh --leg quic --loss 1 --pattern snow --width 320 --height 180 \
    --bitrate 8000 --seconds 45 --out target/media-loss/b-1
scripts/media-loss-report.py target/media-loss/b-1
```

Needs root for `ip netns` / `tc` (the sensor and probe are dropped back to the invoking user
inside the namespace). `--keep` leaves the namespaces standing for inspection.
