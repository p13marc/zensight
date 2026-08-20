//! Parallax (live video / media plane) specialized view (#408, epic #402).
//!
//! Renders the sensor's stream catalogue (fetched on demand from
//! `@rpc/parallax/streams`) and a grid of live tiles. Each catalogue row shows
//! one **Live** button per offered bandwidth tier (#494/#502): clicking a tier
//! sends `open_stream` (codec `h264`, that tier) and spawns an abortable
//! subscriber on the exact `@media/<stream>/video/h264/<tier>` key (see
//! `parallax_h264.rs`); decoded frames land here as an [`iced::widget::image`]
//! with a resolution/fps caption. One tile per stream — picking a different
//! tier replaces it. Closing (or leaving the view) aborts the subscriber and
//! sends `close_stream`. (The low-cost JPEG preview path still exists for demo
//! mode and the expand overlay; it is no longer a catalogue action.)

use iced::widget::{Space, button, column, container, image, mouse_area, row, text, tooltip};
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

/// Wrap a catalogue action button with a hover tooltip spelling out what a
/// click opens (a bare tier name like `high` isn't self-explanatory).
fn action_tooltip<'a>(
    control: impl Into<Element<'a, Message>>,
    hint: String,
) -> Element<'a, Message> {
    tooltip(
        control,
        container(text(hint).size(11))
            .padding(6)
            .style(container::rounded_box),
        tooltip::Position::Top,
    )
    .into()
}

/// The applied-bandwidth suffix for a live video tile (#503): its tier's
/// applied resolution and bitrate, straight from the sensor's per-tier status
/// (not a client-side arrival EMA). `None` for previews or an unreported tier.
fn tier_readout(detail: &ParallaxDetailState, name: &str, tile: &TileState) -> Option<String> {
    let tier = tile.selected_tier.as_deref()?;
    let applied = detail.applied_tier(name, tier)?;
    Some(format!(
        "{tier} · {}×{} · {} kbps",
        applied.applied.width, applied.applied.height, applied.applied.bitrate_kbps
    ))
}

/// Is this stream's bitrate cap biting *right now*?
///
/// `{stream}/stats/rc_drops` counts frames the encoder's rate control swallowed
/// to hold the tier's `bitrate_kbps` (#510). It is cumulative, so a non-zero
/// value only says the cap bit at *some* point; growth between the two most
/// recent samples is what says it is biting now.
///
/// The counter is per stream, summed over its open tiers — but the GUI opens
/// one tier per stream, so it is that tier's.
fn rc_capped(state: &DeviceDetailState, stream: &str) -> bool {
    let history = match state.history.get(&format!("{stream}/stats/rc_drops")) {
        Some(h) => h,
        None => return false,
    };
    let mut counters = history.iter().rev().filter_map(|p| match p.value {
        zensight_common::TelemetryValue::Counter(n) => Some(n),
        _ => None,
    });
    match (counters.next(), counters.next()) {
        (Some(latest), Some(previous)) => latest > previous,
        _ => false,
    }
}

/// A tier button's short label: `<name> ≤<cap>p` (or `<name> native` when the
/// tier is uncapped). The full resolution/fps/bitrate lives in the tooltip.
fn tier_button_label(spec: &zensight_common::stream::TierSpec) -> String {
    match spec.max_height {
        Some(h) => format!("{} ≤{h}p", spec.name),
        None => format!("{} native", spec.name),
    }
}

/// The tier button's tooltip: what a click actually opens.
fn tier_tooltip(spec: &zensight_common::stream::TierSpec) -> String {
    let cap = match spec.max_height {
        Some(h) => format!("≤{h}p"),
        None => "native".to_string(),
    };
    format!(
        "Live H.264 · {cap} · {} fps · {} kbps",
        spec.fps, spec.bitrate_kbps
    )
}

/// One catalogue row: name · native geometry · a Live button per offered tier
/// (#494/#502) · Close. Each tier button opens Live directly on that tier; the
/// currently-live tier reads as selected (disabled). One tile per stream, so
/// picking a different tier replaces it.
fn catalogue_row<'a>(
    detail: &'a ParallaxDetailState,
    stream: &'a zensight_common::StreamDescriptor,
) -> Element<'a, Message> {
    let open = detail.is_open(&stream.stream);
    let live_tier = detail
        .tiles
        .get(&stream.stream)
        .and_then(|t| t.selected_tier.clone());
    // One button per offered tier (only with the H.264 decoder — otherwise the
    // section header carries the build hint). The active tier's button is
    // disabled so it reads as "currently live".
    let controls: Element<'a, Message> = if parallax_h264::AVAILABLE {
        let mut buttons = row![].spacing(space::XS);
        for spec in &stream.tiers {
            let is_live = live_tier.as_deref() == Some(spec.name.as_str());
            let mut b = button(text(tier_button_label(spec)).size(12));
            if !is_live {
                b = b.on_press(Message::ParallaxOpenVideoTile {
                    stream: stream.stream.clone(),
                    tier: spec.name.clone(),
                });
            }
            buttons = buttons.push(action_tooltip(b, tier_tooltip(spec)));
        }
        if open {
            buttons = buttons.push(button(text("Close").size(12)).on_press(
                Message::ParallaxCloseTile {
                    stream: stream.stream.clone(),
                },
            ));
        }
        buttons.into()
    } else {
        Space::new().width(Length::Shrink).into()
    };
    let mut cells = row![text(&stream.stream).size(14).width(Length::Fixed(140.0)),]
        .spacing(space::SM)
        .align_y(iced::Alignment::Center);
    // Native geometry (capability-bearing catalogue, #507) so the offered tiers
    // read as honest — no 720p tier on a 480p camera.
    if let (Some(w), Some(h)) = (stream.width, stream.height) {
        cells = cells.push(
            text(format!("{w}×{h}"))
                .size(12)
                .style(muted)
                .width(Length::Fixed(80.0)),
        );
    }
    if stream.active {
        cells = cells.push(text("live").size(12).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).success()),
        }));
    }
    if let Some(description) = &stream.description {
        cells = cells.push(text(description).size(12).style(muted));
    }
    cells = cells.push(Space::new().width(Length::Fill)).push(controls);
    cells.into()
}

/// One live preview tile: frame (or placeholder / end reason) + caption.
/// Clicking the frame expands the tile to the near-fullscreen overlay (#436).
fn tile<'a>(
    detail: &'a ParallaxDetailState,
    name: &'a str,
    tile: &'a TileState,
    capped: bool,
) -> Element<'a, Message> {
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
        // For a video tile, prefer the sensor's applied per-tier readout
        // (resolution + bitrate, #503) over the client-side fps EMA alone.
        // `· capped` says the encoder is shedding frames to hold the tier's
        // bitrate — the difference between "this tier looks soft" and "this
        // tier is soft *because you asked for 400 kbps*". Grid captions only:
        // this is the surface where several streams are compared at once.
        let cap = if capped { " · capped" } else { "" };
        match tier_readout(detail, name, tile) {
            Some(readout) => format!("{name} · {readout} · {:.1} fps{cap}", tile.fps),
            None => format!("{name} · seq {} · {:.1} fps{cap}", tile.last_seq, tile.fps),
        }
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
        match tier_readout(detail, name, tile) {
            Some(readout) => format!("{name} · {readout} · {:.1} fps", tile.fps),
            None => format!(
                "{name} · {profile} · seq {} · {:.1} fps",
                tile.last_seq, tile.fps
            ),
        }
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

    // Stream catalogue: each stream offers a Live button per tier (#494/#502).
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
        content = content.push(text("Live view").size(14));
        let tiles: Vec<_> = detail.tiles.iter().collect();
        for chunk in tiles.chunks(TILES_PER_ROW) {
            let mut grid_row = row![].spacing(space::MD);
            for (name, tile_state) in chunk {
                grid_row = grid_row.push(tile(detail, name, tile_state, rc_capped(state, name)));
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

    /// `rc_drops` is cumulative, so a non-zero value alone only says the cap
    /// bit at *some* point in this stream's life. The caption marker is about
    /// now, so it keys off growth between the two most recent samples.
    #[test]
    fn rc_capped_reads_growth_not_the_absolute_counter() {
        use crate::view::device::DeviceDetailState;
        use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

        fn state_with(samples: &[u64]) -> DeviceDetailState {
            let mut state = DeviceDetailState::new(crate::message::DeviceId {
                protocol: Protocol::Parallax,
                origin: "h-000000000000".to_string(),
                source: "cam-host".to_string(),
            });
            state.history.insert(
                "cam0/stats/rc_drops".to_string(),
                samples
                    .iter()
                    .map(|n| {
                        TelemetryPoint::new(
                            "cam-host",
                            Protocol::Parallax,
                            "cam0/stats/rc_drops",
                            TelemetryValue::Counter(*n),
                        )
                    })
                    .collect(),
            );
            state
        }

        assert!(
            rc_capped(&state_with(&[7, 11]), "cam0"),
            "the counter grew between the last two ticks — the cap is biting now"
        );
        assert!(
            !rc_capped(&state_with(&[11, 11]), "cam0"),
            "a large but static counter means the cap bit earlier, not now"
        );
        assert!(
            !rc_capped(&state_with(&[4]), "cam0"),
            "one sample cannot show growth"
        );
        assert!(
            !rc_capped(&state_with(&[]), "cam0"),
            "a stream with no rate control publishes nothing to read"
        );
    }

    #[test]
    fn expanded_overlay_renders_caption_and_close() {
        use iced_test::simulator;

        let mut detail = ParallaxDetailState::default();
        let generation = detail.allocate_generation();
        detail.open_tile("cam0", generation, None, true, Some("high".to_string()));
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

    fn two_tier_descriptor() -> zensight_common::StreamDescriptor {
        use zensight_common::stream::TierSpec;
        zensight_common::StreamDescriptor {
            stream: "cam0".into(),
            codecs: vec!["h264".into()],
            active: false,
            width: Some(1280),
            height: Some(720),
            fps: Some(30.0),
            tiers: vec![
                TierSpec {
                    name: "medium".into(),
                    max_height: Some(480),
                    fps: 20,
                    bitrate_kbps: 1200,
                },
                TierSpec {
                    name: "high".into(),
                    max_height: None,
                    fps: 30,
                    bitrate_kbps: 4000,
                },
            ],
            description: None,
        }
    }

    #[test]
    fn tier_button_labels_show_cap_or_native() {
        let specs = two_tier_descriptor().tiers;
        assert_eq!(tier_button_label(&specs[0]), "medium ≤480p");
        assert_eq!(tier_button_label(&specs[1]), "high native");
    }

    #[test]
    fn catalogue_row_opens_the_clicked_tier() {
        use iced_test::simulator;

        // With the H.264 decoder the row shows one Live button per tier; each
        // dispatches ParallaxOpenVideoTile for THAT exact tier. (Without the
        // decoder `parallax_h264::AVAILABLE` is false and the row shows no tier
        // buttons — so this assertion only holds in an h264-feature build.)
        if !parallax_h264::AVAILABLE {
            return;
        }
        let mut detail = ParallaxDetailState::default();
        detail.apply(Ok(vec![two_tier_descriptor()]));
        let descriptor = two_tier_descriptor();

        let mut ui = simulator(catalogue_row(&detail, &descriptor));
        assert!(ui.find("medium ≤480p").is_ok(), "a Live button per tier");
        assert!(ui.find("high native").is_ok());
        let _ = ui.click("high native");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                Message::ParallaxOpenVideoTile { stream, tier } if stream == "cam0" && tier == "high"
            )),
            "clicking a tier opens Live on exactly that tier"
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
