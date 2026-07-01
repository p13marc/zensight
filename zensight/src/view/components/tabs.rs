//! Generic tabbed container for specialized device views (#243).
//!
//! `tabbed_view` renders a horizontal tab strip over the active tab's content.
//! It is the in-view counterpart to the host-level facet strip in `device.rs`:
//! where facets switch *which sensor* you're looking at, tabs switch *which
//! question* you're asking about one sensor (ntopng / Corelith model).
//!
//! The container is generic over a `Copy + Eq` tab id (usually a small enum) so
//! both the netring and netlink redesigns can supply their own tab sets. Only
//! the active tab's `content` is built by the caller, so hidden tabs cost
//! nothing. Tabs whose `visible` flag is false are omitted from the strip
//! entirely (capability-aware visibility, e.g. no DNS tab without `dns/` data).

use iced::widget::{Row, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length};

use crate::view::components::kit;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// One entry in the tab strip. `content` is intentionally absent — the caller
/// builds only the active tab's body, keyed off the returned selection.
#[derive(Debug, Clone)]
pub struct TabItem<Id> {
    /// Tab identity (a `Copy + Eq` enum value).
    pub id: Id,
    /// Human-readable label shown on the tab.
    pub label: String,
    /// Optional count rendered as a chip after the label (e.g. firing anomalies).
    /// Zero renders no chip.
    pub badge: Option<usize>,
    /// Whether the tab is shown at all (capability-aware).
    pub visible: bool,
}

impl<Id> TabItem<Id> {
    /// A plain, always-visible tab.
    pub fn new(id: Id, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            badge: None,
            visible: true,
        }
    }

    /// Set capability-aware visibility.
    pub fn visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Attach a count chip (rendered only when `> 0`).
    pub fn badge(mut self, count: usize) -> Self {
        self.badge = Some(count);
        self
    }
}

/// Render a tabbed view: a strip of `tabs` over `content` (the body of the
/// currently `active` tab). `on_select` maps a tab id to the message emitted
/// when an inactive tab is clicked.
pub fn tabbed_view<'a, Id, Message>(
    tabs: &[TabItem<Id>],
    active: Id,
    content: Element<'a, Message>,
    on_select: impl Fn(Id) -> Message,
) -> Element<'a, Message>
where
    Id: Copy + Eq,
    Message: Clone + 'a,
{
    let mut strip = Row::new().spacing(space::XS).align_y(Alignment::Center);
    for tab in tabs.iter().filter(|t| t.visible) {
        let is_active = tab.id == active;
        let mut label_row = row![text(tab.label.clone()).size(font::CAPTION)]
            .spacing(space::XS)
            .align_y(Alignment::Center);
        if let Some(count) = tab.badge
            && count > 0
        {
            label_row = label_row.push(kit::badge(theme::ACCENT_ANOMALY, format!("{count}")));
        }
        let btn = if is_active {
            button(label_row)
                .padding([space::XS as u16, space::SM as u16])
                .style(button::primary)
        } else {
            button(label_row)
                .padding([space::XS as u16, space::SM as u16])
                .on_press(on_select(tab.id))
                .style(button::text)
        };
        strip = strip.push(btn);
    }

    column![
        container(scrollable(strip).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::hidden(),
        )))
        .width(Length::Fill),
        rule::horizontal(1),
        content,
    ]
    .spacing(space::SM)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_test::simulator;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DummyTab {
        A,
        B,
        C,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Msg {
        Select(DummyTab),
    }

    fn tabs() -> Vec<TabItem<DummyTab>> {
        vec![
            TabItem::new(DummyTab::A, "Alpha"),
            TabItem::new(DummyTab::B, "Bravo"),
            // Hidden: no data for this tab.
            TabItem::new(DummyTab::C, "Charlie").visible(false),
        ]
    }

    #[test]
    fn renders_visible_tabs_and_active_body() {
        let items = tabs();
        let view = tabbed_view(
            &items,
            DummyTab::A,
            text("body-of-alpha").into(),
            Msg::Select,
        );
        let mut ui = simulator(view);
        assert!(ui.find("Alpha").is_ok());
        assert!(ui.find("Bravo").is_ok());
        // Hidden tab is omitted from the strip entirely.
        assert!(ui.find("Charlie").is_err());
        // Active tab's body is shown.
        assert!(ui.find("body-of-alpha").is_ok());
    }

    #[test]
    fn clicking_inactive_tab_emits_select() {
        let items = tabs();
        let view = tabbed_view(
            &items,
            DummyTab::A,
            text("body").into(),
            Msg::Select,
        );
        let mut ui = simulator(view);
        let _ = ui.click("Bravo");
        let msgs: Vec<Msg> = ui.into_messages().collect();
        assert!(msgs.contains(&Msg::Select(DummyTab::B)));
    }
}
