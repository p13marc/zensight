//! Media stream-control types for the `@media` plane (#359).
//!
//! Only the **pixels** ride `@media`
//! ([`crate::keyexpr::media_video_key`] / [`crate::keyexpr::media_preview_key`],
//! opaque bytes, no serialization envelope). Stream *control* rides the
//! ordinary `@/` channels built with [`crate::command`] helpers:
//!
//! - commands: `command_key(prefix, "stream")` carries a
//!   [`Command`](crate::command::Command)<[`StreamControl`]>
//! - query: `query_key(prefix, "streams")` lists the advertised
//!   [`StreamDescriptor`]s (queryable, late-joiner seed)
//! - status: `status_key(prefix, "streams")` reports per-stream
//!   [`StreamStatus`] (open sessions / active profile / viewers)
//!
//! Stream *stats* (fps/kbps/drops/viewers) ride normal telemetry under
//! `zensight/<proto>/<source>/<stream>/stats/<metric>` so existing charts light
//! up for free.

use serde::{Deserialize, Serialize};

/// One named bandwidth **tier** a stream offers — the `<tier>` key chunk plus
/// its target encoder parameters (RFC 07 §1). Tiers are published concurrently,
/// each on its own `@media/<stream>/video/<codec>/<tier>` key; a viewer
/// subscribes to exactly the tier its link can take, so two viewers on
/// different links never fight over one encoder (#494, #497).
///
/// This is the tier *definition* — the sensor owns the numbers; the wire and
/// the key carry the *name*. It appears in [`StreamDescriptor::tiers`] (the
/// catalogue); the ladder itself is configured in `configs/parallax.json5`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TierSpec {
    /// Tier name — the `<tier>` key chunk (`low` / `medium` / `high`).
    pub name: String,
    /// Aspect-preserving height cap in pixels; `None` = native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    /// Target framerate.
    pub fps: u32,
    /// Target encoded bitrate.
    pub bitrate_kbps: u32,
}

/// Runtime control for one media stream, sent on the `stream` command topic.
///
/// Tagged like the other sensor command enums (`type`, snake_case), so on the
/// wire an open looks like
/// `{"type":"open_stream","stream":"cam0","tier":"high"}`.
///
/// Note per-viewer quality is expressed by *which `<tier>` key you subscribe
/// to*, not by a command (#494). These commands manage a stream's lifecycle and
/// keyframes; redefining what a tier *means* ([`TierSpec`]) is config-only
/// (`configs/parallax.json5`), not a bus command (#513).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamControl {
    /// Open (start publishing) one tier of a stream. The sensor declares the
    /// media publisher on the concrete `@media/<stream>/video/<codec>/<tier>`
    /// key (or `…/preview/<format>` for the preview codec) and starts the
    /// pipeline. Distinct tiers open independent encoders.
    OpenStream {
        /// Stream identifier (the `<stream>` key chunk).
        stream: String,
        /// Requested codec (e.g. `h264`, `mjpeg` for the preview); `None` =
        /// sensor default (video).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        /// Which tier to open (the `<tier>` key chunk); `None` = the sensor's
        /// default tier. Ignored for the preview codec.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
    },
    /// Close (stop publishing) one tier of a stream and undeclare its media
    /// publisher. Mirrors the `OpenStream` selector.
    CloseStream {
        /// Stream identifier.
        stream: String,
        /// Codec, matching the open; `None` = video default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        /// Tier, matching the open; `None` = default tier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
    },
    /// Force the encoder to emit a keyframe (IDR) on the next access unit of one
    /// tier. Normally unnecessary — the matching listener forces a keyframe when
    /// a subscriber appears — but RFC 07 §1 mandates it for the Nth viewer, who
    /// gets no matching-listener edge.
    RequestKeyframe {
        /// Stream identifier.
        stream: String,
        /// Which tier's encoder to force; `None` = default tier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
    },
}

/// Per-frame metadata riding as the Zenoh **attachment** on every `@media`
/// sample (#403). The payload stays opaque encoded bytes; this sidecar is what
/// lets a viewer gate on keyframes, detect gaps, and time frames without
/// parsing the bitstream.
///
/// Encoded with [`crate::serialization::encode`] as **CBOR** — it is *not* a
/// telemetry envelope (`TelemetryPoint`/`Format` never appear on `@media`),
/// just a small struct serialized compactly. `None` timing fields are omitted
/// on the wire (the encoder had no clock for them).
///
/// # Two types, one corpus (#711, #728)
///
/// `parallax::wire::FrameMeta` is a byte-compatible twin of this struct, and
/// that is deliberate: neither crate can import the other's. Depending on
/// `parallax-pipeline` here would drag the whole video engine into every sensor
/// that links `zensight-common`, most of which never touch video; the reverse
/// would drag Zenoh into parallax. What binds them instead is a **conformance
/// corpus** of canonical CBOR vectors, checked into both repos and pinned by a
/// test on both sides — ours is `tests/framemeta_corpus.rs`.
///
/// Two rules in that corpus are wire shape, not style, and "tidying" either one
/// breaks the twin:
///
/// - the `skip_serializing_if` attributes below mean an absent timestamp is
///   **missing from the CBOR map**, not present-and-null — a consumer reading
///   the map sees the difference;
/// - `dts_ns` is omitted when it *equals* `pts_ns` (the field is "if distinct",
///   below). That elision belongs to the **producer**; see
///   `zensight-sensor-parallax`'s `metadata_to_frame_meta`.
///
/// Field order is also pinned, since serde emits struct fields in declaration
/// order and the corpus is compared byte for byte.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
pub struct FrameMeta {
    /// Whether this frame is independently decodable (H.264 IDR / any JPEG).
    pub keyframe: bool,
    /// Presentation timestamp in nanoseconds, if the pipeline stamped one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pts_ns: Option<u64>,
    /// Decode timestamp in nanoseconds, if distinct from `pts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dts_ns: Option<u64>,
    /// Frame duration in nanoseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<u64>,
    /// Monotonic per-stream sequence number (gap ⇒ dropped frames).
    pub sequence: u64,
    /// Encoded frame width in pixels.
    pub width: u32,
    /// Encoded frame height in pixels.
    pub height: u32,
}

/// One advertised media stream, served from the `streams` query topic.
///
/// **Capability-bearing** (#507): a viewer builds a sensible tier selector from
/// the camera's *native* geometry and the tiers on offer, without opening the
/// stream first. Native `width`/`height`/`fps` are probed from the source
/// (`None` when genuinely unknown — e.g. an RTSP stream whose SDP carries no
/// dimensions); `codecs` reflects the real per-source capability, not a
/// hardcoded pair.
// No `Eq`: `fps` is an `f32` (native framerate), which is only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StreamDescriptor {
    /// Stream identifier (the `<stream>` key chunk).
    pub stream: String,
    /// Codecs this stream can be opened with (e.g. `["h264", "mjpeg"]`).
    pub codecs: Vec<String>,
    /// Whether the stream is currently open (any tier publishing).
    pub active: bool,
    /// Native capture width in pixels; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Native capture height in pixels; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Native capture framerate; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    /// The bandwidth tiers this stream offers, so a viewer can subscribe to an
    /// exact `<tier>` key and never advertise a tier the camera can't feed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierSpec>,
    /// Optional human-readable description (camera position, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The parameters of one running tier, as reported by the sensor.
///
/// **Two of these four are measured and two are not** (#504). `width`/`height`
/// come from the built pipeline — the scaler even-aligns dimensions and never
/// upscales, so they are genuinely what the encoder produces and can differ
/// from what the tier asked for. `fps` and `bitrate_kbps` are the tier's
/// configured *targets* read back out of its [`TierSpec`]; nothing measures
/// them here, and a hardware encoder that silently ignores a knob would not
/// show up in either.
///
/// For what a tier actually costs on the wire, read the stream's
/// `stats/kbps` telemetry (per stream, summed over open tiers) or measure at
/// the subscriber — `zensight-sensor-parallax/tests/e2e.rs` does the latter
/// per tier. `stats/rc_drops` says whether the bitrate cap is currently
/// biting (#510).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TierApplied {
    /// Encoded width in pixels (post-scale, even-aligned).
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
    /// Applied framerate cap.
    pub fps: u32,
    /// Applied encoded bitrate.
    pub bitrate_kbps: u32,
}

/// State of one running tier, reported inside [`StreamStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TierStatus {
    /// Tier name (the `<tier>` key chunk).
    pub tier: String,
    /// Parameters actually in effect on this tier's encoder.
    pub applied: TierApplied,
    /// Matching subscribers observed by this tier's media publisher.
    pub viewers: u32,
}

/// Why one tier of a stream stopped, reported inside [`StreamStatus`] (#691).
///
/// The tier is **named**, not implied. `StreamStatus::tiers` is a *live set* —
/// a tier that died is removed from it — so an end reported inside a
/// [`TierStatus`] would either be unreachable (the last tier's death empties
/// the vec entirely) or force a non-empty `tiers` onto an `open: false`
/// document, which every consumer reads as "these tiers are live". That would
/// be a worse lie than the one this type exists to end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StreamEnd {
    /// Which tier ended: a ladder rung's name (`low`/`medium`/`high`), or the
    /// literal `preview` for the JPEG preview profile.
    ///
    /// The same one-string tier vocabulary the `rx/{tier}/…` receiver-feedback
    /// telemetry uses (#715), minted from the producer's own profile, so a
    /// reported end can never name a tier an `open_stream` could not.
    pub tier: String,
    /// What happened.
    pub reason: StreamEndReason,
}

/// The producer's own account of why a tier stopped (#691).
///
/// **The producer is the only party that knows.** Before this existed a viewer
/// had to invent a sentence — *"stream ended"*, *"stream failed to open on the
/// sensor"* — out of an `open: false` and nothing else, and those sentences
/// were wrong as often as they were right: an idle reap, an operator close and
/// a dead camera were the same single bit.
///
/// Three families, told apart by [`Self::is_failure`]:
///
/// - **we ended it** — [`Closed`](Self::Closed), [`Idle`](Self::Idle),
///   [`Superseded`](Self::Superseded), [`Shutdown`](Self::Shutdown). Nothing is
///   wrong, and device health must not count these.
/// - **it ended itself** — [`SourceEnded`](Self::SourceEnded).
/// - **it failed** — [`Stalled`](Self::Stalled), [`Failed`](Self::Failed),
///   [`FailedOpen`](Self::FailedOpen). These, and only these, record a device
///   failure and fire the source's alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEndReason {
    /// Every opener called `close_stream`, and the idle countdown that follows
    /// a close ran out.
    ///
    /// A close does not stop a pipeline on its own — it releases a refcount,
    /// and the reaper does the stopping — so this is the *terminal* reason for
    /// a clean operator close, and it arrives one idle window after the close
    /// itself.
    Closed,
    /// Reaped unwatched while an opener still held a refcount: nobody ever
    /// subscribed, or the last viewer left and the opener never said goodbye.
    ///
    /// The crash backstop for a viewer that died without `close_stream`. Told
    /// apart from [`Closed`](Self::Closed) by the refcount at reap time, which
    /// is the honest discriminator: one says *the system did what it was
    /// told*, the other *the system cleaned up after something that vanished*.
    Idle,
    /// Released so another tier of the same stream could open.
    ///
    /// One camera serves one capture at a time, so on an exclusive source
    /// (V4L2/RTSP) a tier switch must hand the device over rather than wait out
    /// the idle window. With receiver-driven tier selection (#720) this is a
    /// routine event, and a viewer whose tile blinks through a switch is owed
    /// the reason.
    Superseded,
    /// The producer is stopping.
    Shutdown,
    /// The source signalled end-of-stream by itself — a finite source ran out.
    ///
    /// A live camera should never do this. If one does, that *is* the finding,
    /// which is why it is not folded into [`Closed`](Self::Closed).
    SourceEnded,
    /// The pipeline built and started but delivered no frame at all within the
    /// producer's first-frame window.
    ///
    /// A wedged source. Deliberately payload-free and deliberately not a
    /// [`Failed`](Self::Failed): the open succeeded, no element failed, and
    /// there is no error to quote — so none is invented. The window is a
    /// producer constant and appears in its logs.
    Stalled,
    /// A pipeline element failed mid-stream.
    Failed {
        /// The element that failed, when the pipeline named one; `egress` when
        /// the failure was on the producer's side of the sink (frame-metadata
        /// encoding, or the media publish itself).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        /// The failure, **in the failing element's own words**. Never a
        /// paraphrase: `node` and `message` stay separate on the wire, and only
        /// [`Display`](std::fmt::Display) ever joins them.
        message: String,
    },
    /// The open never completed: the pipeline could not be built or started, a
    /// publisher or listener could not be declared, or the camera could not be
    /// reached.
    ///
    /// Distinct from [`Failed`](Self::Failed) because this tier never ran, and
    /// the two send an operator to different places — *it never started* means
    /// check the config and whether the camera is reachable; *it stopped* means
    /// check the element `node` names. A late-joining consumer cannot recover
    /// that difference from context.
    FailedOpen {
        /// Why the open failed, in the failing layer's own words.
        message: String,
    },
}

impl StreamEndReason {
    /// Whether this end is a failure.
    ///
    /// The one predicate a consumer needs, so nobody re-derives the three
    /// families by hand and drifts. Device health and the source's alert both
    /// gate on exactly this.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Self::Stalled | Self::Failed { .. } | Self::FailedOpen { .. }
        )
    }
}

/// The operator-facing sentence for an end — **the single source of this
/// prose**.
///
/// The producer's log line, the device-health `last_error` and the viewer's
/// tile caption all render an end through here, so what an operator reads on
/// the tile is what the sensor recorded, word for word.
impl std::fmt::Display for StreamEndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("closed"),
            Self::Idle => f.write_str("no viewer — reaped"),
            Self::Superseded => f.write_str("released for another tier"),
            Self::Shutdown => f.write_str("the sensor stopped"),
            Self::SourceEnded => f.write_str("the source ended"),
            Self::Stalled => f.write_str("no frames from the camera"),
            Self::Failed {
                node: Some(node),
                message,
            } => write!(f, "{node}: {message}"),
            Self::Failed {
                node: None,
                message,
            } => f.write_str(message),
            Self::FailedOpen { message } => write!(f, "failed to open: {message}"),
        }
    }
}

/// Current state of one stream, reported on the `stream/<stream>` status doc.
///
/// **Per-tier** (#497): a stream can have several tiers live at once, each with
/// its own applied params and viewer count. A single `Option<profile>` could
/// not express two live tiers — this is a `Vec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StreamStatus {
    /// Stream identifier.
    pub stream: String,
    /// Whether the stream is currently open (any tier publishing).
    pub open: bool,
    /// Per-tier state for every tier currently live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierStatus>,
    /// Why the most recent tier of this stream stopped, if one has and the
    /// stream has not since resumed on that tier (#691).
    ///
    /// **Absent means no tier has stopped since this stream last opened** — not
    /// "stopped for an unknown reason". `skip_serializing_if` keeps it out of
    /// the map entirely rather than present-and-null, the same discipline
    /// [`FrameMeta`] and [`MediaReceiverReport`] follow.
    ///
    /// One end, not a per-tier history. This is an LWW state document (RFC 05
    /// §5) republished on every transition, so it says what *is*; a consumer
    /// that wants every end reads every transition, which every live consumer
    /// already does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_end: Option<StreamEnd>,
}

/// Receiver feedback for one `@media` key — the payload of
/// `@rpc/<producer>/stream/report` (RFC 07 §1.1, #714).
///
/// # What it is, and what a producer may do with it
///
/// A **snapshot**, which is what makes the procedure's `idempotent = true`
/// true: every counter is cumulative since this consumer subscribed, so a
/// resend under RFC 05 retry repeats a statement rather than adding to one. A
/// delta payload would not be idempotent.
///
/// RFC 07 §1.2 is **normative** about the other half: a producer **MUST NOT**
/// re-tune a shared tier from one consumer's report, and where it acts on
/// *aggregate* feedback it must state its arbitration rule — which must not be
/// "the most recent report". Two viewers share a tier; one reports loss; the
/// bitrate drops; the healthy viewer's picture degrades for a reason it cannot
/// see, caused by a peer it does not know exists. **Feedback informs; it does
/// not command.** The sanctioned adaptation is the *consumer* changing which
/// tier key it subscribes to, and the escape hatch for a viewer that needs its
/// own rate is a tier of its own.
///
/// # Absent is not zero
///
/// Four fields are `Option` because "not measured" and "measured as zero" are
/// different observations and a controller must be able to tell them apart.
/// The `skip_serializing_if` attributes are wire shape, not style: an absent
/// field is **missing from the map**, never present-and-null. RFC 07 §1.3 makes
/// this normative for frame age — where a deployment does not timestamp, frame
/// age is *not asked*, **never zero** — and the same reasoning covers a
/// consumer with no decode queue to report.
///
/// Field order is pinned by `tests/receiver_report_corpus.rs`, since serde
/// emits fields in declaration order and the vectors are compared byte for
/// byte.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaReceiverReport {
    // ── which key this is about: the same selector `StreamControl` uses ──
    /// Stream identifier.
    pub stream: String,
    /// Codec, as in [`StreamControl::CloseStream`]. `None` means the
    /// producer's default video profile.
    ///
    /// Present because `(stream, tier)` alone cannot name the JPEG preview
    /// key, and because reusing the open/close selector shape means a report
    /// can never name a key an `OpenStream` could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Tier name. `None` means the producer's default tier, as in
    /// [`StreamControl::OpenStream`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,

    // ── who is reporting ──
    /// Stable for this viewer instance, regenerated when a tile reopens.
    ///
    /// **In the payload, never in a key** (RFC 07 §1.1). N viewers are N
    /// callers of one key, told apart here — which is why the feedback surface
    /// costs no keyspace at all, and why viewer-origin telemetry was rejected.
    pub consumer_id: String,

    // ── the span this snapshot covers ──
    /// Milliseconds covered by this report.
    ///
    /// A **duration**, deliberately, where a wallclock instant would have been
    /// the obvious choice. A consumer's wallclock is a second skewed cross-host
    /// clock, and its only plausible use — `now - report_ms` — is precisely the
    /// laundered-latency mistake RFC 07 §1.3 forbids. The producer already
    /// knows when the report arrived; what it cannot know is the window the
    /// counters cover, which is what turns them into rates.
    pub interval_ms: u32,

    // ── counters, cumulative since this consumer subscribed ──
    /// Samples received on this key.
    pub received_frames: u64,
    /// Frames inferred missing from sequence gaps — *network* loss.
    pub lost_frames: u64,
    /// Frames the consumer shed on purpose (a deadline miss, a resync).
    pub dropped_frames: u64,
    /// Frames that reached the screen.
    pub decoded_frames: u64,
    /// The highest `FrameMeta.sequence` seen.
    pub last_sequence: u64,

    // ── timing: absent means NOT MEASURED (RFC 07 §1.3) ──
    /// Inter-arrival jitter, milliseconds. Undefined before the second frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interarrival_jitter_ms: Option<f32>,
    /// **Median** frame age over the interval: publisher HLC minus local
    /// arrival, in milliseconds.
    ///
    /// *Observed skewed latency* in RFC 07 §1.3's sense — an observation, never
    /// a verdict on the transport. **Negative values are reported, not
    /// clamped**, because a negative age *is* the skew evidence. Absent when
    /// the samples arrived unstamped: that is "not asked", and treating it as
    /// zero silently disables every deadline built on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_age_ms: Option<f32>,
    /// **Maximum** frame age over the interval, same clock and same caveats.
    ///
    /// Both a median and a max, because a producer aggregating N consumers must
    /// publish both a worst case and a typical case, and one scalar per
    /// consumer can feed only one of them honestly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_age_max_ms: Option<f32>,
    /// Decoder queue depth at the end of the interval.
    ///
    /// Absent when the consumer has no queue to report — the JPEG preview tile,
    /// which decodes each frame as it arrives and has nothing to queue. `0`
    /// would read "queue empty" where the truth is "no queue". The H.264 tile
    /// does report one (#717): a bounded channel drained by the decode task, so
    /// the depth is `max_capacity() - capacity()` — the browser tile's
    /// `decodeQueueSize`, same field and same meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder_queue_depth: Option<u32>,

    // ── recovery ──
    /// Sequence number of the last keyframe the consumer decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_keyframe_sequence: Option<u64>,
    /// Consumer-local milliseconds elapsed since that keyframe.
    ///
    /// Monotonic elapsed time on one host, not a cross-host subtraction: it can
    /// never be negative, and it must not share a mental bucket with
    /// [`Self::frame_age_ms`], which can. Hence the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_last_keyframe_ms: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::{Format, decode, encode};

    #[test]
    fn stream_control_roundtrip_all_variants() {
        for control in [
            StreamControl::OpenStream {
                stream: "cam0".into(),
                codec: Some("h264".into()),
                tier: Some("high".into()),
            },
            StreamControl::OpenStream {
                stream: "cam1".into(),
                codec: None,
                tier: None,
            },
            StreamControl::CloseStream {
                stream: "cam0".into(),
                codec: Some("h264".into()),
                tier: Some("low".into()),
            },
            StreamControl::RequestKeyframe {
                stream: "cam0".into(),
                tier: Some("medium".into()),
            },
        ] {
            for format in [Format::Json, Format::Cbor] {
                let bytes = encode(&control, format).unwrap();
                let back: StreamControl = decode(&bytes, format).unwrap();
                assert_eq!(back, control);
            }
        }
    }

    #[test]
    fn stream_control_wire_tag_convention() {
        // Same `type`-tagged snake_case shape as the other command enums.
        let json = serde_json::to_value(StreamControl::OpenStream {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            tier: None,
        })
        .unwrap();
        assert_eq!(json["type"], "open_stream");
        assert_eq!(json["stream"], "cam0");
        assert_eq!(json["codec"], "h264");
        assert!(json.get("tier").is_none(), "None fields are omitted");

        let json = serde_json::to_value(StreamControl::RequestKeyframe {
            stream: "cam0".into(),
            tier: None,
        })
        .unwrap();
        assert_eq!(json["type"], "request_keyframe");
    }

    #[test]
    fn descriptor_and_status_roundtrip() {
        let desc = StreamDescriptor {
            stream: "cam0".into(),
            codecs: vec!["h264".into(), "mjpeg".into()],
            active: true,
            width: Some(1280),
            height: Some(720),
            fps: Some(30.0),
            tiers: vec![
                TierSpec {
                    name: "low".into(),
                    max_height: Some(240),
                    fps: 10,
                    bitrate_kbps: 400,
                },
                TierSpec {
                    name: "high".into(),
                    max_height: None,
                    fps: 30,
                    bitrate_kbps: 4000,
                },
            ],
            description: Some("front door".into()),
        };
        let bytes = encode(&desc, Format::Cbor).unwrap();
        let back: StreamDescriptor = decode(&bytes, Format::Cbor).unwrap();
        assert_eq!(back, desc);

        let status = StreamStatus {
            stream: "cam0".into(),
            open: true,
            last_end: None,
            tiers: vec![
                TierStatus {
                    tier: "low".into(),
                    applied: TierApplied {
                        width: 320,
                        height: 240,
                        fps: 10,
                        bitrate_kbps: 400,
                    },
                    viewers: 1,
                },
                TierStatus {
                    tier: "high".into(),
                    applied: TierApplied {
                        width: 1280,
                        height: 720,
                        fps: 30,
                        bitrate_kbps: 4000,
                    },
                    viewers: 2,
                },
            ],
        };
        let bytes = encode(&status, Format::Json).unwrap();
        let back: StreamStatus = decode(&bytes, Format::Json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn frame_meta_roundtrip_both_formats() {
        for meta in [
            FrameMeta {
                keyframe: true,
                pts_ns: Some(1_000_000_000),
                dts_ns: Some(999_000_000),
                duration_ns: Some(33_333_333),
                sequence: 42,
                width: 1280,
                height: 720,
            },
            FrameMeta {
                keyframe: false,
                pts_ns: None,
                dts_ns: None,
                duration_ns: None,
                sequence: 0,
                width: 320,
                height: 240,
            },
        ] {
            for format in [Format::Json, Format::Cbor] {
                let bytes = encode(&meta, format).unwrap();
                let back: FrameMeta = decode(&bytes, format).unwrap();
                assert_eq!(back, meta);
            }
        }
    }

    #[test]
    fn frame_meta_none_timing_fields_are_omitted() {
        // Pin the wire shape: absent timing must not serialize as nulls.
        let json = serde_json::to_value(FrameMeta {
            keyframe: true,
            pts_ns: None,
            dts_ns: None,
            duration_ns: None,
            sequence: 7,
            width: 640,
            height: 360,
        })
        .unwrap();
        assert_eq!(json["keyframe"], true);
        assert_eq!(json["sequence"], 7);
        assert_eq!(json["width"], 640);
        assert_eq!(json["height"], 360);
        assert!(json.get("pts_ns").is_none(), "None fields are omitted");
        assert!(json.get("dts_ns").is_none());
        assert!(json.get("duration_ns").is_none());
    }

    fn a_report() -> MediaReceiverReport {
        MediaReceiverReport {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            tier: Some("high".into()),
            consumer_id: "tile-7f3a".into(),
            interval_ms: 2000,
            received_frames: 100,
            lost_frames: 1,
            dropped_frames: 2,
            decoded_frames: 97,
            last_sequence: 103,
            interarrival_jitter_ms: Some(3.5),
            frame_age_ms: Some(42.0),
            frame_age_max_ms: Some(118.25),
            decoder_queue_depth: Some(2),
            last_keyframe_sequence: Some(100),
            since_last_keyframe_ms: Some(533),
        }
    }

    #[test]
    fn media_receiver_report_roundtrips_both_encodings() {
        for format in [Format::Json, Format::Cbor] {
            let bytes = encode(&a_report(), format).expect("encode");
            let back: MediaReceiverReport = decode(&bytes, format).expect("decode");
            assert_eq!(back, a_report(), "{format:?}");
        }
    }

    /// Absent is not null and not zero — the JSON half of the rule the CBOR
    /// corpus pins at the byte level (`tests/receiver_report_corpus.rs`).
    ///
    /// The report rides `@rpc`, where `decode_auto` sniffs the first byte, so
    /// JSON is equally on the wire and a browser client may send it. RFC 07
    /// §1.3 makes the distinction normative for frame age: unstamped is *not
    /// asked*, never zero.
    #[test]
    fn media_receiver_report_omits_absent_options() {
        let report = MediaReceiverReport {
            codec: None,
            tier: None,
            interarrival_jitter_ms: None,
            frame_age_ms: None,
            frame_age_max_ms: None,
            decoder_queue_depth: None,
            last_keyframe_sequence: None,
            since_last_keyframe_ms: None,
            ..a_report()
        };
        let json = serde_json::to_value(&report).unwrap();

        // Present, because they are not Options.
        assert_eq!(json["stream"], "cam0");
        assert_eq!(json["consumer_id"], "tile-7f3a");
        assert_eq!(json["interval_ms"], 2000);
        assert_eq!(json["lost_frames"], 1);

        for absent in [
            "codec",
            "tier",
            "interarrival_jitter_ms",
            "frame_age_ms",
            "frame_age_max_ms",
            "decoder_queue_depth",
            "last_keyframe_sequence",
            "since_last_keyframe_ms",
        ] {
            assert!(
                json.get(absent).is_none(),
                "{absent} must be omitted, not null — a controller reading null \
                 as zero would treat 'not measured' as 'perfectly fresh'"
            );
        }
    }

    /// A negative frame age is an observation, not an error (RFC 07 §1.3).
    #[test]
    fn a_negative_frame_age_is_not_clamped_by_serde() {
        let report = MediaReceiverReport {
            frame_age_ms: Some(-12.5),
            ..a_report()
        };
        for format in [Format::Json, Format::Cbor] {
            let bytes = encode(&report, format).expect("encode");
            let back: MediaReceiverReport = decode(&bytes, format).expect("decode");
            assert_eq!(back.frame_age_ms, Some(-12.5), "{format:?}");
        }
    }

    /// A zero decoder queue and an absent one are different statements.
    ///
    /// `Some(0)` is "I have a queue and it is empty"; `None` is "I have no
    /// queue" — which is the iced H.264 tile today, since it decodes serially.
    /// Collapsing them would make an aggregate publish a queue depth for
    /// consumers that have no queue at all.
    #[test]
    fn an_empty_queue_and_no_queue_are_distinguishable() {
        let empty = MediaReceiverReport {
            decoder_queue_depth: Some(0),
            ..a_report()
        };
        let none = MediaReceiverReport {
            decoder_queue_depth: None,
            ..a_report()
        };
        assert_ne!(empty, none);
        let ej = serde_json::to_value(&empty).unwrap();
        let nj = serde_json::to_value(&none).unwrap();
        assert_eq!(ej["decoder_queue_depth"], 0);
        assert!(nj.get("decoder_queue_depth").is_none());
    }

    /// The report names the same key an `OpenStream` would.
    ///
    /// `(stream, codec, tier)` is deliberately the selector shape
    /// `StreamControl` already uses, so a report cannot name a key that could
    /// not have been opened — and so the sensor can resolve both through one
    /// function instead of two that can disagree.
    #[test]
    fn the_report_selector_matches_stream_controls() {
        let open = StreamControl::OpenStream {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            tier: Some("high".into()),
        };
        let report = a_report();
        match open {
            StreamControl::OpenStream {
                stream,
                codec,
                tier,
            } => {
                assert_eq!(stream, report.stream);
                assert_eq!(codec, report.codec);
                assert_eq!(tier, report.tier);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn stream_control_in_command_envelope() {
        use crate::command::Command;
        let cmd = Command::new(StreamControl::CloseStream {
            stream: "cam0".into(),
            codec: None,
            tier: None,
        })
        .with_id("req-1");
        let bytes = encode(&cmd, Format::Json).unwrap();
        let back: Command<StreamControl> = decode(&bytes, Format::Json).unwrap();
        assert_eq!(back.id.as_deref(), Some("req-1"));
        assert_eq!(
            back.body,
            StreamControl::CloseStream {
                stream: "cam0".into(),
                codec: None,
                tier: None,
            }
        );
    }

    /// Absent is **missing from the map**, never present-and-null: a consumer
    /// must be able to tell "nothing has stopped" from "stopped, reason
    /// unknown", and a `null` reads as the second.
    #[test]
    fn a_stream_that_has_not_stopped_carries_no_end() {
        let status = StreamStatus {
            stream: "cam0".into(),
            open: true,
            tiers: Vec::new(),
            last_end: None,
        };
        let json = String::from_utf8(encode(&status, Format::Json).unwrap()).unwrap();
        assert!(
            !json.contains("last_end"),
            "an absent end must not be on the wire at all: {json}"
        );
        let back: StreamStatus = decode(json.as_bytes(), Format::Json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn stream_end_is_tagged_and_keeps_node_separate() {
        let status = StreamStatus {
            stream: "cam0".into(),
            open: false,
            tiers: Vec::new(),
            last_end: Some(StreamEnd {
                tier: "high".into(),
                reason: StreamEndReason::Failed {
                    node: Some("h264enc".into()),
                    message: "encoder submit failed".into(),
                },
            }),
        };
        let json = String::from_utf8(encode(&status, Format::Json).unwrap()).unwrap();
        assert!(json.contains(r#""type":"failed""#), "{json}");
        // node and message stay two fields: only `Display` ever joins them.
        assert!(json.contains(r#""node":"h264enc""#), "{json}");
        assert!(
            json.contains(r#""message":"encoder submit failed""#),
            "{json}"
        );
        assert_eq!(
            decode::<StreamStatus>(json.as_bytes(), Format::Json).unwrap(),
            status
        );

        // A unit variant is the tag alone, and an unattributed failure omits
        // `node` rather than sending null.
        let closed = encode(&StreamEndReason::Closed, Format::Json).unwrap();
        assert_eq!(String::from_utf8(closed).unwrap(), r#"{"type":"closed"}"#);
        let anon = String::from_utf8(
            encode(
                &StreamEndReason::Failed {
                    node: None,
                    message: "boom".into(),
                },
                Format::Json,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!anon.contains("node"), "{anon}");
    }

    /// The three families, pinned so nobody re-derives them by hand. Device
    /// health and the source's alert both gate on exactly this predicate, so a
    /// variant that drifts into the wrong family silently starts (or stops)
    /// flipping a camera Offline.
    #[test]
    fn only_real_failures_are_failures() {
        for ok in [
            StreamEndReason::Closed,
            StreamEndReason::Idle,
            StreamEndReason::Superseded,
            StreamEndReason::Shutdown,
            StreamEndReason::SourceEnded,
        ] {
            assert!(!ok.is_failure(), "{ok} is not a failure");
        }
        for bad in [
            StreamEndReason::Stalled,
            StreamEndReason::Failed {
                node: None,
                message: "boom".into(),
            },
            StreamEndReason::FailedOpen {
                message: "no such device".into(),
            },
        ] {
            assert!(bad.is_failure(), "{bad} is a failure");
        }
    }

    #[test]
    fn an_end_renders_the_elements_own_words() {
        assert_eq!(
            StreamEndReason::Failed {
                node: Some("h264enc".into()),
                message: "encoder submit failed".into(),
            }
            .to_string(),
            "h264enc: encoder submit failed"
        );
        assert_eq!(
            StreamEndReason::Failed {
                node: None,
                message: "encoder submit failed".into(),
            }
            .to_string(),
            "encoder submit failed"
        );
        assert_eq!(StreamEndReason::Closed.to_string(), "closed");
        assert_eq!(
            StreamEndReason::FailedOpen {
                message: "connection refused".into(),
            }
            .to_string(),
            "failed to open: connection refused"
        );
    }
}
