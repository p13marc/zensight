//! Layout/structure primitives shared across views: card, section header, badge,
//! and empty state. Each is theme-aware (reads [`crate::view::theme`]) and draws
//! dimensions from [`crate::view::tokens`], so views compose consistent chrome
//! instead of hand-rolling containers with ad-hoc colors and spacing.
//!
//! See docs/plans/gui/03-design-system.md (D3).

use iced::widget::{container, row, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};

use crate::view::theme;
use crate::view::tokens::{font, space};

/// Convert a stored `(r, g, b)` triple (0.0–1.0) into an [`iced::Color`]. Use
/// for *data* colors (chart series, threshold lines, group tags) so views never
/// call `Color::from_rgb` directly — keeping the D2 "no ad-hoc colors" guard clean.
pub fn rgb((r, g, b): (f32, f32, f32)) -> Color {
    Color::from_rgb(r, g, b)
}

/// Like [`rgb`] but with an explicit alpha (for translucent data fills).
pub fn rgba((r, g, b): (f32, f32, f32), alpha: f32) -> Color {
    Color::from_rgba(r, g, b, alpha)
}

/// A card surface: a padded container with a subtle border and raised background.
/// Use to group a logical section instead of a bare `column!` of rows.
pub fn card<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(space::MD)
        .width(Length::Fill)
        .style(|theme: &Theme| {
            let c = theme::colors(theme);
            container::Style {
                background: Some(Background::Color(c.card_background())),
                border: Border {
                    color: c.border_subtle(),
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// A section header: a `SECTION`-size title with optional trailing actions
/// pushed to the right edge.
pub fn section_header<'a, Message: 'a>(
    title: impl text::IntoFragment<'a>,
    actions: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let title = text(title).size(font::SECTION);
    match actions {
        Some(actions) => {
            // A fill container pushes the actions to the right edge.
            let spacer = container(text("")).width(Length::Fill);
            row![title, spacer, actions]
                .align_y(Alignment::Center)
                .spacing(space::SM)
                .into()
        }
        None => title.into(),
    }
}

/// A status/severity badge: a colored dot **plus** a text label, so meaning is
/// never carried by color alone (accessibility). `color` should come from a
/// `theme::colors(..).status_*()` / `severity_*()` helper.
pub fn badge<'a, Message: 'a>(
    color: Color,
    label: impl text::IntoFragment<'a>,
) -> Element<'a, Message> {
    let dot = container(text(""))
        .width(8)
        .height(8)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border::default().rounded(4.0),
            ..Default::default()
        });
    row![dot, text(label).size(font::CAPTION)]
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .into()
}

/// An empty-state placeholder: a centered, muted message with optional action,
/// for "no data yet" / "no match" panels. Replaces bare strings of varying size.
pub fn empty_state<'a, Message: 'a>(
    message: impl text::IntoFragment<'a>,
    action: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let label = text(message)
        .size(font::BODY)
        .style(|theme: &Theme| text::Style {
            color: Some(theme::colors(theme).text_muted()),
        });

    let inner: Element<'a, Message> = match action {
        Some(action) => iced::widget::column![label, action]
            .spacing(space::SM)
            .align_x(Alignment::Center)
            .into(),
        None => label.into(),
    };

    container(inner)
        .width(Length::Fill)
        .padding(space::LG)
        .center_x(Length::Fill)
        .into()
}
