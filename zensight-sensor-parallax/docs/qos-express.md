# Why the `@media` plane does not set `express` (#733)

**Decision:** the parallax sensor keeps publishing through
`zensight-sensor-core`'s `RawMediaPublisher`, and its `frame` QoS profile leaves
Zenoh's `express` flag **off**. We do not adopt parallax's `ZenohSink::media`.
No behaviour changes — this document records a conflict and which side of it we
are on, because the next person to compare the two tables will otherwise
conclude we have a bug.

## The conflict

Two upstreams describe the same plane and disagree on one flag.

| | congestion | reliability | priority | express |
|---|---|---|---|---|
| zenkey RFC v1.26 M1, `frame` profile | drop | best-effort | interactive-high | **off** |
| parallax 0.8 `ZenohSink::media` | drop | best-effort | interactive-high | **on** |

parallax's side is at `src/elements/network/zenoh.rs:1568` (`sink.express =
true`), reasoned at `:1538`: *"because a stale frame is worthless and the
encoder must never block"*.

Ours is `zensight-common/src/qos.rs`'s `QosClass::express`, which returns
`false` for every media-carrying class including `LiveVideo` (`Alert` is the
one exception — see the carve-out below).

## Why off is right

The two halves of parallax's reasoning are both true and neither of them
implies express. "A stale frame is worthless" is what `CongestionControl::Drop`
and `Priority::InteractiveHigh` are for, and we set both. "The encoder must
never block" is what best-effort plus drop is for, and we set those too.
Express is a third, separate thing: *do not wait to batch this message with the
next one*.

Batching only engages when there is a next message already waiting — that is,
under back-pressure. So:

- On an **unsaturated** link there is nothing to batch with. Express changes no
  timing and buys no latency; it only costs the per-message framing that
  batching would have amortised.
- On a **saturated** link express spends that per-message overhead at exactly
  the moment the link is the bottleneck — and a `drop`-profile publisher's
  correct response to a saturated link is to *shed*, not to transmit each frame
  more expensively.

There is no regime in which it helps, which is why zenkey RFC v1.26 M1 removed
it from the `frame` profile. ZenSight targets constrained links (that is the
premise of the whole QoS table), so the saturated case is not hypothetical.

## Why we do not adopt `ZenohSink::media`

Independently of the flag, the sink is not the right shape for us. Our media
publishers are declared through the sensor framework so that liveliness, the
matching-listener viewer edges, key minting from the registry, and the
`session.put` ban all apply uniformly to every plane a sensor publishes on;
`ZenohSink` is a pipeline element that opens or takes its own session and knows
nothing about any of that. Adopting it to inherit one QoS table we disagree
with would be a poor trade.

## The alert carve-out (2026-08-30, #830)

This document's conclusion is about the **media** plane, and for a while
`QosClass::express()` generalized it to the whole table — including `Alert`,
where zenkey's ratified `alert` profile (RFC 04 §3, "the rare and must-arrive
profile alone, since v1.26") declares express **on**. The conformance judge
holds observed axes against the declared profile, so the first live alert to
cross a doctor window was correctly flagged as `qos-observed-mismatch`.

The generalization, not the RFC, was wrong. Neither half of the media
argument applies to an alert: it is rare (nothing to batch with, and the
per-message framing cost is paid a handful of times an hour, not per frame),
and it is reliable+block (an alert publisher never sheds, so under
back-pressure express moves the one sample that must arrive ahead of the
batch queue instead of into it). `QosClass::Alert.express()` is now `true`;
every other class stays off.

## What would change the media half

A measurement, on a link that matters, showing express reducing end-to-end
frame latency without raising drop rate. Until then, the table — express for
`Alert` alone, off for every other class — is pinned by
`express_is_the_alert_class_alone` in `zensight-common/src/qos.rs` — a named
test rather than an assertion buried in two others, precisely so that
"parallax sets it, so should we" is not a one-line change.
