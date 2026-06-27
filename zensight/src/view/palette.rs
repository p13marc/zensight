//! Command palette (#28): a fuzzy-searchable list of navigation targets and
//! actions, opened with **Ctrl+P**. Picking a command dispatches its message
//! and closes the palette.
//!
//! The command set is a pure list and the filter reuses the global-search
//! matcher ([`crate::view::search::match_score`]) so palette and metric search
//! rank identically. Both the command list and the filter are unit-tested
//! independently of the UI.

use std::sync::LazyLock;

use iced::widget::{Column, Id, button, column, container, scrollable, text, text_input};
use iced::{Element, Length};

use crate::message::Message;
use crate::view::tokens::{font, space};

/// Text input id for the command palette (focused on open).
pub static COMMAND_PALETTE_ID: LazyLock<Id> = LazyLock::new(|| Id::new("command-palette-input"));

/// State for the command palette overlay (#28).
#[derive(Debug, Default)]
pub struct CommandPaletteState {
    /// Whether the palette is open.
    pub open: bool,
    /// Current query text.
    pub query: String,
}

impl CommandPaletteState {
    /// Open the palette with an empty query.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
    }

    /// Close the palette and clear the query.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
    }
}

/// One palette entry: a label and the message it dispatches when chosen.
pub struct Command {
    /// Display label (also the fuzzy-match target).
    pub label: &'static str,
    /// Message dispatched when the command is run.
    pub message: Message,
}

/// The full, ordered command set: view navigation followed by actions. Kept as
/// a plain function so it can be filtered/tested without any UI.
pub fn commands() -> Vec<Command> {
    vec![
        Command {
            label: "Go to Dashboard",
            message: Message::OpenDashboard,
        },
        Command {
            label: "Go to Alerts",
            message: Message::OpenAlerts,
        },
        Command {
            label: "Go to Topology",
            message: Message::OpenTopology,
        },
        Command {
            label: "Go to Security",
            message: Message::OpenSecurity,
        },
        Command {
            label: "Go to Expectations",
            message: Message::OpenExpectations,
        },
        Command {
            label: "Go to Sensors",
            message: Message::OpenSensors,
        },
        Command {
            label: "Go to Logs",
            message: Message::OpenLogs,
        },
        Command {
            label: "Go to Incidents",
            message: Message::OpenIncidents,
        },
        Command {
            label: "Go to Inventory",
            message: Message::OpenInventory,
        },
        Command {
            label: "Open Settings",
            message: Message::OpenSettings,
        },
        Command {
            label: "Search metrics across devices",
            message: Message::OpenGlobalSearch,
        },
        Command {
            label: "Toggle theme (dark / light)",
            message: Message::ToggleTheme,
        },
        Command {
            label: "Toggle desktop notifications",
            message: Message::ToggleDesktopNotifications,
        },
        Command {
            label: "Keyboard shortcuts help",
            message: Message::ToggleHelp,
        },
        Command {
            label: "Clear triggered alerts",
            message: Message::ClearAlerts,
        },
        Command {
            label: "Export device data (CSV)",
            message: Message::ExportToCsv,
        },
        Command {
            label: "Export device data (JSON)",
            message: Message::ExportToJson,
        },
    ]
}

/// Filter the command set by `query`. An empty/whitespace query returns every
/// command in declared order; otherwise commands are fuzzy-matched (same scorer
/// as global metric search) and ranked by score, then label for a stable order.
/// Pure — the unit of testing for the palette.
pub fn filter(query: &str) -> Vec<Command> {
    let q = query.trim().to_lowercase();
    let cmds = commands();
    if q.is_empty() {
        return cmds;
    }
    let mut scored: Vec<(i32, Command)> = cmds
        .into_iter()
        .filter_map(|c| {
            crate::view::search::match_score(&c.label.to_lowercase(), &q).map(|s| (s, c))
        })
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| sb.cmp(sa).then_with(|| a.label.cmp(b.label)));
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Render the command palette: an input plus the filtered command list. Each
/// row dispatches [`Message::RunPaletteCommand`] with its index into the
/// `filtered` slice (the app re-derives the same filtered list to resolve it).
pub fn command_palette_panel<'a>(
    state: &'a CommandPaletteState,
    filtered: &[Command],
) -> Element<'a, Message> {
    let input = text_input("Type a command…", &state.query)
        .id(COMMAND_PALETTE_ID.clone())
        .on_input(Message::SetCommandPaletteQuery)
        .padding(space::SM)
        .size(font::BODY);

    let header = text("Command Palette").size(font::SECTION);

    let count = text(format!("{} command(s)", filtered.len())).size(font::CAPTION);

    let mut list = Column::new().spacing(2);
    for (i, cmd) in filtered.iter().enumerate() {
        list = list.push(
            button(text(cmd.label).size(font::CAPTION))
                .on_press(Message::RunPaletteCommand(i))
                .width(Length::Fill)
                .padding([space::XS, space::SM])
                .style(iced::widget::button::text),
        );
    }

    container(
        column![
            header,
            input,
            count,
            scrollable(list).height(Length::Fixed(360.0))
        ]
        .spacing(space::SM)
        .padding(space::MD),
    )
    .width(Length::Fixed(460.0))
    .style(iced::widget::container::rounded_box)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_all_in_order() {
        let all = filter("");
        assert_eq!(all.len(), commands().len());
        assert_eq!(all[0].label, "Go to Dashboard");
    }

    #[test]
    fn fuzzy_filters_and_ranks() {
        // "alerts" should surface the Alerts navigation command first.
        let hits = filter("alerts");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].label, "Go to Alerts");
    }

    #[test]
    fn abbreviation_matches_subsequence() {
        // "thm" is a subsequence of "Toggle theme…" but not a substring.
        let hits = filter("thm");
        assert!(hits.iter().any(|c| c.label.starts_with("Toggle theme")));
    }

    #[test]
    fn non_matching_query_is_empty() {
        assert!(filter("zzzzzz").is_empty());
    }
}
