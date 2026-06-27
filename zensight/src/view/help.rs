//! Keyboard-shortcuts help overlay (#28).
//!
//! A small, centered cheat-sheet of the app's global shortcuts, toggled with
//! `?` (and dismissed with `?` again, `Esc`, or the Close button). Mirrors the
//! global-search overlay's card styling so the two feel consistent.

use iced::widget::{Column, column, container, row, text};
use iced::{Element, Length};

use crate::message::Message;
use crate::view::tokens::{font, space};

/// The shortcuts shown in the overlay, as `(keys, description)` rows. Kept here
/// next to the overlay so the cheat-sheet and the real bindings live together;
/// the bindings themselves are in `subscription::keyboard_subscription`.
const SHORTCUTS: &[(&str, &str)] = &[
    ("?", "Toggle this help"),
    ("Ctrl + P", "Open the command palette"),
    ("Ctrl + K", "Search metrics across all devices"),
    ("Ctrl + F", "Focus the device search"),
    ("Esc", "Close dialog / go back"),
    ("+ / -", "Zoom a chart in / out (when focused)"),
    ("← / →", "Pan a focused chart"),
    ("Home", "Reset a focused chart's view"),
];

/// Render the centered keyboard-shortcuts overlay card.
pub fn help_overlay<'a>() -> Element<'a, Message> {
    let header = row![
        text("Keyboard Shortcuts").size(font::SECTION),
        container(text("")).width(Length::Fill),
        iced_anim::widget::button(text("Close").size(font::CAPTION))
            .on_press(Message::ToggleHelp)
            .padding([space::XS, space::SM])
            .style(iced::widget::button::secondary),
    ]
    .align_y(iced::Alignment::Center)
    .spacing(space::SM);

    let mut rows = Column::new().spacing(space::XS);
    for (keys, desc) in SHORTCUTS {
        rows = rows.push(
            row![
                container(text(*keys).size(font::CAPTION).font(iced::Font::MONOSPACE))
                    .width(Length::Fixed(110.0)),
                text(*desc).size(font::CAPTION),
            ]
            .spacing(space::SM)
            .align_y(iced::Alignment::Center),
        );
    }

    container(column![header, rows].spacing(space::SM).padding(space::MD))
        .width(Length::Fixed(420.0))
        .style(iced::widget::container::rounded_box)
        .into()
}
