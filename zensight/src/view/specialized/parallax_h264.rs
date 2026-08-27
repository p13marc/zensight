//! Optional H.264 live view (#409) — behind the `h264` cargo feature.
//!
//! Default builds ship WITHOUT this (openh264 is a C++ build from source —
//! unacceptable unconditionally on GUI/CI/flatpak): [`AVAILABLE`] is `false`
//! and the parallax view renders a "build with `--features h264`" hint. With
//! the feature, [`h264_tile_stream`] subscribes to the **exact** tier key
//! `@media/<stream>/video/h264/<tier>` — keyspace v1.3 revoked the
//! `video/h264/*` wildcard licence (RFC 07 §3): the sensor publishes every
//! tier of the ladder concurrently on its own key, the catalogue advertises
//! which tiers a stream offers, and each viewer subscribes to exactly the one
//! its link chose. A `*` here would pull *every* tier at once — the opposite
//! of demand-driven simulcast. The stream then decodes access units directly
//! (no parallax pipeline/executor — a leaked live-source blocking task in
//! the GUI process would hang shutdown, see the sensor's `StoppableSource`
//! notes): gate on the first `FrameMeta.keyframe`, decode → I420 → RGBA →
//! [`iced::widget::image::Handle`], and on any sequence discontinuity drop
//! sync, rebuild the decoder, and ask the sensor for a fresh IDR via
//! [`Message::ParallaxRequestKeyframe`].
//!
//! # The tile stays live, and says why when it cannot (#716, #717, #718)
//!
//! Decoding is no longer serial-in-line. An access unit is enqueued on a
//! **bounded** channel that a long-lived blocking decode task drains, so the
//! backlog that used to grow invisibly in the subscriber queue is now a number
//! — `max_capacity() - capacity()`, the same quantity the browser tile reads
//! from `VideoDecoder.decodeQueueSize` (#717). Enqueue is where the frame-age
//! deadline is applied (#716), because that is the one place where both the
//! frame's age and the decoder's backlog are visible. And every few seconds
//! the tile tells its producer what it measured, as a `MediaReceiverReport`
//! (#718, RFC 07 §1.1).

/// Whether this build carries the H.264 decoder.
#[cfg(feature = "h264")]
pub const AVAILABLE: bool = true;
/// Whether this build carries the H.264 decoder.
#[cfg(not(feature = "h264"))]
pub const AVAILABLE: bool = false;

/// The hint shown in place of the video controls on builds without the
/// feature.
pub const UNAVAILABLE_HINT: &str = "H.264 live view requires a build with --features h264.";

#[cfg(feature = "h264")]
pub use real::{DecodeFailure, Decoded, H264TileDecoder, h264_tile_stream};

#[cfg(feature = "h264")]
mod real {
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use iced::futures::Stream;
    use iced::widget::image;
    use parallax::buffer::{Buffer, MemoryHandle};
    use parallax::converters::{PixelFormat, VideoConvert};
    use parallax::element::Element;
    use parallax::elements::H264Decoder;
    use parallax::memory::SharedArena;
    use parallax::metadata::Metadata;
    use tokio::sync::mpsc;
    use zenoh::Session;
    use zensight_common::keyexpr::media_video_key;
    use zensight_common::media::observed_frame_age_ms;
    use zensight_common::stream::FrameMeta;
    use zensight_common::{Format, decode};

    use crate::message::Message;
    use crate::view::specialized::parallax_receiver::{
        DecodeLoss, Gap, REPORT_INTERVAL, ReceiverStats, Shed,
    };

    /// Minimum spacing between resync `RequestKeyframe` commands (and their
    /// warn lines) from one tile. A stream that fails to decode every AU
    /// otherwise spams one command + one warn per frame (#435); within the
    /// window the tile still drops sync and waits for the next keyframe —
    /// it just doesn't re-ask (or re-warn) for one.
    ///
    /// The frame-age deadline (#716) asks through this same gate rather than a
    /// second one of its own: a deadline miss under a sustained stall would
    /// otherwise become exactly the keyframe storm #435 was about.
    const RESYNC_MIN_INTERVAL: Duration = Duration::from_secs(2);

    /// If a tile keeps RECEIVING access units but never decodes a single
    /// displayable frame within this window, give up and end the tile with a
    /// reason instead of showing a silent black rectangle forever. This covers
    /// the GUI-only failure the sensor's own first-frame watchdog can't see
    /// (the sensor IS publishing — it just can't be decoded/synced here). The
    /// window is generous enough to outlast a slow first keyframe (a late
    /// viewer waits up to one GOP for a natural IDR).
    ///
    /// This carries more weight since parallax 0.7 (#689). The decoder used to
    /// return `Err` on an access unit it could not use, which tripped the
    /// resync path below within one frame; it now *skips* such units and only
    /// errors after 300 consecutive refusals — about 10 s at 30 fps, longer
    /// below that. So the fast path out of an undecodable stream is no longer
    /// the resync but this timeout, and a tile that receives AUs it can never
    /// decode ends here rather than asking for keyframes it cannot use.
    ///
    /// That is the better trade and the reason it is left at 12 s rather than
    /// tightened: a stream that hits one bad AU and recovers no longer spends a
    /// keyframe request on it, which is exactly the spam #435 was about. The
    /// cost is that a genuinely broken tier takes seconds rather than one frame
    /// to give up, and it gives up with a reason either way.
    const NO_DECODE_TIMEOUT: Duration = Duration::from_secs(12);

    /// If a tile receives NO access unit at all within this window, the tier
    /// isn't publishing (e.g. its open failed on the sensor — a single camera
    /// busy with another tier, RFC 07 §3 exact-tier keys mean nothing else
    /// fills in). End the tile with a reason instead of a permanent "Waiting
    /// for frames…". Comfortably longer than the sensor's own tier hand-over +
    /// build-retry window.
    const NO_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

    /// How many access units may be waiting for the decoder at once (#717).
    ///
    /// This is the tile's `decodeQueueSize`, and it is deliberately shallow. A
    /// deep queue on a live plane buys nothing: `frame`'s QoS already declares
    /// a stale frame worthless, so depth beyond "cover a scheduling hiccup"
    /// only converts a visible drop into invisible latency — which is the
    /// exact failure #716 exists to stop. Eight frames is a quarter-second at
    /// 30 fps.
    const DECODE_QUEUE_CAP: usize = 8;

    /// How many oversize access units a tile tolerates before it gives up.
    ///
    /// An AU too large for a decoder slot is not transient — the encoder is
    /// producing a picture this tile cannot hold, and it will do so again on
    /// the next IDR. Three strikes distinguishes "one freak keyframe" from
    /// "this tier is undecodable here", and the second case ends the tile with
    /// a stated reason rather than resyncing at it forever.
    const OVERSIZE_STRIKES: u32 = 3;

    /// A stateful H.264 → RGBA frame decoder (pure — unit-testable without
    /// Zenoh). Owns the openh264 decoder plus a cached I420→RGBA converter
    /// keyed by frame dimensions.
    pub struct H264TileDecoder {
        decoder: H264Decoder,
        converter: Option<(u32, u32, VideoConvert)>,
        /// Backing store for the access units handed to the decoder.
        ///
        /// parallax 0.7 made a decoder an ordinary `Element` (#160), so the
        /// input is a `Buffer` rather than a `&[u8]` and the caller owns the
        /// memory it comes from. Slots are recycled, so this is one allocation
        /// for the life of the tile rather than one per frame.
        arena: SharedArena,
    }

    /// Slot size for the access-unit arena.
    ///
    /// One compressed AU at the tier resolutions we publish. It was 1 MiB, and
    /// a native-resolution IDR on a high tier is what that could not hold — an
    /// oversize AU is a hard error, and #717's acceptance criterion is that a
    /// 1080p high-tier stream does not silently fail on one. Two MiB covers an
    /// IDR far above any tier the reference sensor offers, at 16 MiB of arena
    /// per open video tile (and the GUI opens one tile per stream).
    const AU_SLOT_BYTES: usize = 2 << 20;

    /// How many AUs may be in flight through the decoder at once. The decoder
    /// holds a reference while it reorders, so this cannot be 1.
    const AU_SLOTS: usize = 8;

    /// What came of feeding one access unit to the decoder.
    ///
    /// This used to be `Option<(u32, u32, Vec<u8>)>`, where `None` meant
    /// *either* "the decoder is buffering, more data coming" *or* "the arena
    /// had no free slot and the frame is gone". They are different diagnoses —
    /// one is normal, the other is decoder overload — and an operator watching
    /// a tile lose a third of its frames to arena exhaustion saw exactly the
    /// same picture as one losing them to the network (#717).
    #[derive(Debug)]
    pub enum Decoded {
        /// A displayable picture: width, height, RGBA.
        Picture(u32, u32, Vec<u8>),
        /// The decoder needs more data before it can emit a picture. Normal.
        Buffered,
        /// No free arena slot: the frame was dropped without being decoded.
        ArenaFull,
    }

    /// Why an access unit could not be decoded at all.
    #[derive(Debug)]
    pub enum DecodeFailure {
        /// Larger than one arena slot. Not transient: the next IDR will be too.
        Oversize { bytes: usize, slot: usize },
        /// The decoder refused it.
        Codec(String),
    }

    impl std::fmt::Display for DecodeFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Oversize { bytes, slot } => write!(
                    f,
                    "access unit of {bytes} bytes exceeds the {slot}-byte decoder slot"
                ),
                Self::Codec(e) => write!(f, "{e}"),
            }
        }
    }

    impl H264TileDecoder {
        pub fn new() -> Result<Self, String> {
            Ok(Self {
                decoder: H264Decoder::new().map_err(|e| e.to_string())?,
                converter: None,
                arena: SharedArena::new(AU_SLOT_BYTES, AU_SLOTS).map_err(|e| e.to_string())?,
            })
        }

        /// Rebuild the decoder after a discontinuity (stale reference frames
        /// would otherwise smear until the next IDR).
        pub fn reset(&mut self) -> Result<(), String> {
            self.decoder = H264Decoder::new().map_err(|e| e.to_string())?;
            Ok(())
        }

        /// Decode one access unit. See [`Decoded`] for why the two
        /// no-picture outcomes are told apart.
        pub fn decode_to_rgba(&mut self, nal: &[u8]) -> Result<Decoded, DecodeFailure> {
            if nal.len() > AU_SLOT_BYTES {
                return Err(DecodeFailure::Oversize {
                    bytes: nal.len(),
                    slot: AU_SLOT_BYTES,
                });
            }
            // Sweep the release queue FIRST. A dropped slot is pushed onto the
            // arena's queue, not freed in place, and only the owner drains it —
            // `SharedArena::acquire` does not, it just scans for an already-free
            // slot. Nothing else in either process hits this because everything
            // else allocates through the engine's `OutputArena`, whose
            // `try_acquire` reclaims on every call; this is the one hand-rolled
            // `SharedArena` we own. Without the sweep the tile handed out its
            // AU_SLOTS slots exactly once and then starved forever: the video
            // froze on the 8th decoded frame — no error, no resync, no timeout,
            // GUI still responsive — because a starved `acquire` was reported
            // as ordinary decoder buffering.
            self.arena.reclaim();
            // Every slot is still held by the decoder or by a frame the UI has
            // not dropped yet. The frame is gone, and it is gone for a reason
            // that is neither the network's fault nor the decoder's refusal —
            // so it is counted as itself rather than laundered into either.
            let Some(mut slot) = self.arena.acquire() else {
                return Ok(Decoded::ArenaFull);
            };
            slot.data_mut()[..nal.len()].copy_from_slice(nal);
            let input = Buffer::new(MemoryHandle::with_len(slot, nal.len()), Metadata::default());

            let Some(out) = self
                .decoder
                .process(input)
                .map_err(|e| DecodeFailure::Codec(e.to_string()))?
            else {
                return Ok(Decoded::Buffered);
            };
            // Geometry travels on the buffer now: `DecodedFrame` is
            // crate-internal in 0.8, and the legacy `"width"`/`"height"`
            // metadata keys carry nothing (#160).
            let (w, h) = out.metadata().video_dims().ok_or_else(|| {
                DecodeFailure::Codec("decoded frame declared no geometry".to_string())
            })?;

            if self
                .converter
                .as_ref()
                .is_none_or(|(cw, ch, _)| (*cw, *ch) != (w, h))
            {
                let conv = VideoConvert::new(PixelFormat::I420, PixelFormat::Rgba, w, h)
                    .map_err(|e| DecodeFailure::Codec(e.to_string()))?;
                self.converter = Some((w, h, conv));
            }
            let (_, _, conv) = self.converter.as_ref().expect("converter just cached");
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            // 0.8 takes the input plane layout so a strided frame needs no
            // repack (#196); the decoder hands back packed I420.
            conv.convert(out.as_bytes(), conv.packed_input_layout(), &mut rgba)
                .map_err(|e| DecodeFailure::Codec(e.to_string()))?;
            Ok(Decoded::Picture(w, h, rgba))
        }
    }

    /// One item on the bounded decode queue.
    ///
    /// `Reset` rides the *same* channel as the access units on purpose: a
    /// decoder reset is a point in the stream, not a side channel. Resetting
    /// out of band would race whatever is still queued and rebuild the decoder
    /// underneath frames that were fine.
    enum DecodeJob {
        Au {
            payload: Vec<u8>,
            sequence: u64,
            keyframe: bool,
        },
        Reset,
    }

    /// What the decode task hands back.
    enum DecodeOut {
        Picture {
            sequence: u64,
            keyframe: bool,
            width: u32,
            height: u32,
            rgba: Vec<u8>,
        },
        Buffered,
        ArenaFull,
        Oversize(usize),
        Failed(String),
    }

    /// The decode task's body — one blocking thread for the tile's lifetime.
    ///
    /// It ends when `jobs` closes, which happens when the tile's stream is
    /// dropped (the iced task handle is `abort_on_drop`). That is the whole
    /// shutdown path: no flag, no cancellation token, nothing to leak.
    fn decode_loop(
        mut dec: H264TileDecoder,
        mut jobs: mpsc::Receiver<DecodeJob>,
        out: mpsc::Sender<DecodeOut>,
    ) {
        while let Some(job) = jobs.blocking_recv() {
            let message = match job {
                DecodeJob::Reset => {
                    let _ = dec.reset();
                    continue;
                }
                DecodeJob::Au {
                    payload,
                    sequence,
                    keyframe,
                } => match dec.decode_to_rgba(&payload) {
                    Ok(Decoded::Picture(width, height, rgba)) => DecodeOut::Picture {
                        sequence,
                        keyframe,
                        width,
                        height,
                        rgba,
                    },
                    Ok(Decoded::Buffered) => DecodeOut::Buffered,
                    Ok(Decoded::ArenaFull) => DecodeOut::ArenaFull,
                    Err(DecodeFailure::Oversize { bytes, .. }) => DecodeOut::Oversize(bytes),
                    Err(e @ DecodeFailure::Codec(_)) => DecodeOut::Failed(e.to_string()),
                },
            };
            if out.blocking_send(message).is_err() {
                break;
            }
        }
    }

    /// What to do with one arriving access unit.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Admit {
        /// Hand it to the decoder.
        Decode,
        /// Drop it, for this reason.
        Shed(Shed),
    }

    /// The playout policy (#716), as a pure function — so the rule that decides
    /// what an operator actually sees is testable without a bus, a decoder, or a
    /// clock.
    ///
    /// A **late keyframe is always admitted**. Shedding those too would leave a
    /// tile on a genuinely slow link showing nothing at all; taking them gives a
    /// slideshow that snaps back to live the moment the link does, and the report
    /// carries the high frame age either way. And an **unstamped** sample never
    /// trips the deadline: an unknown age is not age zero (RFC 07 §1.3), so a
    /// producer with timestamping off plays rather than shedding everything.
    fn admit(
        meta: &FrameMeta,
        age_ms: Option<f64>,
        synced: bool,
        max_live_latency: Option<Duration>,
    ) -> Admit {
        let late = max_live_latency
            .zip(age_ms)
            .is_some_and(|(limit, age)| age > limit.as_millis() as f64);
        if late && !meta.keyframe {
            Admit::Shed(Shed::Deadline)
        } else if !synced && !meta.keyframe {
            // The reference chain is gone; this AU is undecodable, not lost.
            Admit::Shed(Shed::Unsynced)
        } else {
            Admit::Decode
        }
    }

    /// Ask the sensor for a fresh IDR, at most once per
    /// [`RESYNC_MIN_INTERVAL`]. Every path that drops sync goes through here,
    /// so no combination of them can add up to a keyframe storm (#435).
    fn ask_for_keyframe(
        last_resync: &mut Option<Instant>,
        outbox: &mut Vec<Message>,
        stream: &str,
    ) {
        if last_resync.is_none_or(|at| at.elapsed() >= RESYNC_MIN_INTERVAL) {
            *last_resync = Some(Instant::now());
            outbox.push(Message::ParallaxRequestKeyframe {
                stream: stream.to_string(),
            });
        }
    }

    /// The per-tile H.264 subscriber stream: decoded video frames as
    /// [`image::Handle`]s, plus a periodic [`Message::ParallaxReceiverReport`]
    /// (#718). Ends with [`Message::ParallaxTileEnded`]; aborting the wrapping
    /// task drops the future, undeclares the subscriber and stops the decode
    /// task. Every yielded message carries the tile `generation` it was opened
    /// with.
    ///
    /// `max_live_latency` is the frame-age deadline (#716). `None` disables it.
    /// A sample that arrives **unstamped** never trips it — an unknown age is
    /// not age zero (RFC 07 §1.3), and treating it as zero would silently
    /// disable the deadline against a producer with timestamping off.
    pub fn h264_tile_stream(
        session: Arc<Session>,
        origin: zenkey::RemoteOrigin,
        stream: String,
        tier: String,
        generation: u64,
        max_live_latency: Option<Duration>,
    ) -> impl Stream<Item = Message> {
        async_stream::stream! {
            // The EXACT tier key (keyspace v1.3): the sensor publishes each
            // ladder tier concurrently on its own `video/h264/<tier>` key, the
            // catalogue advertises which tiers a stream offers, and this viewer
            // subscribes to exactly the one it picked. A `*` here would pull
            // every tier at once (RFC 07 §3 revoked that licence). Zenoh
            // matching is exact, so the sensor's per-tier matching listener
            // counts this subscriber against that tier alone (pinned in the
            // sensor e2e: two viewers on distinct tiers stream independently).
            let key = media_video_key(&origin, &stream, "h264", &tier);
            let subscriber = match session.declare_subscriber(&key).await {
                Ok(s) => s,
                Err(e) => {
                    yield Message::ParallaxTileEnded {
                        stream,
                        generation,
                        error: Some(format!("subscribe failed: {e}")),
                    };
                    return;
                }
            };
            let dec = match H264TileDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    yield Message::ParallaxTileEnded { stream, generation, error: Some(e) };
                    return;
                }
            };

            let (jobs, job_rx) = mpsc::channel::<DecodeJob>(DECODE_QUEUE_CAP);
            let (out_tx, mut decoded) = mpsc::channel::<DecodeOut>(DECODE_QUEUE_CAP);
            // Detached on purpose: it exits when `jobs` drops with this stream.
            let _decode_task = tokio::task::spawn_blocking(move || decode_loop(dec, job_rx, out_tx));

            let mut stats = ReceiverStats::new(
                stream.clone(),
                Some("h264".to_string()),
                Some(tier.clone()),
                generation,
                Instant::now(),
            );
            // Never feed the decoder before its first IDR.
            let mut synced = false;
            // Backoff for resync keyframe requests (see RESYNC_MIN_INTERVAL);
            // cleared by a successful decode so a fresh failure after a
            // healthy stretch asks immediately.
            let mut last_resync: Option<Instant> = None;
            // A reset the queue was too full to accept. It must land BEFORE the
            // next access unit, so it is retried at the next enqueue rather
            // than dropped — a decoder that missed its reset would smear stale
            // reference frames into the recovery keyframe.
            let mut pending_reset = false;
            // Guard against a tile that receives frames but never decodes one
            // (NO_DECODE_TIMEOUT): stamp when the first AU arrives, and whether
            // any displayable frame has ever come out.
            let mut first_frame_at: Option<Instant> = None;
            let mut ever_decoded = false;
            // Whether ANY sample has arrived on this tier's key. Until one does,
            // bound the wait (NO_FIRST_FRAME_TIMEOUT) so a tier that never
            // publishes (open failed on the sensor) ends with a reason.
            let mut any_sample = false;
            let mut oversize_strikes = 0u32;

            // The report cadence runs on its own clock, NOT off arriving
            // frames: a tile that is receiving nothing still reports, and a
            // report saying "nothing is arriving" is the most useful one there
            // is (#718).
            let mut reports = tokio::time::interval_at(
                tokio::time::Instant::now() + REPORT_INTERVAL,
                REPORT_INTERVAL,
            );
            reports.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let first_frame_deadline = tokio::time::sleep(NO_FIRST_FRAME_TIMEOUT);
            tokio::pin!(first_frame_deadline);

            // Messages are collected here and yielded after the select, rather
            // than yielded from inside an arm: `async_stream` rewrites `yield`
            // syntactically and nesting it inside another macro's arm is not
            // worth the risk.
            let mut outbox: Vec<Message> = Vec::new();
            let mut ended: Option<Option<String>> = None;

            while ended.is_none() {
                tokio::select! {
                    _ = &mut first_frame_deadline, if !any_sample => {
                        ended = Some(Some(
                            "no video on this tier — the camera may be busy or unavailable"
                                .to_string(),
                        ));
                    }
                    received = subscriber.recv_async() => 'sample: {
                        let Ok(sample) = received else {
                            ended = Some(None); // session closed
                            break 'sample;
                        };
                        any_sample = true;
                        let Some(meta) = sample
                            .attachment()
                            .and_then(|a| decode::<FrameMeta>(&a.to_bytes(), Format::Cbor).ok())
                        else {
                            break 'sample;
                        };
                        // Frames ARE arriving. If none ever decodes within the
                        // window, stop showing a silent black tile and say why.
                        let arrived = *first_frame_at.get_or_insert_with(Instant::now);
                        if !ever_decoded && arrived.elapsed() >= NO_DECODE_TIMEOUT {
                            ended = Some(Some(format!(
                                "receiving {}×{} video but could not decode this tier",
                                meta.width, meta.height
                            )));
                            break 'sample;
                        }
                        // The frame-age clock is the publisher's HLC stamp
                        // (RFC 07 §1.3), read as observed skewed latency:
                        // `None` means unstamped, which is NOT age zero.
                        let age_ms =
                            observed_frame_age_ms(sample.timestamp(), SystemTime::now());
                        let gap = stats.on_sample(&meta, age_ms);

                        // Any break in the sequence — a gap (dropped access
                        // units), a pipeline restart, or a backwards jump —
                        // means the reference frames this AU needs are not
                        // what the decoder holds. Drop sync, reset, and ask
                        // for a fresh IDR.
                        if synced && gap != Gap::None {
                            synced = false;
                            pending_reset = true;
                            ask_for_keyframe(&mut last_resync, &mut outbox, &stream);
                        }

                        match admit(&meta, age_ms, synced, max_live_latency) {
                            Admit::Shed(why) => {
                                stats.on_shed(why);
                                if synced {
                                    synced = false;
                                    pending_reset = true;
                                }
                                // Only a deadline miss asks: the unsynced skip
                                // is already waiting on a keyframe the gap
                                // path asked for.
                                if why == Shed::Deadline {
                                    ask_for_keyframe(&mut last_resync, &mut outbox, &stream);
                                }
                                break 'sample;
                            }
                            Admit::Decode => {}
                        }

                        // Admitted, so this is either in-sequence or the
                        // keyframe that re-anchors us.
                        synced = true;
                        // The reset must precede the AU it belongs in front
                        // of; if the queue cannot take it, shed the AU and try
                        // again next time rather than decoding against stale
                        // references.
                        if pending_reset && jobs.try_send(DecodeJob::Reset).is_err() {
                            stats.on_shed(Shed::QueueFull);
                            synced = false;
                            break 'sample;
                        }
                        pending_reset = false;
                        let job = DecodeJob::Au {
                            payload: sample.payload().to_bytes().to_vec(),
                            sequence: meta.sequence,
                            keyframe: meta.keyframe,
                        };
                        match jobs.try_send(job) {
                            Ok(()) => {}
                            // The decoder is behind. Shed to the next keyframe
                            // rather than blocking here, which is how the
                            // backlog used to grow.
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                stats.on_shed(Shed::QueueFull);
                                synced = false;
                                pending_reset = true;
                                ask_for_keyframe(&mut last_resync, &mut outbox, &stream);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                ended = Some(Some("decoder stopped".to_string()));
                            }
                        }
                        stats.set_queue_depth(Some(queue_depth(&jobs)));
                    },
                    out = decoded.recv() => {
                        match out {
                            None => ended = Some(Some("decoder stopped".to_string())),
                            Some(DecodeOut::Picture { sequence, keyframe, width, height, rgba }) => {
                                ever_decoded = true;
                                last_resync = None;
                                stats.on_decoded(sequence, keyframe, Instant::now());
                                stats.set_queue_depth(Some(queue_depth(&jobs)));
                                outbox.push(Message::ParallaxFrame {
                                    stream: stream.clone(),
                                    generation,
                                    seq: sequence,
                                    handle: image::Handle::from_rgba(width, height, rgba),
                                });
                            }
                            // The decoder needs more data. Normal, and now
                            // distinguishable from the two below.
                            Some(DecodeOut::Buffered) => {}
                            Some(DecodeOut::ArenaFull) => {
                                stats.on_decode_loss(DecodeLoss::ArenaFull);
                            }
                            Some(DecodeOut::Oversize(bytes)) => {
                                stats.on_decode_loss(DecodeLoss::Oversize);
                                oversize_strikes += 1;
                                if oversize_strikes >= OVERSIZE_STRIKES {
                                    ended = Some(Some(format!(
                                        "access units of {bytes} bytes exceed the \
                                         {AU_SLOT_BYTES}-byte decoder slot — this tier \
                                         cannot be decoded here"
                                    )));
                                } else {
                                    synced = false;
                                    pending_reset = true;
                                    tracing::warn!(
                                        stream = %stream, bytes, slot = AU_SLOT_BYTES,
                                        "h264 access unit exceeds the decoder slot"
                                    );
                                    ask_for_keyframe(&mut last_resync, &mut outbox, &stream);
                                }
                            }
                            Some(DecodeOut::Failed(e)) => {
                                stats.on_decode_loss(DecodeLoss::Failed);
                                synced = false;
                                pending_reset = true;
                                if last_resync.is_none_or(|at| at.elapsed() >= RESYNC_MIN_INTERVAL) {
                                    tracing::warn!(stream = %stream, error = %e, "h264 decode failed; resyncing");
                                } else {
                                    tracing::debug!(stream = %stream, error = %e, "h264 decode failed during resync backoff");
                                }
                                ask_for_keyframe(&mut last_resync, &mut outbox, &stream);
                            }
                        }
                    }
                    _ = reports.tick() => {
                        stats.set_queue_depth(Some(queue_depth(&jobs)));
                        outbox.push(Message::ParallaxReceiverReport {
                            stream: stream.clone(),
                            generation,
                            report: Box::new(stats.snapshot(Instant::now())),
                        });
                    }
                }
                for message in outbox.drain(..) {
                    yield message;
                }
            }
            yield Message::ParallaxTileEnded {
                stream,
                generation,
                error: ended.flatten(),
            };
        }
    }

    /// Access units waiting for the decoder — the tile's `decodeQueueSize`
    /// (#717). `max_capacity` is the constant; `capacity` is what is still
    /// free, so the difference is what is pending.
    fn queue_depth(jobs: &mpsc::Sender<DecodeJob>) -> u32 {
        (jobs.max_capacity() - jobs.capacity()) as u32
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use parallax::elements::{H264Encoder, H264EncoderConfig};

        /// Encode synthetic I420 frames with the same openh264 the sensor
        /// uses, then require our tile decoder to produce RGBA pictures —
        /// the whole decode path without any Zenoh.
        #[test]
        fn encode_decode_round_trip_produces_rgba() {
            let (w, h) = (64u32, 48u32);
            // parallax 0.6: the config carries no geometry — dimensions travel with
            // the frame data (`encode_yuv420_at`), so a resolution switch is a
            // clean IDR with no configured size to contradict.
            let mut encoder = H264Encoder::new(
                H264EncoderConfig::new()
                    .bitrate(200_000)
                    .frame_rate(10.0)
                    .keyframe_interval(10),
            )
            .expect("create encoder");
            let mut decoder = H264TileDecoder::new().expect("create decoder");

            // Simple I420 frame: mid-gray luma, neutral chroma.
            let mut yuv = vec![128u8; (w * h) as usize];
            yuv.extend(std::iter::repeat_n(128u8, (w * h / 2) as usize));

            let mut decoded = 0usize;
            for _ in 0..5 {
                let nal = encoder.encode_yuv420_at(&yuv, w, h).expect("encode frame");
                if nal.is_empty() {
                    continue;
                }
                if let Decoded::Picture(dw, dh, rgba) =
                    decoder.decode_to_rgba(&nal).expect("decode access unit")
                {
                    assert_eq!((dw, dh), (w, h));
                    assert_eq!(rgba.len(), (w * h * 4) as usize);
                    // Mid-gray in, mid-gray out (allow codec wiggle).
                    assert!((rgba[0] as i32 - 128).abs() < 24, "r = {}", rgba[0]);
                    decoded += 1;
                }
            }
            assert!(decoded > 0, "no frames decoded from the round trip");

            // A reset decoder keeps working from the next IDR.
            decoder.reset().expect("reset");
            encoder.force_keyframe();
            let nal = encoder.encode_yuv420_at(&yuv, w, h).expect("encode idr");
            let recovered = matches!(
                decoder.decode_to_rgba(&nal).expect("decode after reset"),
                Decoded::Picture(..)
            ) || matches!(
                decoder
                    .decode_to_rgba(&encoder.encode_yuv420_at(&yuv, w, h).expect("encode next"))
                    .expect("decode next"),
                Decoded::Picture(..)
            );
            assert!(recovered, "decoder must recover after reset + IDR");
        }

        /// A tile must keep decoding past its arena's slot count.
        ///
        /// The slots come back through the arena's release queue, which only a
        /// `reclaim()` drains — without one the tile decoded exactly
        /// [`AU_SLOTS`] frames and then froze on the last picture forever,
        /// silently: a starved `acquire` was `Ok(None)`, which the stream loop
        /// read as "decoder buffered, more data coming", so no error surfaced,
        /// no resync fired, and the no-decode watchdog stayed disarmed (it only
        /// covers a tile that never decoded *anything*). The round trip above
        /// runs five frames and could never see it.
        #[test]
        fn decoding_continues_past_the_arena_slot_count() {
            let (w, h) = (64u32, 48u32);
            let mut encoder = H264Encoder::new(
                H264EncoderConfig::new()
                    .bitrate(200_000)
                    .frame_rate(10.0)
                    .keyframe_interval(10),
            )
            .expect("create encoder");
            let mut decoder = H264TileDecoder::new().expect("create decoder");

            let frames = AU_SLOTS * 4;
            let mut decoded = 0usize;
            for i in 0..frames {
                // Vary the luma so every frame carries residual and the encoder
                // never emits an empty AU.
                let mut yuv = vec![(64 + (i * 7) % 128) as u8; (w * h) as usize];
                yuv.extend(std::iter::repeat_n(128u8, (w * h / 2) as usize));
                let nal = encoder.encode_yuv420_at(&yuv, w, h).expect("encode frame");
                if nal.is_empty() {
                    continue;
                }
                if matches!(
                    decoder.decode_to_rgba(&nal).expect("decode access unit"),
                    Decoded::Picture(..)
                ) {
                    decoded += 1;
                }
            }
            assert!(
                decoded > AU_SLOTS,
                "decoded only {decoded} of {frames} frames — the arena starved at its \
                 {AU_SLOTS}-slot ceiling instead of reclaiming released slots"
            );
        }

        /// #717: an access unit too big for a slot must be its own, named
        /// failure — not a generic codec error the tile resyncs at forever.
        #[test]
        fn an_oversize_access_unit_is_named_not_a_codec_error() {
            let mut decoder = H264TileDecoder::new().expect("create decoder");
            let huge = vec![0u8; AU_SLOT_BYTES + 1];
            match decoder.decode_to_rgba(&huge) {
                Err(DecodeFailure::Oversize { bytes, slot }) => {
                    assert_eq!(bytes, AU_SLOT_BYTES + 1);
                    assert_eq!(slot, AU_SLOT_BYTES);
                }
                other => panic!("expected Oversize, got {other:?}"),
            }
        }

        /// #717: arena exhaustion and decoder buffering used to be the same
        /// value. An operator watching a tile lose a third of its frames to a
        /// starved arena saw the same picture as one losing them to the
        /// network; nothing counted either.
        #[test]
        fn arena_exhaustion_is_distinguishable_from_decoder_buffering() {
            // Hold every slot, then require the next acquire to say so rather
            // than report ordinary buffering.
            let mut decoder = H264TileDecoder::new().expect("create decoder");
            let held: Vec<_> = (0..AU_SLOTS)
                .map(|_| decoder.arena.acquire().expect("slot"))
                .collect();
            assert!(
                matches!(decoder.decode_to_rgba(&[0u8; 16]), Ok(Decoded::ArenaFull)),
                "a starved arena must report itself, not masquerade as buffering"
            );
            drop(held);
        }

        fn delta(sequence: u64) -> FrameMeta {
            FrameMeta {
                sequence,
                keyframe: false,
                width: 640,
                height: 480,
                ..Default::default()
            }
        }

        fn idr(sequence: u64) -> FrameMeta {
            FrameMeta {
                keyframe: true,
                ..delta(sequence)
            }
        }

        const DEADLINE: Option<Duration> = Some(Duration::from_millis(1500));

        /// #716: a tile that falls behind sheds late deltas instead of
        /// decoding a backlog it will only ever be further behind.
        #[test]
        fn a_late_delta_frame_is_shed_and_a_late_keyframe_is_not() {
            assert_eq!(
                admit(&delta(2), Some(4_000.0), true, DEADLINE),
                Admit::Shed(Shed::Deadline)
            );
            assert_eq!(
                admit(&idr(2), Some(4_000.0), true, DEADLINE),
                Admit::Decode,
                "a tile on a genuinely slow link must show a slideshow, not a \
                 black rectangle — and the report carries the high age either way"
            );
            assert_eq!(admit(&delta(2), Some(200.0), true, DEADLINE), Admit::Decode);
        }

        /// #716's second acceptance criterion: a stream from a publisher with
        /// timestamping off still plays. An unknown age is not age zero
        /// (RFC 07 §1.3) — reading it as zero would silently disable the
        /// deadline, and clamping the other way would shed everything.
        #[test]
        fn an_unstamped_stream_plays_with_the_deadline_inactive() {
            assert_eq!(admit(&delta(2), None, true, DEADLINE), Admit::Decode);
            assert_eq!(admit(&idr(2), None, true, DEADLINE), Admit::Decode);
        }

        /// A deadline of `None` is off, and off means off — even for a frame
        /// that is minutes old.
        #[test]
        fn no_deadline_admits_a_frame_of_any_age() {
            assert_eq!(admit(&delta(2), Some(600_000.0), true, None), Admit::Decode);
        }

        /// Never feed the decoder before its first IDR: a delta frame with no
        /// reference chain is undecodable, which is a shed of ours and not
        /// loss on the wire.
        #[test]
        fn a_delta_frame_before_the_first_keyframe_is_shed_as_unsynced() {
            assert_eq!(
                admit(&delta(2), Some(10.0), false, DEADLINE),
                Admit::Shed(Shed::Unsynced)
            );
            assert_eq!(
                admit(&idr(2), Some(10.0), false, DEADLINE),
                Admit::Decode,
                "the keyframe is what re-anchors us"
            );
        }

        /// A negative age is clock skew, not a frame from the future to shed.
        #[test]
        fn a_negative_frame_age_never_trips_the_deadline() {
            assert_eq!(admit(&delta(2), Some(-90.0), true, DEADLINE), Admit::Decode);
        }

        /// #717: the tile's queue depth is a real number readable at any
        /// instant — the same quantity the browser tile gets for free from
        /// `VideoDecoder.decodeQueueSize` — rather than something inferred
        /// after the fact from arena exhaustion.
        #[test]
        fn the_queue_depth_counts_what_is_waiting_for_the_decoder() {
            let (jobs, mut drain) = mpsc::channel::<DecodeJob>(DECODE_QUEUE_CAP);
            assert_eq!(queue_depth(&jobs), 0, "an idle tile has nothing pending");

            for sequence in 0..3 {
                jobs.try_send(DecodeJob::Au {
                    payload: Vec::new(),
                    sequence,
                    keyframe: sequence == 0,
                })
                .expect("queue has room");
            }
            assert_eq!(queue_depth(&jobs), 3);

            // Fill it, and require the overflow to be refusable rather than
            // silently buffered: `Full` is what the stream loop turns into a
            // counted shed instead of unbounded latency.
            while jobs.try_send(DecodeJob::Reset).is_ok() {}
            assert_eq!(queue_depth(&jobs), DECODE_QUEUE_CAP as u32);
            assert!(matches!(
                jobs.try_send(DecodeJob::Reset),
                Err(mpsc::error::TrySendError::Full(_))
            ));

            drain.close();
            while drain.try_recv().is_ok() {}
            assert_eq!(queue_depth(&jobs), 0, "a drained queue reports empty");
        }
    }
}
