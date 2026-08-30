//! Bus explorer (#748): the live key-tree, bounded, with explicit drop
//! accounting — what `zengui` is upstream, for this deployment's bus.
//!
//! Architecture (see `docs/views.md`, "Bus Explorer"):
//! - a **pump** ([`pump`]) owns the `zenkey_fleet::Monitor` on the GUI's
//!   session and folds per-sample work off the GUI thread ([`core`]);
//! - the GUI receives ~4 [`crate::message::Message::ExplorerTick`]s per
//!   second, whatever the bus rate;
//! - this module holds the per-view state and the pure render, per the
//!   repo's view/state pattern;
//! - the pure fold is deliberately drivable by a `.zrec` replay (#747):
//!   nothing below the pump can tell live traffic from a fixture.
//!
//! Scope caution (from the issue): this is a **new** view. It does not
//! replace `subscription.rs`, the focus-mode machinery, or the redb store —
//! the dashboards' data path is untouched.

pub mod core;
pub mod inspector;
pub mod pump;
pub mod tree;

use std::sync::Arc;

use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Font, Length};

use crate::message::Message;
use crate::view::components::kit::{badge, empty_state, metric_tile, section_header};
use crate::view::theme;
use crate::view::tokens::{font, space};

use core::ExplorerSnapshot;
use tree::{Expansion, TreeRow, is_expanded, tree_rows};

/// Per-view state (the view/state pattern; `docs/views.md` L1).
#[derive(Debug, Default)]
pub struct ExplorerState {
    /// A pump is running (live or demo). The last snapshot outlives a stop
    /// deliberately — a stopped monitor's final tree is still worth reading.
    pub running: bool,
    pub snapshot: Option<Arc<ExplorerSnapshot>>,
    /// Flattened rows, recomputed on tick / toggle — never per redraw.
    pub rows: Vec<TreeRow>,
    pub expansion: Expansion,
    /// The key whose retained sample the inspector shows.
    pub selected: Option<String>,
    pub watch_input: String,
    pub error: Option<String>,
}

impl ExplorerState {
    pub fn apply_tick(&mut self, snapshot: Arc<ExplorerSnapshot>) {
        self.snapshot = Some(snapshot);
        self.recompute();
    }

    pub fn toggle(&mut self, path: String) {
        let depth = path.split('/').count() - 1;
        let now = is_expanded(&self.expansion, &path, depth);
        self.expansion.insert(path, !now);
        self.recompute();
    }

    fn recompute(&mut self) {
        self.rows = match &self.snapshot {
            Some(s) => tree_rows(&s.tree, &self.expansion),
            None => Vec::new(),
        };
    }
}

/// The view. Pure render of [`ExplorerState`].
pub fn explorer_view(state: &ExplorerState) -> Element<'_, Message> {
    let Some(snapshot) = &state.snapshot else {
        let hint = if state.running {
            "monitor starting…"
        } else if let Some(e) = &state.error {
            e.as_str()
        } else {
            "The bus explorer needs a connection. Open it while connected \
             (or in demo mode) to start a monitor."
        };
        return empty_state(hint, None);
    };

    let mut col = column![header(state, snapshot), ledger_strip(snapshot)].spacing(space::SM);

    if let Some(e) = &state.error {
        col = col.push(
            text(format!("error: {e}"))
                .size(font::CAPTION)
                .style(|t: &iced::Theme| iced::widget::text::Style {
                    color: Some(theme::colors(t).status_error()),
                }),
        );
    }

    col = col.push(presence_strip(snapshot));
    if !snapshot.qos.mismatches.is_empty()
        || snapshot.qos.refused > 0
        || !snapshot.qos.unregistered.is_empty()
    {
        col = col.push(qos_panel(snapshot));
    }

    let tree: Element<'_, Message> = if state.rows.is_empty() {
        empty_state(
            "No keys yet. Add a watch (e.g. `v1/**`) to subscribe the \
             monitor to the data planes.",
            None,
        )
    } else {
        scrollable(
            state
                .rows
                .iter()
                .fold(column![].spacing(1), |c, r| c.push(tree_row(r, state))),
        )
        .height(Length::Fill)
        .into()
    };

    let body: Element<'_, Message> = match &snapshot.inspected {
        Some(sample) => row![
            container(tree).width(Length::FillPortion(3)),
            container(inspector::inspector_pane(sample)).width(Length::FillPortion(2)),
        ]
        .spacing(space::SM)
        .into(),
        None => tree,
    };
    col = col.push(body);

    container(col.spacing(space::SM))
        .padding(space::MD)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn header<'a>(state: &'a ExplorerState, snapshot: &'a ExplorerSnapshot) -> Element<'a, Message> {
    let status = if state.running {
        format!("monitoring — {} watch(es)", snapshot.watches.len())
    } else {
        "stopped — last snapshot shown".to_string()
    };

    let mut controls = row![
        text_input("watch selector, e.g. v1/**", &state.watch_input)
            .on_input(Message::ExplorerWatchInput)
            .on_submit(Message::ExplorerWatchSubmit)
            .size(font::BODY)
            .width(280),
        button(text("Watch").size(font::BODY)).on_press(Message::ExplorerWatchSubmit),
    ]
    .spacing(space::SM);
    if state.running {
        controls = controls.push(
            button(text("Stop").size(font::BODY))
                .style(iced::widget::button::danger)
                .on_press(Message::ExplorerStop),
        );
    }
    // Keys are shown as this session sees them: under a namespaced
    // deployment the base is stripped on ingress, so claiming "the wire"
    // would be a lie the caption avoids.
    let mut col = column![
        section_header(format!("Bus — {status}"), Some(controls.into())),
        text("keys as this session sees them (base-relative)").size(font::CAPTION),
    ]
    .spacing(space::XS);

    if !snapshot.watches.is_empty() {
        let chips = snapshot
            .watches
            .iter()
            .fold(row![].spacing(space::SM), |r, (id, sel)| {
                r.push(
                    button(text(format!("{sel} ✕")).size(font::CAPTION))
                        .style(iced::widget::button::secondary)
                        .on_press(Message::ExplorerUnwatch(*id)),
                )
            });
        col = col.push(chips);
    }
    col.into()
}

/// The five loss/bound ledgers, one tile each, zeros rendered honestly. They
/// are DISTINCT facts (RFC 13): key-bound evictions, released-watch
/// retirements, broadcast shed, and the retention ring's byte/age drops —
/// never summed into one reassuring number.
fn ledger_strip(snapshot: &ExplorerSnapshot) -> Element<'_, Message> {
    let t = &snapshot.tree;
    let r = &snapshot.retention;
    row![
        metric_tile("keys", format!("{} of {}", t.keys, core::EXPLORER_MAX_KEYS)),
        metric_tile("keys evicted", t.evicted.to_string()),
        metric_tile("keys unwatched", t.unwatched.to_string()),
        metric_tile("samples shed", snapshot.stream_dropped.to_string()),
        metric_tile(
            "retained",
            format!(
                "{} in {} KiB / {}s",
                r.retained,
                r.retained_bytes / 1024,
                r.budget.max_age.as_secs()
            )
        ),
    ]
    .spacing(space::SM)
    .into()
}

fn presence_strip(snapshot: &ExplorerSnapshot) -> Element<'_, Message> {
    if snapshot.presence.is_empty() {
        return text("presence: no liveliness tokens seen yet")
            .size(font::CAPTION)
            .into();
    }
    snapshot
        .presence
        .iter()
        .fold(
            row![text("presence:").size(font::CAPTION)].spacing(space::SM),
            |r, ((origin, producer), up)| {
                let color = if *up {
                    theme::STATUS_ONLINE
                } else {
                    theme::STATUS_OFFLINE
                };
                r.push(badge(color, format!("{origin}/{producer}")))
            },
        )
        .into()
}

fn qos_panel(snapshot: &ExplorerSnapshot) -> Element<'_, Message> {
    let q = &snapshot.qos;
    let mut col = column![].spacing(space::XS);
    for (key, m) in &q.mismatches {
        col = col.push(row![
            badge(
                theme::STATUS_DEGRADED,
                format!(
                    "QoS mismatch · declared {} observed {} · {}×",
                    m.declared, m.observed, m.count
                ),
            ),
            text(key.clone()).size(font::CAPTION).font(Font::MONOSPACE),
        ]);
    }
    if q.refused > 0 {
        col = col.push(
            text(format!(
                "…and {} more mismatching key(s) past the ledger bound",
                q.refused
            ))
            .size(font::CAPTION),
        );
    }
    if !q.unregistered.is_empty() || q.unregistered_refused > 0 {
        // Unregistered is a distinct fact, not a mismatch: nothing was
        // declared, so nothing can disagree.
        col = col.push(
            text(format!(
                "unregistered keys: {}{}",
                q.unregistered.len(),
                if q.unregistered_refused > 0 {
                    format!(" (+{} past the bound)", q.unregistered_refused)
                } else {
                    String::new()
                }
            ))
            .size(font::CAPTION),
        );
    }
    crate::view::components::kit::card(col)
}

fn tree_row<'a>(r: &'a TreeRow, state: &'a ExplorerState) -> Element<'a, Message> {
    let indent = (r.depth as f32) * space::MD;
    let arrow = if r.has_children {
        if r.expanded { "▾" } else { "▸" }
    } else {
        " "
    };

    let mut line = row![]
        .spacing(space::SM)
        .align_y(iced::Alignment::Center)
        .push(iced::widget::Space::new().width(indent));

    let toggle: Element<'_, Message> = if r.has_children {
        button(text(arrow).size(font::CAPTION))
            .style(iced::widget::button::text)
            .padding(0)
            .on_press(Message::ExplorerToggleNode(r.path.clone()))
            .into()
    } else {
        text(arrow).size(font::CAPTION).into()
    };
    line = line.push(toggle);

    let label = text(r.label.clone())
        .size(font::CAPTION)
        .font(Font::MONOSPACE);
    let label: Element<'_, Message> = if r.is_key {
        let selected = state.selected.as_deref() == Some(r.path.as_str());
        button(label)
            .style(if selected {
                iced::widget::button::primary
            } else {
                iced::widget::button::text
            })
            .padding([0.0, space::XS])
            .on_press(Message::ExplorerSelectKey(if selected {
                None
            } else {
                Some(r.path.clone())
            }))
            .into()
    } else {
        label.into()
    };
    line = line.push(label);

    let stats = if r.is_key {
        format!("{:.1}/s · {} · {} B", r.rate_hz, r.count, r.bytes)
    } else {
        format!("{:.1}/s · {} keys · {} B", r.rate_hz, r.keys, r.bytes)
    };
    line = line.push(text(stats).size(font::CAPTION));

    if state
        .snapshot
        .as_ref()
        .is_some_and(|s| s.qos.mismatches.contains_key(&r.path))
    {
        line = line.push(badge(theme::STATUS_DEGRADED, "qos"));
    }

    line.into()
}
