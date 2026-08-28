# Streams, tiers, and pipelines

## The model

One **catalogue entry** (= one `<stream>` key chunk) can be open in several
**profiles** at once, each an independent parallax pipeline with its own
publisher, egress task, and refcount:

| Profile | Key | Encoding | Purpose |
|---------|-----|----------|---------|
| video tier | `zensight/v1/<origin>/@media/parallax/<stream>/video/h264/<tier>` | `video/h264` | one bandwidth tier of the live view |
| `preview` | `zensight/v1/<origin>/@media/parallax/<stream>/preview/jpeg` | `image/jpeg` | low-fps GUI tiles |

**Demand-driven tiered simulcast (#494).** The video plane is a *ladder* of
bandwidth tiers (`low` / `medium` / `high` by default — see
`docs/configuration.md`). Each tier is a **separate** H.264 pipeline published
concurrently on its own `<tier>` key with its own resolution/fps/bitrate. A
viewer subscribes to exactly the one tier its link can take, so two viewers on
different links — one on `…/video/h264/low`, one on `…/video/h264/high` —
render independently and neither perturbs the other's bitrate. That is the
thing a single shared encoder structurally cannot do (one encoder,
last-writer-wins on resolution).

An `open_stream` command with `codec: "h264"` (or the sensor default) and an
optional `tier` opens that video tier; `codec: "mjpeg"` opens the preview.
An open with no `tier` resolves to the sensor's `default_tier`. Each
(stream, tier) and the preview is refcounted per open command and reaped when
its refcount is 0 **and** its publisher has had no matching subscribers for
`idle_timeout_secs` (the matching listener is the crash backstop for GUIs that
die without `close_stream`).

**Viewer keys are exact.** Keyspace v1.3 revoked the
`…/@media/parallax/<stream>/video/h264/*` wildcard licence (RFC 07 §3): the
catalogue advertises which tiers a stream offers (`StreamDescriptor.tiers`), and
a viewer subscribes to exactly one `<tier>` key. A `*` here would pull every
tier at once — the opposite of demand-driven. The per-tier matching listener
counts a subscriber against that tier alone (pinned in `tests/e2e.rs`:
*two viewers on distinct tiers stream independently*). See `docs/KEYSPACE.md`.

Separate pipelines per profile/tier — deliberately **no tee**: the preview must
keep its cadence whether or not any encoder runs, and closing one tier must not
disturb another. The cost (one device open per tier on V4L2) is a
documented limitation, see below; a shared-capture fanout (#508) is the
deferred optimisation.

## Pipeline shapes per source kind

The video path stamps geometry into the pipeline `Metadata` and inserts a
scaler + throttle before the encoder, so the tier's resolution and framerate are
enforced in-pipeline (parallax 0.6: encoders/scaler take **no** dimensions at
construction — geometry travels in the data):

| Source | video tier (h264/`<tier>`) | preview profile (jpeg) |
|--------|----------------------------|------------------------|
| Test | `VideoTestSrc` (Rgb24, live) → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `VideoTestSrc` (preview fps, live) → `VideoScale` → `JpegEncoder`(Rgb) → `AppSink` |
| V4L2 MJPG | `V4l2Src` → `JpegDecoder` → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `AppSink` (MJPG passthrough — no scaler, `preview.max_height` does not apply) |
| V4L2 YUYV | `V4l2Src` → `VideoConvert`(→I420) → `VideoScale` → `Throttle` → `H264Encoder` → `AppSink` | `V4l2Src` → `Throttle` → `VideoScale`(Yuyv) → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |
| RTSP H.264 | `RtspSession` → `AppSink` (**passthrough** — no re-encode, no scale) | `RtspSession` → `H264Decoder` → `Throttle` → `VideoScale`(I420) → `VideoConvert`(→Rgb) → `JpegEncoder` → `AppSink` |

Notes:

- `VideoScale` is **aspect-preserving and never upscales** — a tier whose
  `max_height` exceeds the source stays at the source height (so the catalogue
  only *offers* a tier the camera can actually feed; a 360-high source offers
  `low` + `high`, not `medium`, since 480 would upscale).
- Framerate limiting to the tier fps uses parallax's `Throttle` element
  (drop-based). The delay-based `RateLimiter` is never used — it would
  backpressure a live source. Test-source previews are built directly at the
  preview fps.
- The H.264 encoder runs in `RateControlMode::Bitrate` with `skip_frames(true)`
  so the tier's `bitrate_kbps` is a real cap (OpenH264 silently overshoots a
  bitrate target with skip-frames off). Note *how* it is held: on input the
  encoder cannot compress further, it does not shrink each frame — it **sheds
  frames**, so the cap moves throughput rather than frame size. `tests/e2e.rs`
  pins this subscriber-side (*the ladder's bitrate cap bites on the wire*): two
  rungs identical but for `bitrate_kbps`, on per-pixel noise, emit ~48 kB and
  ~61 kB per access unit respectively — but 3 and 12 of them in the same window.
  `{stream}/stats/rc_drops` is what the shedding looks like from inside (#510).
- JPEG previews are always flagged `keyframe: true` in `FrameMeta` (every
  JPEG is independently decodable). The preview scaler caps the thumbnail at
  `preview.max_height` on every path that re-encodes; the one exception is
  the V4L2 MJPG passthrough, which forwards the camera's JPEG bytes verbatim
  at whatever size the camera produces. The scaler sits before the RGB
  convert (still YUYV/I420), so convert + JPEG run on the capped size.

## Encoder shaping per tier (#509)

The ladder's `max_height` / `fps` / `bitrate_kbps` say what a tier *delivers* —
they are on the wire, because a viewer picks a tier by them. How the encoder
reaches those numbers is a separate, **sensor-local** set of knobs:
`video.encoder` for the defaults, a per-tier `encoder` block to override any
field. See [`configuration.md`](configuration.md) for the table.

They are deliberately not on the wire. `TierSpec` rides the catalogue inside
`StreamDescriptor`, which is a derived entry in the fleet-wide `SchemaSet` every
producer serves on `@rpc/<producer>/describe` (RFC 08 §7) — so an encoder
implementation detail would become a bus contract, and `zensight-common` would
need serde mirrors of the codec crate's enums to carry it. Nobody subscribes by
entropy coder. The sensor owns the numbers; the wire carries the name.

**Everything ships unset**, and each knob is applied only when set, so an unset
knob is OpenH264's own default *by construction* rather than by a copy of it
that can drift. Two of them are worth reading before you reach for them:

- **`profile`.** The frontend decodes with OpenH264 too, so a profile its
  decoder cannot read would be a self-inflicted outage. All three profiles are
  pinned by a test that runs the encoder output through the GUI's exact decode
  path (`every_profile_the_ladder_can_name_decodes_like_the_gui`); keep that
  gate green before shipping a default.
- **`max_slice_len`** caps each NAL near the path MTU. The usual reason to want
  that is fragmentation: a large keyframe NAL split across IP fragments loses
  the whole IDR when one fragment drops, whereas MTU-sized slices lose one
  slice. **That reasoning does not apply to this egress.** The media plane
  publishes one *whole access unit* per Zenoh sample at `QosClass::LiveVideo`
  (best-effort, drop-on-congestion), so a lost sample costs the entire AU
  whether it was one slice or twenty — there is no RTP payloader in the path.
  What slicing does cost today is a slice header per ~1200 bytes on whichever
  tier has the tightest budget, plus OpenH264's `SM_SIZELIMITED_SLICE`
  threading constraint. It is wired and tested so it is *ready*; turn it on when
  something downstream packetises (an RTP/WebRTC gateway), or when a decoder
  doing slice-level concealment is on the other end.

`threads` and `sps_pps_strategy` are deliberately not exposed: parallax
auto-detects a thread count and a per-tier thread budget needs a host-wide
story, while OpenH264 writes the parameter sets into every IDR under every
strategy (the strategy only renumbers ids) and the egress's
self-contained-keyframe guarantee is derived from the bytes regardless.

## Why there is no live re-tune command

There isn't one, and that is the design — not an omission.

Per-viewer quality is expressed by **which `<tier>` key you subscribe to**
(#494), and redefining what a tier *means* is config-only (#513). `StreamControl`
carries `open_stream` / `close_stream` / `request_keyframe` and nothing else;
there is no `SetVideoParams`, no `tiers/set`, and no registry entry for either.
To change the ladder, edit `configs/parallax.json5` and restart the sensor.

The elements *do* expose live control handles, and `PipelineControls` clones
every one of them before the executor starts — that mechanism is real and
load-bearing, because it is the only way to reach an element after
`Executor::start()` moves it into its task, and it is how keyframe forcing works
at all (below). But the session actor drives **only the keyframe handle**. The
bitrate, scaler, throttle and preview-quality handles are cloned and unused.
They are kept because the clone must happen at construction or not at all, so a
future retune path cannot be added later without them.

Making one of them reachable is not a small change: it needs a new
`StreamControl` variant, a registry and schema entry for it, and a rate-limit
story — a bitrate change is seamless (OpenH264 `SetOption`), but GOP and
resolution rebuild the inner encoder and emit a fresh IDR, so a slider wired
straight through would re-key the stream on every pixel of travel. None of that
exists today.

*(This section previously described those knobs as working. They never did;
#504 corrected it — the same class of defect as #513's phantom `tiers/set` and
#479's phantom payload type.)*

## Keyframe control

The H.264 encoder's `KeyframeHandle` is cloned **before** the encoder is
consumed by the pipeline (it is unreachable once running). A keyframe is
forced when:

- the tier publisher's matching listener sees a viewer appear (rising edge), or
- a `request_keyframe` command arrives (explicit recovery). The command carries
  the `tier` to re-key; the sensor forces an IDR on that tier's encoder alone.

RTSP video is passthrough — the sensor cannot force a remote camera's IDR, so
`request_keyframe` logs and no-ops; viewers instead gate on the in-band IDRs
(`FrameMeta.keyframe`, GOP-rate).

### RTSP reconnect (#410, #731)

The connected `RtspSession` **is** the graph's source (`add_async_source`),
not a hand-written task shovelling frames into an `AppSrc`, so the retry loop
is upstream's and runs inside `produce()`: exponential backoff from 500 ms to a
30 s ceiling, with full jitter so a rack of cameras behind one switch does not
retry in lockstep.

Two consequences worth knowing:

- **A clean end is retried too.** RTSP has no in-band end-of-stream for a live
  stream, so a server whose process dies looks exactly like one that finished.
  Configuring a reconnect policy *is* the statement "this source is live", and
  every source in our catalogue is a live camera. A finite stream — a recording
  served over RTSP — would want `.without_reconnect()` instead; nothing in
  `configs/parallax.json5` can configure one today.
- **The ladder is bounded** (8 attempts, ≈ 90 s) even though upstream's default
  is "retry forever". Forever would mean a camera that is *gone* never produces
  an error, so `rtsp_connect_failed` could never fire again after the initial
  connect and a dead stream would sit silently "open". Exhausting the ladder
  fails the pipeline, which is what turns sustained failure back into an alert.

The first buffer after a successful reconnect carries `BufferFlags::DISCONT`.
The egress re-arms on it: the cached SPS/PPS belong to the *previous* session,
so replaying them in front of the resumed stream's first keyframe could hand a
decoder a geometry the bytes no longer match. The cache is cleared and refills
from the camera's own in-band sets, and sequence-gap accounting restarts.

### Self-contained keyframes (#435)

The video egress guarantees, at the byte level, that every access unit it
publishes with `keyframe: true` is a **self-contained decoder entry point**:

- the flag itself is derived from the bitstream (an IDR NAL is present), not
  from upstream pipeline metadata — raw sources flag every uncompressed frame
  as a sync point, and parallax < 0.1.3 leaked that through the encoder,
  sending fresh decoders into an unrecoverable `dsNoParamSets` loop;
- the egress caches the last SPS/PPS NAL units it has seen for the stream and
  prepends them to any keyframe AU that arrived without its own (relevant for
  RTSP passthrough cameras that announce parameter sets only out-of-band in
  the SDP; the OpenH264 encoder paths already inline SPS/PPS with every IDR).

An RTSP keyframe that arrives before *any* in-band parameter sets have been
seen is published as-is — there is nothing to prepend yet.

The extract/cache/prepend logic itself is `parallax::codec::annexb`'s
`ParamSetCache` (#730), not ours: it is codec-aware (H.265's two-byte NAL header
included, where our `& 0x1F` returned nonsense) and returns the input slice
borrowed for every delta frame and every keyframe that already carries its sets,
so only a genuinely repaired keyframe copies. `zensight-sensor-parallax`'s own
`annexb` module is down to one helper with no upstream equivalent,
`coded_slice_count`, which exists to prove `encoder.max_slice_len` (#509)
reached OpenH264.

A stream's H.264 `profile-level-id` (`avc1.<6 hex>`, what a WebCodecs client
configures a decoder with) is **not** on the catalogue and not in `FrameMeta`.
The catalogue is built from config at startup and answers for closed streams,
while the value only exists once a keyframe has been encoded — it would be
`None` in exactly the case a viewer consults the catalogue for. A consumer
derives it from the first keyframe instead, which is possible precisely because
of the parameter-set promise above; `annexb::h264_profile_level_id` is the
three-byte read that does it (#707).

### Recovery: drop stale, ask for one keyframe, and nothing else (#721)

The keyframe request above is the *whole* loss-recovery mechanism. There is no
retransmission and no FEC, and that is a decision with a measurement behind it
rather than an omission.

```
repair is worth it only while  estimated_repair_time < remaining_frame_lifetime
```

A frame's lifetime on a live surface is the viewer's frame-age deadline (#716,
default 1500 ms and usually set lower). A repair costs at least one round trip.
On a LAN that budget holds hundreds of frame lifetimes; on the RF and satellite
store-and-forward links the sibling `zenoh-modem` carries Zenoh over — a 220-byte
MTU, RTTs from hundreds of milliseconds to minutes — it holds none, and a
retransmitted frame arrives long after anything wanted it, having spent
bandwidth belonging to frames that had not yet expired.

One keyframe, by contrast, repairs *every* outstanding loss at once and costs
one round trip however many frames were lost. That is the property retransmission
lacks, and it is why the request is paced by wall-clock backoff
(`RESYNC_MIN_INTERVAL`) and never by its own success: #435 was a keyframe storm,
and pacing recovery by decoded keyframes rebuilds it, turning a link that is
already too slow into an all-intra stream.

What the measurement (#713,
[`docs/plans/adaptive-media/loss-measurement.md`](../../docs/plans/adaptive-media/loss-measurement.md))
adds is that on a `tcp/` deployment the thing recovery would repair is
**congestion**, not loss: at 300 kbit against ~1.7 Mbps offered, 83 % of
sequence numbers never arrived while `stats/drops` stayed at 0 and frame age
reached 3.5 s median — the frames died in Zenoh's transport queue, upstream of
every counter published here (#801). The repair for that is to send less, which
is what the tier ladder is for. On `quic/…?mixed_rel=1`, where best-effort does
ride unreliable datagrams, loss scales with **access-unit size**: 20 % of 34 KB
access units lost at 1 % packet loss, 41 % of 136 KB ones — again pointing at a
smaller tier rather than at repair.

The two conditions that would reopen it, both stated so they can be tested:
FEC across an IDR's fragments becomes interesting if in-flight loss turns out to
be concentrated on keyframes (it tracks size, not frame type, on every source
measured so far); selective retransmission becomes interesting if a link with a
genuinely short RTT turns out to be genuinely lossy. Neither is a shared-tier
decision in any case — RFC 07 §1.2 is normative, and a per-consumer repair
channel is a tier of its own.

## Frame metadata

Every media sample carries a CBOR `FrameMeta` attachment
(`zensight-common::stream::FrameMeta`): keyframe flag, optional
pts/dts/duration (ns), per-stream sequence, width, height. On the h264 video
path width/height are the **tier's encoded (post-scale) dimensions**, so a low
tier reports 240-high frames while the high tier reports native height on the
same source. Sequence gaps mean dropped frames (LiveVideo QoS is best-effort by
design).

Two encoding rules are normative wire shape rather than compression, and a
viewer may rely on both:

- **Absent is not null.** A timing field the pipeline never stamped is *missing
  from the CBOR map*, not present-and-null.
- **`dts_ns` is omitted when it equals `pts_ns`** — the field means "decode
  timestamp *if distinct*". Our encoders emit no B-frames, so in practice `dts`
  equals `pts` on essentially every frame and the field is simply absent; a
  decoder that wants a value reads `dts_ns.or(pts_ns)`.

`parallax::wire::FrameMeta` is a byte-compatible twin of the zensight type
(#711: two types, one corpus — neither crate can depend on the other). The
binding artifact is a set of canonical CBOR vectors checked into both repos;
ours live in `zensight-common/tests/fixtures/framemeta/` and are pinned by
`zensight-common/tests/framemeta_corpus.rs`.

## Teardown

`close_stream` (with the tile's `codec` + `tier`) decrements that profile's
refcount; at 0 the profile enters the idle countdown (unless a viewer is still
subscribed). The idle reaper stops the pipeline, aborts the egress and
matching-listener tasks, and undeclares the publisher. A viewer *edge* (appear /
leave) also republishes the per-stream `StreamStatus` so the state plane
reflects each tier's live viewer count. Closing the last profile marks the
stream inactive in the catalogue and publishes a `StreamStatus{open: false}`
transition on `state/parallax/stream/<stream>` (the doc is only tombstoned when
the stream leaves the config, never on close).

> A codec-less `close_stream` resolves to the sensor's **default video tier**
> (see `resolve_profile`) — so a preview or a non-default tier must name its own
> `codec`/`tier`, or the wrong refcount is decremented. The GUI's per-tile
> `close_control` does exactly this.

A profile that was opened but never gets (or loses) its viewer is reaped by
the same countdown even if its refcount is non-zero — the matching listener
is the crash backstop for GUIs that die without `close_stream`, and an opener
that never subscribes is a zombie.

### Why it stopped (#691)

`StreamStatus.last_end` carries the producer's own account of the most recent
tier to stop: `{ tier, reason }`, where `tier` is a ladder rung's name or the
literal `preview`. **The producer is the only party that knows.** A viewer sees
`open: false` and its own subscriber ending, and from those two facts it used
to invent a sentence — *"stream ended"* — that was wrong as often as it was
right, because an idle reap, an operator close and a dead camera were the same
single bit.

Eight reasons in three families:

| Family | Reason | What happened |
|---|---|---|
| we ended it | `closed` | every opener called `close_stream` |
| | `idle` | reaped unwatched while an opener still held a reference |
| | `superseded` | released so another tier could open |
| | `shutdown` | the producer is stopping |
| it ended itself | `source_ended` | the source signalled EOS. A live camera should never do this — if one does, that *is* the finding |
| it failed | `stalled` | started, but delivered no frame within the first-frame window |
| | `failed` | an element failed, carrying `node` and the element's own `message` |
| | `failed_open` | the open never completed |

Two rules the field lives or dies by:

- **Absent means nothing has stopped since this stream last opened**, never
  "stopped for an unknown reason". It is `skip_serializing_if`'d out of the map
  entirely rather than sent as null, the same discipline `FrameMeta` and
  `MediaReceiverReport` follow. A reopen on the same tier clears it; a
  *sibling's* end survives, because that is still the truth about the sibling.
- **A `failed` message is the element's own words.** `node` and `message` stay
  two fields on the wire and only `Display` ever joins them, so a consumer can
  show the message and act on the element separately.

`closed` and `idle` deserve a note, because they are the same code path. A
`close_stream` does not stop a pipeline — it releases a refcount, and the idle
countdown does the stopping — so a clean close and the crash backstop both
arrive through the reaper, one idle window later. The refcount *at reap time*
is the honest discriminator: zero means every opener closed and the system did
what it was told; non-zero means nobody ever subscribed or a viewer died
without saying goodbye, and the system is cleaning up after something that
vanished. Those are different events with different follow-ups.

`aborted` is deliberately **not** in the table. `PipelineHandle::stop()`
produces `Eos` and `abort()` produces `Aborted`, and an `AppSink` never sees
`Aborted` at all — so our own teardowns are named by *us*, from the path that
initiated them, never inferred from the pipeline's report.

**Failed opens** publish a definitive `StreamStatus{open: false}` transition
carrying `last_end.reason = failed_open` with the failing layer's own message
(the GUI shows that on a still-waiting tile instead of guessing), record a
device failure, and
drop the stream's stats entry — a leaked entry would publish phantom
zero-valued stats forever. All open-failure paths (pipeline build, media
publisher declare, matching listener declare, pipeline start, RTSP connect)
funnel through the same cleanup exit.

**Dead-profile reopen**: `open_stream` for a profile whose egress already
ended (its `EgressEnded` still queued behind the command) tears the dead
pipeline down and builds a fresh one instead of refcounting a corpse; the
queued stale `EgressEnded` is recognized by its epoch stamp and ignored, so
it cannot kill the replacement. The successful rebuild clears that tier's
`last_end` and leaves a sibling's alone.

**Stopping a live pipeline**: teardown calls `PipelineHandle::stop()` before
`abort()`, and the order is the point. `stop()` raises the executor's
cooperative wind-down flag; every source loop checks it at the top of its next
iteration, ends like a natural EOS, and drops its device on the way out — so
the whole graph unwinds within one frame period, and `wait()` would report
`Eos`. `abort()` raises the same flag but cancels the tasks in the same breath,
which is fine for the async plumbing and not fine for a source still holding an
exclusive V4L2 device that the incoming tier is about to open (see
*exclusive-source tier switch* above). `Drop` on the profile asks too, however
the session dies.

> This used to be a `StoppableSource` wrapper around every synchronous source,
> because parallax once ran source loops on blocking threads that `abort()`
> could not cancel. None of that has been true since 0.8: sources run as
> ordinary tokio tasks with `produce()` called inline, the loop polls the
> shutdown flag itself, and `abort()` raises that flag before it cancels
> anything. The wrapper was retired in #709 — a forwarding `Source` impl is a
> standing hazard, since every method upstream adds and we forget to forward is
> answered by the wrapper instead of the source (which is exactly how
> `set_output_budget` was swallowed in #689).

## Stats, health, alerts

Per-stream stats ride ordinary telemetry under
`zensight/v1/<origin>/telemetry/parallax/<stream>/stats/<metric>` every
`stats_interval_secs`, **aggregated over the stream's open profiles**:

| Metric | Kind | Meaning |
|--------|------|---------|
| `fps` | gauge | frames published per second (all open tiers + preview combined) |
| `kbps` | gauge | total media bandwidth published for the stream |
| `drops` | counter | frames lost between encoder and egress (video-tier sequence gaps; intentional preview throttling is never counted) |
| `rc_drops` | counter | frames the encoder's rate control swallowed to hold the tier's `bitrate_kbps` (omitted for streams with no rate-controlled encoder) |
| `viewers` | gauge | open profiles with matching subscribers (0 .. tiers + 1) |
| `encode_ms` | gauge | average wall time per encoder `process()` call (omitted when no encoder ran) |

## Receiver feedback (#714, #715)

Consumers report how the stream is *arriving* on
`@rpc/parallax/stream/report` — a `MediaReceiverReport`, `idempotent = true`,
with the consumer's id **in the payload and never in a key** (RFC 07 §1.1). N
viewers are N callers of one key, so the whole feedback surface costs no
keyspace at all.

The sensor keeps the latest report per `(consumer_id, stream, tier)`, bounded
and aged out on **the tier reaper's own window** (`idle_timeout_secs` — one
field, two readers, because a browser tab that closes never says goodbye), and
publishes the fold per tier:

| Metric | Kind | Meaning |
|--------|------|---------|
| `rx/{tier}/consumers` | gauge | consumers with a live report. A **lower bound** on viewers: one that never reports is invisible here — which is why this does not replace `stats/viewers` |
| `rx/{tier}/loss_pct_{max,p50}` | gauge | worst and median `lost/(received+lost)` across live reports |
| `rx/{tier}/frame_age_ms_{max,p50}` | gauge | worst and median frame age — *observed skewed latency*, negatives published unclamped; **omitted** when no consumer measured one |
| `rx/{tier}/decode_queue_{max,p50}` | gauge | worst and median decoder queue depth; **omitted** when no consumer has a queue |

Four things about that table are contract rather than preference:

- **`{tier}` is a required chunk.** "Per-tier worst case and median" cannot be
  spelled without it, and a stream-level average would hide the case the whole
  adaptive-media epic exists for: one tier healthy, another not.
- **A max *and* a median, never one number.** A worst-of-medians understates
  and a median-of-instants is noise, so the report carries both and the
  aggregate folds each on its own terms.
- **The timing families are omitted, never zeroed**, exactly like `rc_drops`
  above and for a stronger reason: RFC 07 §1.3 makes *"unstamped is not asked,
  never zero"* normative. A `frame_age_ms` of `0` tells a controller the stream
  is perfectly fresh at the moment nobody knows.
- **This producer never re-tunes a tier from a report** (RFC 07 §1.2,
  normative). Feedback informs; it does not command. Two viewers share a tier,
  one reports loss, and if the bitrate dropped the *healthy* viewer's picture
  would degrade for a reason it cannot see, caused by a peer it does not know
  exists. The sanctioned adaptation is the **consumer** changing which tier key
  it subscribes to; the escape hatch for a viewer that needs its own rate is a
  tier of its own.

  That is enforced structurally, not by comment: `src/reports.rs` takes an
  `Arc<ReceiverReports>` and nothing else — no `SessionHandle`, no
  `PipelineControls` — so it has no path to an encoder knob.
  `tests/rfc07_receiver_driven.rs` and a CI grep both fail if one appears, and
  `reports_never_retune_a_shared_tier` in `tests/e2e.rs` proves the deployment
  behaves that way with a viewer screaming about 95 % loss.

The reference consumer is the ZenSight frontend's parallax tiles, which report
every 3 s — well inside the ceiling below. What it measures, and what it does
about being late, is
[`zensight/docs/media-receiver.md`](../../zensight/docs/media-receiver.md)
(#716, #717, #718).

Reports are rate-limited to one per second per `(consumer, stream, tier)` —
declared as the procedure's `rate` in the registry and pinned against the code
by `tests/registry_conformance.rs` — and refused with `error/invalid-args` for
a malformed payload, an empty or oversized `consumer_id`, a zero
`interval_ms`, or a selector naming no offered profile. That last refusal is
load-bearing rather than tidy: it is what stops a caller minting tier names and
growing the map without bound. A well-formed but self-contradictory report
(`decoded > received`) is **accepted** — that is a consumer lying about itself,
which the aggregate should show rather than the producer hide.

Three things about that table are easy to get wrong, so they are written down
here (#510):

- **`drops` and `rc_drops` cannot overlap.** The H.264 element numbers its
  output from its own *emitted*-frame count, and a frame the rate controller
  swallows produces no buffer at all — so it leaves no sequence gap for the
  egress task to notice. `drops` is therefore always the `AppSink` shedding
  under a slow consumer; `rc_drops` is always the bitrate cap biting. A tier
  whose `rc_drops` climbs is being held to its `bitrate_kbps`, which is the
  ladder working, not a fault — but it is also the number to read before
  concluding a tier "looks soft".
- **`rc_drops` is omitted, not zeroed**, when a stream has no rate-controlled
  encoder — an RTSP passthrough (no encoder at all) or a preview-only stream
  (JPEG has no rate control). A `0` there would read as "the cap is not
  biting" when the truth is "there is no cap".
- **`fps` and `kbps` stay egress-sourced on purpose.** They count what actually
  crossed Zenoh — the SPS/PPS the egress injects into a bare keyframe included,
  and frames the sink shed excluded — which is the bandwidth an operator is
  paying for. The encoder's own `bytes_encoded` over-reports on both counts, and
  an RTSP passthrough has no encoder to ask. For the same reason `encode_ms`
  remains a mean over whole `process()` calls rather than the encoder handle's
  `last_encode_ns`, which is a single sample of the inner encode.
- **`encode_p95_ms` / `encode_p99_ms` are the tail** (#729), read off parallax's
  own lock-free histogram inside the H.264 encoder. They sit *beside*
  `encode_ms`, not instead of it: the mean is an interval figure the all-time
  histogram cannot give, it covers the whole `process()` call (pending-control
  application, geometry lookup, arena copy, IDR scan) where the histogram times
  only the inner `encode()`, and it is the only encode timing the JPEG preview
  paths have at all. Two properties to know when reading them: they are
  **all-time for that tier's encoder incarnation** (a tail needs history; a 5 s
  window at 30 fps holds 150 samples, of which p99 is one), and they are bucket
  upper bounds — at most 19% high, never low. A stream with several open tiers
  reports its **worst** live tier; the figures disappear when that tier closes.
- **`encoder_overrun` is judged on the tail**, not the mean. A stream whose
  average frame fits the budget while its p95 does not is exactly the one that
  stutters, and overrun is what the rule is named for. The interval mean stays
  the fallback for the JPEG preview paths, which are timed but not
  histogrammed. Because the histogram is all-time, the rule clears more slowly
  than a windowed one would: a bad patch stays in the distribution until later
  frames dilute it or the tier is rebuilt.

For per-tier **applied** resolution/viewers, read the `StreamStatus` doc's
`tiers[]` — that is what the GUI's per-tile bandwidth readout shows. `tiers[]`
is a strict **live set**: a tier that stopped is removed from it, and its end is
reported in `last_end` rather than by retaining a corpse in the vector. Note that
`TierApplied`'s `fps` and `bitrate_kbps` are the tier's configured *targets*
read back, not measurements; only `width`/`height` come from the built
pipeline.

`streams/advertised` (catalogue size) is published every tick regardless of
open streams, so a parallax host appears on the dashboard before anything is
opened.

Health: each successful profile open records a device success for the stream.
Failures — and **only** failures — record one: `stalled`, `failed` and
`failed_open`, i.e. exactly `StreamEndReason::is_failure()`. Three consecutive
flips the stream's device Offline, and the `last_error` it records is the same
sentence the tile shows, because both render the reason through one `Display`.

The producer's **own** teardowns never count. That is structural rather than a
check: `teardown_profile` removes the slot before the profile is torn down, and
the teardown aborts the egress task — so a deliberately stopped profile never
reports an end at all, and a late one is discarded by the epoch guard before it
reaches health.

Alert rules on `state/parallax/alert/*` (auto-resolve on recovery):

- `camera_disappeared` — an advertised V4L2 device vanished from periodic
  re-enumeration.
- `rtsp_connect_failed` — the RTSP camera is not delivering: either the initial
  `open_stream` connect failed, or a stream that had opened dropped and the
  source's reconnect ladder ran out. Since #731 the source retries a dropped
  stream itself, so a single blip no longer fires this — only sustained failure
  does, which is what the rule is named for. It fires on **any** failure end on
  an RTSP source, `stalled` included, and carries the same sentence health
  records.
- `encoder_overrun` — `encode_p95_ms` above the strictest open tier's per-frame
  budget (1000 / fps), falling back to the `encode_ms` mean on a path with no
  encoder histogram (the JPEG previews).

## Limitations

- **V4L2 multi-open**: each video tier and the preview open the device
  independently, so watching two tiers of one camera + its preview is three
  `V4l2Src` opens; most UVC cameras reject the second open (`EBUSY`), and the
  losing pipeline fails with an error status. On such cameras, watch one tier
  at a time, or use an RTSP/test source. A shared-capture fanout that opens the
  device once and scales to every tier (#508) is the planned fix; the current
  design trades that for genuinely independent per-tier pipelines.
- **RTSP is H.264-only and passthrough**: bitrate/GOP/resolution config does
  **not** apply and `max_height` cannot rescale a passthrough tier — the tiers
  a passthrough stream "offers" all carry the camera's own encoding. Each open
  profile makes its own RTSP connection. The connect runs in its own task,
  **never inside the session actor**: the profile slot is reserved as *pending*
  (refcounting opens/closes that race the connect; the stream reads `open` in
  status/catalogue meanwhile), so an unreachable camera (bounded by a 5 s
  timeout) stalls no stream commands, no `@rpc/parallax/streams` replies, and no
  `state/parallax/stream/<stream>` doc updates. If the SDP carries no video
  dimensions, `FrameMeta.width/height` are `0` (= unknown) and the JPEG preview
  cannot be opened (the advertised `FrameMeta`/catalogue size would be a lie).
- **`max_height` is now real** for encoder-backed sources (test patterns and
  V4L2): the inserted `VideoScale` caps the encoded height aspect-preserving
  (never upscaling) per tier. The cap is applied when the tier's pipeline is
  **built** — the scaler's `ScaleControl` is cloned and could retarget it live,
  but nothing does (see "Why there is no live re-tune command"). It is inert
  altogether on RTSP passthrough, where there is no encoder or scaler to drive.
