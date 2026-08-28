# Why there is no `qos_proportion`, `jitter_ms` or `latency_ms` (#692)

**Decision:** the parallax sensor publishes **no QoS-sourced telemetry** and
attaches **no pipeline bus**. `{stream}/stats/drops` is read from
`AppSinkHandle::stats().total_dropped`, and the `stream_degraded` rule computes
parallax's own `(processed + dropped) / processed` from those counters rather
than receiving it as an `Event::Qos`.

No behaviour is missing that was ever present — this records four things that
*cannot* be sourced, so the next person to read parallax's QoS API and ask
"why aren't we using this?" gets the answer instead of re-deriving it.

Line numbers below are against **`parallax-pipeline 0.8.0` as published**,
which is what `Cargo.toml` pinned when this was written. The pin has since
moved to **0.9.0**, so the line numbers drift; the findings themselves were
re-checked at that bump and still hold — none of 0.9.0's breaks touches an
`AppSink`-terminated graph. The checkout at `/srv/dev/repos/parallax` carries
unreleased work on top and drifts further still.

## What was asked for

#692 asked for four metrics off the pipeline bus: `drops` sourced from
`QosEvent.dropped` instead of inferred, plus new `qos_proportion`, `jitter_ms`
and `latency_ms` subjects, with fps/kbps re-sourced from the encoder's
`RateMeter`.

## Why none of it can be sourced

**QoS events have no origin in our graphs.** `MessageKind::Qos` reaches the bus
at exactly one place — `unified_executor.rs:4199-4201`, mirroring what a sink
hands back from `take_upstream_event()`. The trait default
(`element/traits.rs:836`) returns `None`, and the only implementations that
return `Event::Qos` are `AppVideoSink` (`elements/app/appvideosink.rs:388`) and
`AutoVideoSink` (`elements/app/autovideosink.rs:643`) — both *display* sinks.
**Every profile this sensor builds terminates in `AppSink`**, which does not
override it. So `qos_proportion` and `jitter_ms` have no producer, and `drops`
cannot come from a QoS event.

**No element declares a latency.** `Element::latency()` is overridden only by
`RtpJitterBuffer` (`elements/rtp/jitter_buffer.rs:480`) and `AutoVideoSink`
(`:669`). Neither is in any graph here — the RTSP path uses `RtspSession`'s
internal depacketizer, not a standalone jitter buffer — so
`Pipeline::query_latency()` returns `None`, `MessageKind::LatencyChanged` is
never posted (`unified_executor.rs:1318-1325`), and `PipelineHandle::latency()`
is `None`. There is no value to publish.

**No other element exposes a drop count.** `Throttle::stats()` takes `&self` on
an element the executor *moves* into its task at `start()`; the only cloneable
handle is `ThrottleControl`, which carries the interval and nothing else.
Link-level `LinkPolicy::DropNewest`/`DropOldest` expose no counters — and every
link here is `Block` anyway, deliberately (see `pipeline.rs`'s header).

**fps and kbps stay egress-sourced**, for the reasons #510 already wrote down in
`streams.md`: egress counts what actually crossed Zenoh, injected SPS/PPS
included and sink-shed frames excluded, and an RTSP passthrough has no encoder
to ask at all. Re-sourcing them from `EncoderStatsHandle`'s `RateMeter` would
reverse that decision rather than improve it.

## Why the bus is not attached either

`PipelineHandle::take_bus()` would hand us a `Bus` carrying `Error`, `Warning`,
`StateChanged` and `Eos` — never `Qos`. None of it is worth a second channel:

- **`Error` is strictly redundant, and would be worse.** `EndReason::Error(StreamError)`
  already reaches `egress.rs` on the pull, and #691 types it all the way to
  `StreamStatus.last_end`. A bus copy would deliver the same fact on a
  *different schedule* — drained on a tick, versus delivered on the pull — which
  is two orderings for one event.
- **`StateChanged` and `Eos` we already know.** We call `start()`, we own the
  stop switch, and EOS arrives as `Pulled::Ended(EndReason::Eos)`.
- **`Warning` is empty for us.** Every `post_warning` site is a seek failure (we
  never seek), a `finish_timeout` expiry (an `AppSink` has no finish work to
  time out), or buffering. The one warning that *would* matter — arena-exhaustion
  shedding — is not a bus post at all: `ShedTracker::record` logs and calls
  `observability::record_buffer_dropped`, which goes to the `metrics` crate's
  global recorder, and nothing in this workspace installs one.

An attached-but-undrained bus is a queue growing behind a live pipeline, so
"attach it now, use it later" is not free either.

## What we publish instead

`AppSinkHandle::stats()` is readable on a running pipeline — upstream calls
`total_dropped` "the number a live consumer actually cares about"
(`elements/app/appsink.rs:601-606`) — and every sink here is built
`drop_on_full(true)`. From it:

- **`stats/drops`** = `total_dropped`, folded as a delta per profile
  incarnation. This is not a substitute for what #692 wanted; it is *the number
  this metric was always documented to be*. The sequence-gap inference it
  replaces was a proxy for the sink's own counter, and a worse one — blinded
  across every RTSP reconnect by the DISCONT reset, absent on the preview path,
  and structurally zero on RTSP passthrough (see below).
- **`stats/sink_queue`** = `queued_buffers`, the backlog that precedes shedding.
- **`stream_degraded`** = `(published + shed) / published` over one stats
  interval, which is `QosEvent::proportion`'s own definition computed at the one
  place we can compute it.

There is no fourth number hiding in `AppSinkStats`:
`total_received - total_pulled - queued_buffers` is identically **zero**, not a
third view. A shed buffer never increments `total_received`, and the two
`clear_queue` paths (`AppSinkHandle::clear`, `Event::FlushStart`) are unreached
here. `an_unpulled_sink_sheds_and_says_so` in `pipeline.rs` pins that as an
invariant, which is what justifies deleting the gap counter rather than
publishing two numbers for one thing.

**Note what saturation costs.** Because every link is `Block` with a shallow
channel, shedding does not begin until the *whole* chain has filled —
source → convert → scale → throttle → encoder → sink. Measured on the 8 fps test
source, that is about **three seconds**, which is also how long a real stall
takes to appear in `stats/drops`.

## Blind spots this does not close

- **Transport loss is upstream of every counter here.** #713 measured
  `stats/drops` at **0** while 83 % of sequence numbers never arrived: the
  frames died in Zenoh's own transport queue. Nothing on this page changes
  that, and the tier ladder — sending less — remains the answer. See
  `docs/plans/adaptive-media/loss-measurement.md`.
- **Frames the V4L2 driver never delivered.** `V4l2Src` stamps the driver's
  `meta.sequence`, which gaps on driver-level loss, but the encoder re-stamps
  downstream from its own emitted count, so nothing observes it.
- **`RtspSession` never stamps `Metadata::sequence` at all**, so on the RTSP
  passthrough path `FrameMeta.sequence` is a constant 0 on the wire. That also
  makes consumer-side `MediaReceiverReport.lost_frames` blind there. It is a
  bug in its own right, on #714/#715's surface rather than this one, and wants
  its own issue.

## What would change this

Any one of these makes the bus the only route and reopens the decision:

- upstream giving `AppSink` a `take_upstream_event` that originates QoS — it is
  the natural origin, since it is the element that knows both its lateness and
  what it shed;
- an `AutoVideoSink` or `AppVideoSink` appearing in a graph here;
- any element declaring `Element::latency()` — which we would get for free if we
  ever depacketized RTP ourselves instead of using `RtspSession`'s internal
  path;
- a graph here needing a leaky `LinkPolicy`, which would shed frames no counter
  can see and would require `stats/drops` to gain a second source before the
  link could land.
