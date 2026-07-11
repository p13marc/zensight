//! Optional H.264 live view (#409) — behind the `h264` cargo feature.
//!
//! Default builds ship WITHOUT this (openh264 is a C++ build from source —
//! unacceptable unconditionally on GUI/CI/flatpak): [`AVAILABLE`] is `false`
//! and the parallax view renders a "build with `--features h264`" hint. With
//! the feature, [`h264_tile_stream`] subscribes to the exact
//! `@media/<stream>/video/h264/main` key and decodes access units directly
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

    use iced::futures::Stream;
    use iced::widget::image;
    use parallax::converters::{PixelFormat, VideoConvert};
    use parallax::elements::H264Decoder;
    use zenoh::Session;
    use zensight_common::keyexpr::media_video_key;
    use zensight_common::stream::FrameMeta;
    use zensight_common::{Format, Protocol, decode};

    use crate::message::Message;

    /// A stateful H.264 → RGBA frame decoder (pure — unit-testable without
    /// Zenoh). Owns the openh264 decoder plus a cached I420→RGBA converter
    /// keyed by frame dimensions.
    pub struct H264TileDecoder {
        decoder: H264Decoder,
        converter: Option<(u32, u32, VideoConvert)>,
    }

    impl H264TileDecoder {
        pub fn new() -> Result<Self, String> {
            Ok(Self {
                decoder: H264Decoder::new().map_err(|e| e.to_string())?,
                converter: None,
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
            let Some(frame) = self.decoder.decode(nal).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            let (w, h) = (frame.width() as u32, frame.height() as u32);
            let yuv = frame.to_yuv420_planar();
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
            conv.convert(&yuv, &mut rgba).map_err(|e| e.to_string())?;
            Ok(Some((w, h, rgba)))
        }
    }

    /// The per-tile H.264 subscriber stream: decoded video frames as
    /// [`image::Handle`]s. Ends with [`Message::ParallaxTileEnded`]; aborting
    /// the wrapping task drops the future and undeclares the subscriber.
    pub fn h264_tile_stream(
        session: Arc<Session>,
        source: String,
        stream: String,
    ) -> impl Stream<Item = Message> {
        async_stream::stream! {
            // The sensor's default video profile chunk is `main` (the
            // catalogue carries no profile; see zensight-sensor-parallax
            // docs/streams.md).
            let key = media_video_key(Protocol::Parallax, &source, &stream, "h264", "main");
            let subscriber = match session.declare_subscriber(&key).await {
                Ok(s) => s,
                Err(e) => {
                    yield Message::ParallaxTileEnded {
                        stream,
                        error: Some(format!("subscribe failed: {e}")),
                    };
                    return;
                }
            };
            let mut dec = match H264TileDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    yield Message::ParallaxTileEnded { stream, error: Some(e) };
                    return;
                }
            };
            // Never feed the decoder before its first IDR.
            let mut synced = false;
            let mut last_seq: Option<u64> = None;
            loop {
                let sample = match subscriber.recv_async().await {
                    Ok(s) => s,
                    Err(_) => break, // session closed
                };
                let Some(meta) = sample
                    .attachment()
                    .and_then(|a| decode::<FrameMeta>(&a.to_bytes(), Format::Cbor).ok())
                else {
                    continue;
                };
                // A gap means dropped access units: reference frames are
                // gone. Drop sync, rebuild, and ask for a fresh IDR.
                if synced
                    && last_seq.is_some_and(|prev| meta.sequence != prev.wrapping_add(1))
                {
                    synced = false;
                    let _ = dec.reset();
                    yield Message::ParallaxRequestKeyframe {
                        stream: stream.clone(),
                    };
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
                        error: Some("decode task panicked".to_string()),
                    };
                    return;
                };
                dec = dec_back;
                match decoded {
                    Ok(Some((w, h, rgba))) => {
                        yield Message::ParallaxFrame {
                            stream: stream.clone(),
                            seq: meta.sequence,
                            handle: image::Handle::from_rgba(w, h, rgba),
                        };
                    }
                    Ok(None) => {} // decoder buffered; more data coming
                    Err(e) => {
                        tracing::warn!(stream = %stream, error = %e, "h264 decode failed; resyncing");
                        synced = false;
                        let _ = dec.reset();
                        yield Message::ParallaxRequestKeyframe {
                            stream: stream.clone(),
                        };
                    }
                }
            }
            yield Message::ParallaxTileEnded {
                stream,
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
            let mut encoder = H264Encoder::new(
                H264EncoderConfig::new(w, h)
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
                let nal = encoder.encode_yuv420(&yuv).expect("encode frame");
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
            let nal = encoder.encode_yuv420(&yuv).expect("encode idr");
            assert!(
                decoder
                    .decode_to_rgba(&nal)
                    .expect("decode after reset")
                    .is_some()
                    || decoder
                        .decode_to_rgba(&encoder.encode_yuv420(&yuv).expect("encode next"))
                        .expect("decode next")
                        .is_some(),
                "decoder must recover after reset + IDR"
            );
        }
    }
}
