//! Parallax (live video / media plane) specialized view (#359).
//!
//! ZenSight's media plane (issue #359) carries opaque encoded frames on the
//! `@media/<stream>/…` keyspace — a sibling of the `@/` control plane, invisible
//! to the telemetry firehose. This view is the frontend seam for it: it renders
//! a JPEG **preview** frame as an [`iced::widget::image`].
//!
//! Scope note (#359): this issue delivers the *zenoh-side enabler* — keyspace,
//! QoS, the raw media publisher, and stream-control types. The live
//! subscription pipeline (a media subscriber feeding the newest preview frame
//! per stream into app state) and the H.264/parallax daemon are **out of
//! scope**, so with no frame wired in yet this view shows a generated
//! "no signal" placeholder. Once the media subscriber lands, pass the received
//! JPEG bytes through [`preview_handle_from_jpeg`] and hand the resulting
//! handle to [`preview_frame`].

use iced::widget::{column, container, image, text};
use iced::{Element, Length, Theme};

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::tokens::space;

/// Preview frame dimensions for the placeholder tile (16:9).
const PREVIEW_W: u32 = 320;
const PREVIEW_H: u32 = 180;

/// Decode an encoded JPEG preview frame (received off the
/// `@media/<stream>/preview/jpeg` key) into an iced image handle, or `None` if
/// the bytes aren't a decodable JPEG. We decode to RGBA with the `image` crate
/// (jpeg codec only) rather than leaning on an in-iced decoder, so iced needs no
/// codec features (and AVIF/ravif never enter the build).
///
/// This is the wiring point for the (out-of-scope) live media subscriber: once
/// the newest preview frame per stream is threaded into app state, feed its
/// bytes here and render with [`preview_frame`].
pub fn preview_handle_from_jpeg(bytes: &[u8]) -> Option<image::Handle> {
    let rgba = ::image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(image::Handle::from_rgba(w, h, rgba.into_raw()))
}

/// A generated dark "no signal" tile, used until a live preview frame is wired
/// in. Rendered as raw RGBA so it exercises the same image widget the live path
/// will use — no external asset, no decode.
fn placeholder_frame() -> image::Handle {
    let (w, h) = (PREVIEW_W as usize, PREVIEW_H as usize);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            // Subtle diagonal gradient so the tile reads as an intentional
            // placeholder rather than a black rectangle.
            let v = (((x + y) % 48) as u8) / 3 + 18;
            pixels.extend_from_slice(&[v, v, v.saturating_add(8), 0xFF]);
        }
    }
    image::Handle::from_rgba(PREVIEW_W, PREVIEW_H, pixels)
}

/// Render a preview frame (live or placeholder) as a bounded image widget.
pub fn preview_frame<'a>(handle: image::Handle) -> Element<'a, Message> {
    container(
        image(handle)
            .width(Length::Fixed(PREVIEW_W as f32))
            .height(Length::Fixed(PREVIEW_H as f32)),
    )
    .padding(space::MD)
    .into()
}

/// Specialized view for a Parallax media source.
pub fn parallax_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let source = state.device_id.source.as_str();

    let header = text(format!("Live media — {source}")).size(16);
    let subtitle = text(
        "Opaque video/preview frames ride the @media plane (zensight/parallax/<source>/@media/<stream>/…). \
         Live-frame subscription is not wired in the GUI yet (#359 delivers the zenoh-side enabler); \
         showing a placeholder preview.",
    )
    .size(12)
    .style(|t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    });

    column![
        icons::protocol_parallax::<Message>(IconSize::Medium),
        header,
        preview_frame(placeholder_frame()),
        subtitle,
    ]
    .spacing(space::SM)
    .padding(space::LG)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_frame_builds() {
        // Constructing the raw-RGBA placeholder must not panic.
        let _ = placeholder_frame();
    }

    #[test]
    fn rejects_non_jpeg_bytes() {
        assert!(preview_handle_from_jpeg(b"definitely not a jpeg").is_none());
    }

    #[test]
    fn decodes_a_real_jpeg_frame() {
        // Encode a small tile to JPEG, then round-trip it through the decoder
        // the live media path would use.
        let tile = ::image::RgbaImage::from_pixel(8, 8, ::image::Rgba([120, 30, 200, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgba8(tile)
            .write_to(&mut buf, ::image::ImageFormat::Jpeg)
            .expect("encode jpeg");
        assert!(
            preview_handle_from_jpeg(buf.get_ref()).is_some(),
            "a valid JPEG preview frame must decode to an image handle"
        );
    }
}
