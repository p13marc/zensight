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
pub use real::{H264TileDecoder, h264_tile_stream};

#[cfg(feature = "h264")]
mod real {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use iced::futures::Stream;
    use iced::widget::image;
    use parallax::buffer::{Buffer, MemoryHandle};
    use parallax::converters::{PixelFormat, VideoConvert};
    use parallax::element::Element;
    use parallax::elements::H264Decoder;
    use parallax::memory::SharedArena;
    use parallax::metadata::Metadata;
    use zenoh::Session;
    use zensight_common::keyexpr::media_video_key;
    use zensight_common::stream::FrameMeta;
    use zensight_common::{Format, decode};

    use crate::message::Message;

    /// Minimum spacing between resync `RequestKeyframe` commands (and their
    /// warn lines) from one tile. A stream that fails to decode every AU
    /// otherwise spams one command + one warn per frame (#435); within the
    /// window the tile still drops sync and waits for the next keyframe —
    /// it just doesn't re-ask (or re-warn) for one.
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
    /// One compressed AU at the tier resolutions we publish; a keyframe at the
    /// top tier is the worst case and lands far inside this. An AU larger than
    /// a slot is refused rather than silently truncated — see `decode_to_rgba`.
    const AU_SLOT_BYTES: usize = 1 << 20;

    /// How many AUs may be in flight through the decoder at once. The decoder
    /// holds a reference while it reorders, so this cannot be 1.
    const AU_SLOTS: usize = 8;

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

        /// Decode one access unit; `Ok(Some((w, h, rgba)))` when a picture
        /// comes out (the decoder may buffer: `Ok(None)` = needs more data).
        pub fn decode_to_rgba(
            &mut self,
            nal: &[u8],
        ) -> Result<Option<(u32, u32, Vec<u8>)>, String> {
            if nal.len() > AU_SLOT_BYTES {
                return Err(format!(
                    "access unit of {} bytes exceeds the {AU_SLOT_BYTES}-byte decoder slot",
                    nal.len()
                ));
            }
            // `None` means every slot is still held by the decoder or by a
            // frame the UI has not dropped yet — transient backpressure, not
            // an error, so the tile waits for the next AU rather than resyncing.
            let Some(mut slot) = self.arena.acquire() else {
                return Ok(None);
            };
            slot.data_mut()[..nal.len()].copy_from_slice(nal);
            let input = Buffer::new(MemoryHandle::with_len(slot, nal.len()), Metadata::default());

            let Some(out) = self.decoder.process(input).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            // Geometry travels on the buffer now: `DecodedFrame` is
            // crate-internal in 0.7, and the legacy `"width"`/`"height"`
            // metadata keys carry nothing (#160).
            let (w, h) = out
                .metadata()
                .video_dims()
                .ok_or_else(|| "decoded frame declared no geometry".to_string())?;

            if self
                .converter
                .as_ref()
                .is_none_or(|(cw, ch, _)| (*cw, *ch) != (w, h))
            {
                let conv = VideoConvert::new(PixelFormat::I420, PixelFormat::Rgba, w, h)
                    .map_err(|e| e.to_string())?;
                self.converter = Some((w, h, conv));
            }
            let (_, _, conv) = self.converter.as_ref().expect("converter just cached");
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            // 0.7 takes the input plane layout so a strided frame needs no
            // repack (#196); the decoder hands back packed I420.
            conv.convert(out.as_bytes(), conv.packed_input_layout(), &mut rgba)
                .map_err(|e| e.to_string())?;
            Ok(Some((w, h, rgba)))
        }
    }

    /// The per-tile H.264 subscriber stream: decoded video frames as
    /// [`image::Handle`]s. Ends with [`Message::ParallaxTileEnded`]; aborting
    /// the wrapping task drops the future and undeclares the subscriber.
    /// Every yielded message carries the tile `generation` it was opened with.
    pub fn h264_tile_stream(
        session: Arc<Session>,
        origin: zenkey::RemoteOrigin,
        stream: String,
        tier: String,
        generation: u64,
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
            let mut dec = match H264TileDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    yield Message::ParallaxTileEnded { stream, generation, error: Some(e) };
                    return;
                }
            };
            // Never feed the decoder before its first IDR.
            let mut synced = false;
            let mut last_seq: Option<u64> = None;
            // Backoff for resync keyframe requests (see RESYNC_MIN_INTERVAL);
            // cleared by a successful decode so a fresh failure after a
            // healthy stretch asks immediately.
            let mut last_resync: Option<Instant> = None;
            // Guard against a tile that receives frames but never decodes one
            // (NO_DECODE_TIMEOUT): stamp when the first AU arrives, and whether
            // any displayable frame has ever come out.
            let mut first_frame_at: Option<Instant> = None;
            let mut ever_decoded = false;
            // Whether ANY sample has arrived on this tier's key. Until one does,
            // bound the wait (NO_FIRST_FRAME_TIMEOUT) so a tier that never
            // publishes (open failed on the sensor) ends with a reason.
            let mut any_sample = false;
            loop {
                let sample = if any_sample {
                    match subscriber.recv_async().await {
                        Ok(s) => s,
                        Err(_) => break, // session closed
                    }
                } else {
                    match tokio::time::timeout(NO_FIRST_FRAME_TIMEOUT, subscriber.recv_async())
                        .await
                    {
                        Ok(Ok(s)) => s,
                        Ok(Err(_)) => break, // session closed
                        Err(_) => {
                            yield Message::ParallaxTileEnded {
                                stream,
                                generation,
                                error: Some(
                                    "no video on this tier — the camera may be busy or unavailable"
                                        .to_string(),
                                ),
                            };
                            return;
                        }
                    }
                };
                any_sample = true;
                let Some(meta) = sample
                    .attachment()
                    .and_then(|a| decode::<FrameMeta>(&a.to_bytes(), Format::Cbor).ok())
                else {
                    continue;
                };
                // Frames ARE arriving. If none ever decodes within the window,
                // stop showing a silent black tile and say why.
                let arrived = *first_frame_at.get_or_insert_with(Instant::now);
                if !ever_decoded && arrived.elapsed() >= NO_DECODE_TIMEOUT {
                    yield Message::ParallaxTileEnded {
                        stream,
                        generation,
                        error: Some(format!(
                            "receiving {}×{} video but could not decode this tier",
                            meta.width, meta.height
                        )),
                    };
                    return;
                }
                // A gap means dropped access units: reference frames are
                // gone. Drop sync, rebuild, and ask for a fresh IDR.
                if synced
                    && last_seq.is_some_and(|prev| meta.sequence != prev.wrapping_add(1))
                {
                    synced = false;
                    let _ = dec.reset();
                    if last_resync.is_none_or(|at| at.elapsed() >= RESYNC_MIN_INTERVAL) {
                        last_resync = Some(Instant::now());
                        yield Message::ParallaxRequestKeyframe {
                            stream: stream.clone(),
                        };
                    }
                }
                last_seq = Some(meta.sequence);
                if !synced {
                    if meta.keyframe {
                        synced = true;
                    } else {
                        continue;
                    }
                }
                let payload = sample.payload().to_bytes().to_vec();
                // Decode off the UI thread; the decoder travels with the task.
                let Ok((dec_back, decoded)) = tokio::task::spawn_blocking(move || {
                    let mut d = dec;
                    let r = d.decode_to_rgba(&payload);
                    (d, r)
                })
                .await
                else {
                    yield Message::ParallaxTileEnded {
                        stream,
                        generation,
                        error: Some("decode task panicked".to_string()),
                    };
                    return;
                };
                dec = dec_back;
                match decoded {
                    Ok(Some((w, h, rgba))) => {
                        last_resync = None;
                        ever_decoded = true;
                        yield Message::ParallaxFrame {
                            stream: stream.clone(),
                            generation,
                            seq: meta.sequence,
                            handle: image::Handle::from_rgba(w, h, rgba),
                        };
                    }
                    Ok(None) => {} // decoder buffered; more data coming
                    Err(e) => {
                        synced = false;
                        let _ = dec.reset();
                        if last_resync.is_none_or(|at| at.elapsed() >= RESYNC_MIN_INTERVAL) {
                            last_resync = Some(Instant::now());
                            tracing::warn!(stream = %stream, error = %e, "h264 decode failed; resyncing");
                            yield Message::ParallaxRequestKeyframe {
                                stream: stream.clone(),
                            };
                        } else {
                            tracing::debug!(stream = %stream, error = %e, "h264 decode failed during resync backoff");
                        }
                    }
                }
            }
            yield Message::ParallaxTileEnded {
                stream,
                generation,
                error: None,
            };
        }
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
                if let Some((dw, dh, rgba)) =
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
            assert!(
                decoder
                    .decode_to_rgba(&nal)
                    .expect("decode after reset")
                    .is_some()
                    || decoder
                        .decode_to_rgba(&encoder.encode_yuv420_at(&yuv, w, h).expect("encode next"))
                        .expect("decode next")
                        .is_some(),
                "decoder must recover after reset + IDR"
            );
        }
    }
}
