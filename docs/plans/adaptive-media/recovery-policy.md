# The latency-aware recovery policy (#721)

Why this stack does not do what WebRTC does about loss. Written once, so the next NACK or FEC
proposal can be answered with a link instead of a re-derivation.

The durable half of this — the rule and the numbers — belongs to
[`zensight-sensor-parallax/docs/streams.md`](../../../zensight-sensor-parallax/docs/streams.md);
this file keeps the working-out and the measurements it rests on.

## The rule

```
if estimated_repair_time < remaining_frame_lifetime:
    repair may be worth it
else:
    drop, and request a keyframe
```

A frame's lifetime on a live monitoring surface is the frame-age deadline from #716 — default
**1500 ms**, and an operator watching a camera for a reason will set it far lower. A repair
costs at least one round trip, plus the time to notice the loss, plus queueing on a link that
is already the reason the loss happened.

| link | RTT | one repair | frames that fit in a 1500 ms budget | verdict |
|---|---|---|---|---|
| LAN | 0.1–1 ms | ~2 ms | ~700 | repair is affordable |
| VPN / WAN | 20–80 ms | 40–160 ms | 9–37 | affordable, marginal at the tail |
| RF (`zenoh-modem`) | 200 ms – 2 s | 0.4–4 s | 0–3 | **too late** |
| SAT store-and-forward | seconds to minutes | seconds to minutes | 0 | **not a repair, an archive** |

The rows that matter are the bottom two. The sibling
[`zenoh-modem`](https://git.marcpardo.eu/marcpardo/zenoh-modem) carries Zenoh over RF and
satellite store-and-forward at a **220-byte MTU**, and those are the links this project exists
to reach. On them a retransmitted frame arrives long after anything would have wanted it, and
the bandwidth it consumed was taken from frames that had not yet expired. Repair on a
store-and-forward link is not slow recovery; it is a second copy of a dead frame, charged to
the living ones.

## So v1 is drop-stale plus request-keyframe, and nothing else

Both halves already exist and are load-bearing:

- **Drop stale** — the frame-age deadline (#716). A late access unit is shed rather than
  decoded, because decoding it produces a picture of the past at the cost of the present. A
  late *keyframe* is always decoded: shedding those too leaves a slow link showing nothing at
  all.
- **Request a keyframe** — `StreamControl::RequestKeyframe`, coalesced at the sensor, with a
  `RESYNC_MIN_INTERVAL` backoff on the viewer. One keyframe repairs *every* outstanding loss at
  once and costs one round trip regardless of how many frames were lost, which is precisely the
  property retransmission lacks.

The backoff is not decoration. #435 was a keyframe storm, and #716's own review found it
rebuilt out of new parts: the backoff was being cleared by the keyframe it had asked for, so
requests were paced by decoded keyframes rather than by time. On a link too slow to keep up,
the steady state was an all-intra stream — the most expensive possible response to congestion.
Any future recovery mechanism inherits that lesson: **recovery must be paced by wall-clock, not
by its own success.**

## What Zenoh's layer does and does not mean

`QosClass::LiveVideo` sets `Reliability::BestEffort` + `CongestionControl::Drop`. The names
promise more than the shipped deployment delivers, and #713 measured exactly how much.

**On `tcp/` — every config in `configs/` — best-effort cannot lose a sample in flight.** It only
permits the sender to drop under congestion. But the drops are real and they are *invisible*:
at 300 kbit against ~1.7 Mbps offered, the receiver missed **83 %** of sequence numbers with
`stats/drops` at **0**, and frame age reached **3.5 s median**. The frames died in Zenoh's
transport queue, upstream of every counter the sensor publishes
([#801](https://git.marcpardo.eu/marcpardo/zensight/issues/801)). So on today's deployment the
thing recovery would be repairing is *congestion*, and the repair for congestion is to send
less — a lower tier — not to send the same frame twice.

**On `quic/…?mixed_rel=1` best-effort really does ride unreliable datagrams**, and loss is
amplified by access-unit size: at 1 % packet loss a 34 KB access unit was lost **20 %** of the
time, a 136 KB one **41 %**. Full numbers in
[`loss-measurement.md`](loss-measurement.md).

## No retransmission, no FEC — and the exact conditions that would reopen it

Neither is refused on principle. Each has a condition, and #713 supplies the evidence to test
it against:

**FEC becomes interesting only if in-flight loss turns out to be concentrated on keyframes.**
It is not, yet — the measurement shows loss tracking access-unit **size**, not frame *type*.
Keyframes are more fragile only in so far as they are bigger, and on the synthetic sources used
they were only 1.4×–4.4× the size of a delta. A real camera's IDR is 10–30× its deltas, which
is where the effect would show; measuring it needs a real camera on a real lossy link. If it
does show, FEC across an IDR's fragments is the right shape of answer, because the repair is
in-band and costs no round trip — the one property that survives the table above. Parity across
a *sequence of frames* is not: it costs the latency of the frames it spans.

**Selective retransmission becomes interesting only if a genuinely short-RTT link turns out to
be genuinely lossy.** On a LAN, ~700 frame lifetimes fit inside one deadline, so a NACK is
affordable. But #713 could not produce in-flight loss on any `tcp/` configuration we ship, and
the QUIC leg that could is not a deployed profile. Until a shipped configuration is both fast
and lossy, retransmission would be code with no traffic to serve.

**And one thing that is settled, not open:** repair must never be attempted on a shared tier's
behalf. RFC 07 §1.2 is normative — a producer must not re-tune a shared tier from one consumer's
report. A per-consumer repair channel is a tier of its own, with its own cost, and that is the
escape hatch to reach for rather than a NACK on the common stream.

## The short version, for a reviewer

> Repair is worth it only while it can beat the frame's deadline. On the links this project
> targets it cannot, and on the links where it could we have never observed the loss it would
> repair. What we do instead is shed what is late and ask for one keyframe, paced by the clock.
> Reopen it with a measurement: keyframe-concentrated in-flight loss (→ FEC across an IDR's
> fragments), or a short-RTT link that is genuinely lossy (→ selective retransmission).
