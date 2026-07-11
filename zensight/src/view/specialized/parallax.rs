//! Parallax (live video / media plane) specialized view (#408, epic #402).
//!
//! Renders the sensor's stream catalogue (fetched on demand from
//! `@/query/streams`) and a grid of live JPEG preview tiles. Opening a tile
//! sends `open_stream` (codec `mjpeg`) and spawns an abortable subscriber
//! task on the exact `@media/<stream>/preview/jpeg` key (see
//! `parallax_detail.rs`); each decoded frame lands here as an
//! [`iced::widget::image`] with a seq/fps caption. Closing (or leaving the
//! view) aborts the subscriber and sends `close_stream`.

use iced::widget::{Space, button, column, container, image, mouse_area, row, text};
use iced::{ContentFit, Element, Length, Theme};

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::icons::{self, IconSize};
use crate::view::specialized::fetch::Fetch;
use crate::view::specialized::parallax_detail::{ParallaxDetailState, TileState};
use crate::view::specialized::parallax_h264;
use crate::view::theme;
use crate::view::tokens::space;

/// Preview frame dimensions for the placeholder tile (16:9).
const PREVIEW_W: u32 = 320;
const PREVIEW_H: u32 = 180;

/// Tiles per grid row.
const TILES_PER_ROW: usize = 3;

/// Decode an encoded JPEG preview frame (received off the
/// `@media/<stream>/preview/jpeg` key) into an iced image handle, or `None` if
/// the bytes aren't a decodable JPEG. We decode to RGBA with the `image` crate
/// (jpeg codec only) rather than leaning on an in-iced decoder, so iced needs no
/// codec features (and AVIF/ravif never enter the build).
pub fn preview_handle_from_jpeg(bytes: &[u8]) -> Option<image::Handle> {
    let rgba = ::image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(image::Handle::from_rgba(w, h, rgba.into_raw()))
}

/// A generated dark "no signal" tile, shown while a tile waits for its first
/// frame (and for demo-mode tiles, which have no live subscriber). Rendered
/// as raw RGBA so it exercises the same image widget as the live path — no
/// external asset, no decode.
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
    .into()
}

fn muted(t: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(t).text_muted()),
    }
}

/// One catalogue row: name · description · codecs · live badge · open/close.
fn catalogue_row<'a>(
    detail: &'a ParallaxDetailState,
    stream: &'a zensight_common::StreamDescriptor,
) -> Element<'a, Message> {
    let open = detail.is_open(&stream.stream);
    let toggle: Element<'a, Message> = if open {
        button(text("Close").size(12))
            .on_press(Message::ParallaxCloseTile {
                stream: stream.stream.clone(),
            })
            .into()
    } else {
        let mut buttons = row![
            button(text("Open").size(12)).on_press(Message::ParallaxOpenTile {
                stream: stream.stream.clone(),
            })
        ]
        .spacing(space::XS);
        // The H.264 live view exists only on `--features h264` builds (#409);
        // without it the section header carries the build hint instead.
        if parallax_h264::AVAILABLE {
            buttons = buttons.push(button(text("Video").size(12)).on_press(
                Message::ParallaxOpenVideoTile {
                    stream: stream.stream.clone(),
                },
            ));
        }
        buttons.into()
    };
    let mut cells = row![
        text(&stream.stream).size(14).width(Length::Fixed(140.0)),
        text(stream.codecs.join(" · "))
            .size(12)
            .style(muted)
            .width(Length::Fixed(110.0)),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);
    if stream.active {
        cells = cells.push(text("live").size(12).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).success()),
        }));
    }
    if let Some(description) = &stream.description {
        cells = cells.push(text(description).size(12).style(muted));
    }
    cells = cells.push(Space::new().width(Length::Fill)).push(toggle);
    cells.into()
}

/// One live preview tile: frame (or placeholder / end reason) + caption.
/// Clicking the frame expands the tile to the near-fullscreen overlay (#436).
fn tile<'a>(name: &'a str, tile: &'a TileState) -> Element<'a, Message> {
    let picture: Element<'a, Message> = match (&tile.frame, &tile.ended) {
        (Some(handle), _) => preview_frame(handle.clone()),
        (None, Some(reason)) => container(text(reason.as_str()).size(12).style(muted))
            .width(Length::Fixed(PREVIEW_W as f32))
            .height(Length::Fixed(PREVIEW_H as f32))
            .center(Length::Fill)
            .into(),
        (None, None) => preview_frame(placeholder_frame()),
    };
    let frame: Element<'a, Message> = mouse_area(picture)
        .on_press(Message::ParallaxExpandTile {
            stream: name.to_string(),
        })
        .interaction(iced::mouse::Interaction::Pointer)
        .into();
    let caption = if let Some(reason) = &tile.ended {
        format!("{name} — {reason}")
    } else if tile.frame.is_some() {
        format!("{name} · seq {} · {:.1} fps", tile.last_seq, tile.fps)
    } else {
        format!("{name} · waiting for frames…")
    };
    column![
        frame,
        row![
            text(caption).size(12).style(muted),
            Space::new().width(Length::Fill),
            button(text("Close").size(11)).on_press(Message::ParallaxCloseTile {
                stream: name.to_string(),
            }),
        ]
        .spacing(space::SM)
        .align_y(iced::Alignment::Center)
        .width(Length::Fixed(PREVIEW_W as f32)),
    ]
    .spacing(space::XS)
    .into()
}

/// The near-fullscreen expanded-tile overlay (#436): a dimming scrim (click
/// outside to dismiss) around the stream's latest frame scaled up, with a
/// caption + Close button. `None` while nothing is expanded — and a closed
/// or torn-down tile dismisses the overlay implicitly (`expanded_tile`).
pub fn expanded_overlay(detail: &ParallaxDetailState) -> Option<Element<'_, Message>> {
    let (name, tile) = detail.expanded_tile()?;
    let picture: Element<'_, Message> = image(tile.frame.clone().unwrap_or_else(placeholder_frame))
        .width(Length::Fill)
        .height(Length::Fill)
        .content_fit(ContentFit::Contain)
        .into();
    let profile = if tile.video { "H.264" } else { "preview" };
    let caption = if let Some(reason) = &tile.ended {
        format!("{name} · {profile} — {reason}")
    } else if tile.frame.is_some() {
        format!(
            "{name} · {profile} · seq {} · {:.1} fps",
            tile.last_seq, tile.fps
        )
    } else {
        format!("{name} · {profile} · waiting for frames…")
    };
    let header = row![
        text(caption).size(14),
        Space::new().width(Length::Fill),
        button(text("Close").size(12)).on_press(Message::ParallaxCollapseTile),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);
    let card = container(
        column![header, picture]
            .spacing(space::SM)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .padding(space::MD)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|t: &Theme| iced::widget::container::Style {
        background: Some(theme::colors(t).background_strongest().into()),
        ..iced::widget::container::rounded_box(t)
    });
    // The scrim ring around the card is the click-outside surface; the card
    // itself is opaque, so clicks on it (Close, the picture) never fall
    // through to the dismissing mouse_area.
    Some(
        mouse_area(
            container(iced::widget::opaque(card))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(space::XL)
                .style(|t: &Theme| iced::widget::container::Style {
                    background: Some(theme::colors(t).scrim().into()),
                    ..Default::default()
                }),
        )
        .on_press(Message::ParallaxCollapseTile)
        .into(),
    )
}

/// Specialized view for a Parallax media source: stream catalogue + live
/// preview tile grid.
pub fn parallax_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let source = state.device_id.source.as_str();
    let detail = &state.parallax_detail;

    let header = row![
        icons::protocol_parallax::<Message>(IconSize::Medium),
        text(format!("Live media — {source}")).size(16),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let mut content = column![header].spacing(space::MD).padding(space::LG);

    // Stream catalogue.
    content = content.push(text("Streams").size(14));
    if !parallax_h264::AVAILABLE {
        content = content.push(text(parallax_h264::UNAVAILABLE_HINT).size(11).style(muted));
    }
    match &detail.catalogue {
        Fetch::Idle => {
            content = content.push(
                row![
                    text("Stream catalogue not loaded.").size(12).style(muted),
                    button(text("Load streams").size(12)).on_press(Message::FetchParallaxStreams),
                ]
                .spacing(space::SM)
                .align_y(iced::Alignment::Center),
            );
        }
        Fetch::Loading => {
            content = content.push(text("Loading stream catalogue…").size(12).style(muted));
        }
        Fetch::Error(message) => {
            content = content.push(
                row![
                    text(format!("Catalogue unavailable: {message}"))
                        .size(12)
                        .style(|t: &Theme| text::Style {
                            color: Some(theme::colors(t).danger_text()),
                        }),
                    button(text("Retry").size(12)).on_press(Message::FetchParallaxStreams),
                ]
                .spacing(space::SM)
                .align_y(iced::Alignment::Center),
            );
        }
        Fetch::Ready(streams) if streams.is_empty() => {
            content = content.push(
                text("This sensor advertises no streams.")
                    .size(12)
                    .style(muted),
            );
        }
        Fetch::Ready(streams) => {
            let mut list = column![].spacing(space::XS);
            for stream in streams {
                list = list.push(catalogue_row(detail, stream));
            }
            content = content.push(list);
        }
    }

    // Live preview tiles.
    if detail.tiles.is_empty() {
        content = content.push(
            text("No previews open — open a stream above to watch its live preview.")
                .size(12)
                .style(muted),
        );
    } else {
        content = content.push(text("Previews").size(14));
        let tiles: Vec<_> = detail.tiles.iter().collect();
        for chunk in tiles.chunks(TILES_PER_ROW) {
            let mut grid_row = row![].spacing(space::MD);
            for (name, tile_state) in chunk {
                grid_row = grid_row.push(tile(name, tile_state));
            }
            content = content.push(grid_row);
        }
    }

    iced::widget::scrollable(content).into()
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
    fn expanded_overlay_renders_caption_and_close() {
        use iced_test::simulator;

        let mut detail = ParallaxDetailState::default();
        let generation = detail.allocate_generation();
        detail.open_tile("cam0", generation, None, true);
        assert!(
            expanded_overlay(&detail).is_none(),
            "no overlay while nothing is expanded"
        );

        detail.expand("cam0");
        let overlay = expanded_overlay(&detail).expect("overlay for the expanded tile");
        let mut ui = simulator(overlay);
        assert!(
            ui.find("cam0 · H.264 · waiting for frames…").is_ok(),
            "caption names the stream + profile"
        );
        let _ = ui.click("Close");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Message::ParallaxCollapseTile)),
            "Close dismisses the overlay"
        );
    }

    #[test]
    fn decodes_a_real_jpeg_frame() {
        // Encode a small tile to JPEG, then round-trip it through the decoder
        // the live media path uses.
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
