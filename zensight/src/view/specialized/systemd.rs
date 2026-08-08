//! systemd host specialized view (#281) — a tabbed surface (Overview · Units ·
//! Timers · Sentinel · Events · cgroups) over the sensor's streamed aggregates
//! and `@rpc/systemd/*` procedures. Reuses the tabbed foundation built for
//! netlink/netring (epic #257/#270).

use iced::widget::{Column, button, column, row, scrollable, text};
use iced::{Element, Length, Theme};
use zensight_common::TelemetryValue;
use zensight_common::query_detail::{CgroupNode, UnitRecord};

use zensight_common::action::Verb;

use crate::message::Message;
use crate::view::components::{
    Column as DataColumn, DataTable, SortKey, TabItem, badge, card, empty_state, section_header,
    tabbed_view,
};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::SpecializedTab;
use crate::view::specialized::fetch::Fetch;
use crate::view::specialized::systemd_detail::{
    ActionGate, SystemdDetailState, SystemdDetailTopic, SystemdEventRecord, UNIT_TYPES,
};
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the systemd host specialized view: header + tabbed content.
pub fn systemd_host_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let tabs = systemd_tabs(state);
    let active = if tabs
        .iter()
        .any(|t| t.visible && t.id == state.specialized_tab)
    {
        state.specialized_tab
    } else {
        SpecializedTab::Overview
    };
    let device_id = state.device_id.clone();
    let content = systemd_tab_content(state, active);
    column![
        render_header(state),
        tabbed_view(&tabs, active, content, move |t| {
            Message::SelectSpecializedTab(device_id.clone(), t)
        }),
    ]
    .spacing(space::SM)
    .padding(space::LG)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn systemd_tabs(state: &DeviceDetailState) -> Vec<TabItem<SpecializedTab>> {
    use SpecializedTab::*;
    let failed = mval(state, "units/failed").unwrap_or(0.0) as usize;
    vec![
        TabItem::new(Overview, "Overview"),
        TabItem::new(Units, "Units"),
        TabItem::new(Timers, "Timers"),
        TabItem::new(Sentinel, "Sentinel"),
        TabItem::new(Events, "Events"),
        TabItem::new(Cgroups, "cgroups"),
        // Only on a host that actually offers service control — an audit
        // timeline that can never have entries is just a dead tab.
        TabItem::new(Actions, "Actions").visible(
            state
                .systemd_detail
                .capability
                .ready()
                .is_some_and(|c| c.enabled),
        ),
    ]
    .into_iter()
    .map(|t| {
        if t.id == Units && failed > 0 {
            t.badge(failed)
        } else {
            t
        }
    })
    .collect()
}

fn systemd_tab_content(state: &DeviceDetailState, tab: SpecializedTab) -> Element<'_, Message> {
    use SpecializedTab::*;
    let inner: Column<'_, Message> = match tab {
        Units => render_units_tab(state),
        Timers => render_timers_tab(state),
        Sentinel => render_sentinel_tab(state),
        Events => render_events_tab(state),
        Cgroups => render_cgroups_tab(state),
        Actions => render_actions_tab(state),
        // Overview is the default for any non-systemd remembered tab.
        _ => render_overview(state),
    };
    scrollable(inner.width(Length::Fill))
        .height(Length::Fill)
        .into()
}

fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    row![
        text(format!("systemd: {}", state.device_id.source)).size(font::TITLE),
        text(format!("({} metrics)", state.metrics.len()))
            .size(font::CAPTION)
            .style(dim),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center)
    .into()
}

// ── Overview ──────────────────────────────────────────────────────────────────

fn render_overview(state: &DeviceDetailState) -> Column<'_, Message> {
    let total = mval(state, "units/total").unwrap_or(0.0);
    let active = mval(state, "units/active").unwrap_or(0.0);
    let failed = mval(state, "units/failed").unwrap_or(0.0);
    let n_failed = mval(state, "manager/n_failed_units").unwrap_or(0.0);
    let n_jobs = mval(state, "manager/n_jobs").unwrap_or(0.0);

    let sys_color = if failed > 0.0 || n_failed > 0.0 {
        move |t: &Theme| text::Style {
            color: Some(theme::colors(t).status_degraded()),
        }
    } else {
        move |t: &Theme| text::Style {
            color: Some(theme::colors(t).status_healthy()),
        }
    };
    let sys_label = if failed > 0.0 || n_failed > 0.0 {
        format!("degraded — {} failed unit(s)", failed.max(n_failed) as u64)
    } else {
        "running".to_string()
    };

    let summary = card(
        column![
            section_header("System state", None),
            text(sys_label).size(font::EMPHASIS).style(sys_color),
            row![
                stat("Total units", total),
                stat("Active", active),
                stat("Failed", failed),
                stat("Jobs", n_jobs),
            ]
            .spacing(space::LG),
        ]
        .spacing(space::SM),
    );

    let mut col = column![summary].spacing(space::MD);

    // Boot-performance phases (from boot/*_usec), rendered as a ranked bar in ms.
    let phases = boot_phases_ms(state);
    if !phases.is_empty() {
        col = col.push(card(
            column![
                section_header("Boot performance", None),
                crate::view::chart::ranked_bar(&phases, |v| format!("{v:.0} ms"), 8),
            ]
            .spacing(space::XS),
        ));
    }

    // Opt-in journal health, when present.
    if let Some(usage) = mval(state, "journal/disk_usage_bytes") {
        let avail = mval(state, "journal/disk_available_bytes");
        let line = match avail {
            Some(a) => format!(
                "journal: {} on disk, {} free",
                human_bytes(usage),
                human_bytes(a)
            ),
            None => format!("journal: {} on disk", human_bytes(usage)),
        };
        col = col.push(card(
            column![section_header("Journal", None), text(line).size(font::BODY)]
                .spacing(space::XS),
        ));
    }

    col
}

fn stat<'a>(label: &'a str, value: f64) -> Element<'a, Message> {
    column![
        text(format!("{}", value as u64)).size(font::SECTION),
        text(label).size(font::CAPTION).style(dim),
    ]
    .spacing(space::XS)
    .into()
}

// ── Units ─────────────────────────────────────────────────────────────────────

fn render_units_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.systemd_detail;
    let mut actions = row![refresh_button(SystemdDetailTopic::Units)].spacing(space::XS);
    // daemon-reload is manager-wide, so it belongs to the table, not a row.
    if d.permits_daemon_reload() {
        actions = actions.push(daemon_reload_control(d.pending_action.as_ref()));
    }
    let header = row![section_header("Units", None), actions]
        .spacing(space::SM)
        .align_y(iced::Alignment::Center);

    let body = fetch_body(&d.units, SystemdDetailTopic::Units, |units| {
        if units.is_empty() {
            return empty_state("This host reported no units.", None);
        }
        let table = DataTable::new(unit_columns(state))
            // Chips narrow what the table is about; the filter box searches
            // within that.
            .retain(move |u: &UnitRecord| d.chips_admit(u))
            .searchable(|u: &UnitRecord| format!("{} {}", u.name, u.description))
            .on_sort(Message::SystemdUnitsTableSort)
            .on_filter(Message::SystemdUnitsTableFilter)
            .on_more(Message::SystemdUnitsTableMore)
            .noun("units")
            .view(units, &d.units_table);
        column![table].into()
    });

    let mut panel = column![header, state_chips(d), type_chips(d)].spacing(space::SM);
    // One explanation per table rather than one per row.
    if let Some(note) = gate_note(d) {
        panel = panel.push(note);
    }
    panel = panel.push(body);

    let mut col = column![].spacing(space::MD);
    // Identity drill-down panel (#313): the selected unit's join keys
    // (control group, MainPID, invocation id) rendered as pivot chips.
    if let Some(unit) = &d.selected_unit {
        col = col.push(card(render_unit_detail_panel(state, unit)));
    }
    col.push(card(panel))
}

/// The Units table's columns. The `Actions` column is omitted entirely on a host
/// that answered "service control is off" — a column of permanently dead buttons
/// is worse than no column.
fn unit_columns(state: &DeviceDetailState) -> Vec<DataColumn<'_, UnitRecord, Message>> {
    let d = &state.systemd_detail;
    let mut cols = vec![
        // The unit name is the identity drill-down chip (#313): clicking fetches
        // `@rpc/systemd/unit?name=` and opens the panel above the table.
        DataColumn::fill("Unit", 3, |u: &UnitRecord| {
            button(text(u.name.clone()).size(font::CAPTION))
                .padding([2, 6])
                .style(iced::widget::button::text)
                .on_press(Message::SystemdSelectUnit(Some(u.name.clone())))
                .into()
        })
        .sortable(|u: &UnitRecord| SortKey::Text(u.name.clone())),
        DataColumn::fill("Active", 1, |u: &UnitRecord| state_text(&u.active_state))
            .sortable(|u: &UnitRecord| SortKey::Text(u.active_state.clone())),
        DataColumn::fill("Sub", 1, |u: &UnitRecord| plain(&u.sub_state))
            .sortable(|u: &UnitRecord| SortKey::Text(u.sub_state.clone())),
        DataColumn::fill("Load", 1, |u: &UnitRecord| plain(&u.load_state))
            .sortable(|u: &UnitRecord| SortKey::Text(u.load_state.clone())),
        DataColumn::fill("Enabled", 1, |u: &UnitRecord| {
            plain(u.unit_file_state.as_deref().unwrap_or("—"))
        })
        .sortable(|u: &UnitRecord| SortKey::Text(u.unit_file_state.clone().unwrap_or_default())),
        DataColumn::fill("Description", 3, |u: &UnitRecord| plain(&u.description))
            .sortable(|u: &UnitRecord| SortKey::Text(u.description.clone())),
    ];
    if !matches!(d.capability.ready(), Some(c) if !c.enabled) {
        cols.push(DataColumn::fill("Actions", 3, move |u: &UnitRecord| {
            action_cell(d, u)
        }));
    }
    cols
}

/// Active-state chips, derived from the states actually present rather than a
/// fixed four — `activating`/`reloading` are already colour-coded in the table,
/// so they should be selectable too.
fn state_chips(d: &SystemdDetailState) -> Element<'_, Message> {
    let mut present: Vec<&str> = d
        .units
        .ready()
        .map(|units| {
            units
                .iter()
                .filter(|u| {
                    d.unit_type_filter
                        .as_deref()
                        .is_none_or(|s| u.name.ends_with(s))
                })
                .map(|u| u.active_state.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        })
        .unwrap_or_default();
    // Keep a selected state visible even once nothing is in it, or the chip that
    // produced an empty table would vanish and strand the operator.
    if let Some(f) = d.unit_state_filter.as_deref()
        && !present.contains(&f)
    {
        present.push(f);
    }
    let mut r = row![filter_chip("all", d.unit_state_filter.is_none(), None)].spacing(space::XS);
    for s in present {
        r = r.push(filter_chip_owned(
            s.to_string(),
            d.unit_state_filter.as_deref() == Some(s),
            Some(s.to_string()),
        ));
    }
    r.into()
}

fn type_chips(d: &SystemdDetailState) -> Element<'_, Message> {
    let mut r = row![type_chip("all types", d.unit_type_filter.is_none(), None)].spacing(space::XS);
    for t in UNIT_TYPES {
        r = r.push(type_chip(
            t,
            d.unit_type_filter.as_deref() == Some(t),
            Some(t.to_string()),
        ));
    }
    r.into()
}

/// The one-line explanation of this host's gate, when there is something to say.
fn gate_note(d: &SystemdDetailState) -> Option<Element<'_, Message>> {
    let note = match d.capability {
        Fetch::Ready(ref c) if !c.enabled => {
            "Service control is disabled on this host — the sensor is read-only.".to_string()
        }
        Fetch::Ready(ref c) if c.allow_units.is_empty() => {
            "Service control is enabled but its allowlist is empty, so every unit is refused."
                .to_string()
        }
        Fetch::Ready(ref c) => format!("Service control allows: {}", c.allow_units.join(", ")),
        Fetch::Error(_) => {
            "This host did not answer the service-control probe — actions may be unavailable."
                .to_string()
        }
        _ => return None,
    };
    Some(text(note).size(font::CAPTION).style(dim).into())
}

/// The unit identity drill-down (#313): description/state plus the cross-view
/// join keys — control group (the `process.cgroup` join), MainPID (a pivot chip
/// into the process explorer, carrying the `(pid, start_time)` identity pair),
/// and invocation id (a pivot chip into the Logs view for exactly this run).
/// Unresolvable pivots render as plain text, never a dead button.
fn render_unit_detail_panel<'a>(
    state: &'a DeviceDetailState,
    unit: &'a str,
) -> Element<'a, Message> {
    let d = &state.systemd_detail;
    let host = &state.device_id.source;
    let close = button(text("Close").size(font::CAPTION))
        .padding([3, 9])
        .style(iced::widget::button::secondary)
        .on_press(Message::SystemdSelectUnit(None));
    let header = row![
        section_header("Unit identity", Some(close.into())),
        text(unit.to_string()).size(font::EMPHASIS),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let line = |label: &str, value: String| -> Element<'a, Message> {
        row![
            text(label.to_string())
                .size(font::CAPTION)
                .width(Length::Fixed(150.0))
                .style(dim),
            text(value).size(font::CAPTION),
        ]
        .spacing(space::SM)
        .into()
    };

    let body: Element<'a, Message> = match &d.unit_detail {
        Fetch::Idle | Fetch::Loading => text("Fetching unit detail…")
            .size(font::CAPTION)
            .style(dim)
            .into(),
        Fetch::Error(e) => text(format!("unit detail unavailable: {e}"))
            .size(font::CAPTION)
            .style(dim)
            .into(),
        Fetch::Ready(detail) => {
            let mut colm = column![
                line("description", detail.description.clone()),
                line(
                    "state",
                    format!("{} ({})", detail.active_state, detail.sub_state),
                ),
            ]
            .spacing(3);
            if let Some(p) = &detail.fragment_path {
                colm = colm.push(line("fragment", p.clone()));
            }
            colm = colm.push(line("restarts", detail.n_restarts.to_string()));
            colm = colm.push(line(
                "control group",
                detail
                    .control_group
                    .clone()
                    .unwrap_or_else(|| "—".to_string()),
            ));

            // MainPID → process explorer pivot, or plain text when not running.
            let pid_row: Element<'a, Message> = match detail.main_pid {
                Some(pid) => row![
                    text("MainPID")
                        .size(font::CAPTION)
                        .width(Length::Fixed(150.0))
                        .style(dim),
                    button(text(format!("{pid} → process explorer")).size(font::CAPTION))
                        .padding([2, 8])
                        .style(iced::widget::button::secondary)
                        .on_press(Message::PivotToProcess {
                            host: host.clone(),
                            pid: pid as i32,
                            start_time: detail.main_pid_start_time,
                        }),
                ]
                .spacing(space::SM)
                .align_y(iced::Alignment::Center)
                .into(),
                None => line("MainPID", "— (not running)".to_string()),
            };
            colm = colm.push(pid_row);

            // Invocation id → Logs-for-this-run pivot, or plain text.
            let inv_row: Element<'a, Message> = match &detail.invocation_id {
                Some(inv) => {
                    let short: String = inv.chars().take(12).collect();
                    row![
                        text("invocation")
                            .size(font::CAPTION)
                            .width(Length::Fixed(150.0))
                            .style(dim),
                        text(format!("{short}…")).size(font::CAPTION),
                        button(text("Logs for this run").size(font::CAPTION))
                            .padding([2, 8])
                            .style(iced::widget::button::secondary)
                            .on_press(Message::OpenLogsForInvocation {
                                unit: unit.to_string(),
                                invocation_id: inv.clone(),
                            }),
                    ]
                    .spacing(space::SM)
                    .align_y(iced::Alignment::Center)
                    .into()
                }
                None => line("invocation", "no active run".to_string()),
            };
            colm = colm.push(inv_row);
            colm.into()
        }
    };

    column![header, body, unit_file_section(d, unit)]
        .spacing(space::SM)
        .into()
}

/// The unit's on-disk definition, behind a toggle.
///
/// Opt-in per host (`actions.expose_unit_files`), so a sensor that does not
/// serve it answers nothing and the panel says so rather than spinning.
fn unit_file_section<'a>(d: &'a SystemdDetailState, unit: &'a str) -> Element<'a, Message> {
    match &d.unit_file {
        Fetch::Idle => button(text("View unit file").size(font::CAPTION))
            .padding([2, 8])
            .style(iced::widget::button::secondary)
            .on_press(Message::SystemdFetchUnitFile(unit.to_string()))
            .into(),
        Fetch::Loading => text("Reading unit file…")
            .size(font::CAPTION)
            .style(dim)
            .into(),
        Fetch::Error(e) => text(format!("Unit file unavailable: {e}"))
            .size(font::CAPTION)
            .style(dim)
            .into(),
        Fetch::Ready(file) => {
            let mut colm = column![
                row![
                    section_header("Unit file", None),
                    button(text("Hide").size(font::CAPTION))
                        .padding([2, 8])
                        .style(iced::widget::button::text)
                        .on_press(Message::SystemdHideUnitFile),
                ]
                .spacing(space::SM)
                .align_y(iced::Alignment::Center)
            ]
            .spacing(space::XS);
            // Say plainly that this is not the file as it exists on disk.
            if file.redacted {
                colm = colm.push(
                    text("Secret-looking assignments have been redacted by the sensor.")
                        .size(font::CAPTION)
                        .style(dim),
                );
            }
            if file.truncated {
                colm = colm.push(
                    text("Content was truncated at the sensor's size cap.")
                        .size(font::CAPTION)
                        .style(dim),
                );
            }
            if let Some(body) = &file.fragment {
                let path = file.fragment_path.as_deref().unwrap_or("(fragment)");
                colm = colm.push(text(path.to_string()).size(font::CAPTION).style(dim));
                colm = colm.push(text(body.clone()).size(font::CAPTION));
            }
            for (path, body) in &file.dropins {
                colm = colm.push(
                    text(format!("drop-in: {path}"))
                        .size(font::CAPTION)
                        .style(dim),
                );
                colm = colm.push(text(body.clone()).size(font::CAPTION));
            }
            if file.fragment.is_none() && file.dropins.is_empty() {
                colm = colm.push(
                    text("This unit has no unit file (generated or transient).")
                        .size(font::CAPTION)
                        .style(dim),
                );
            }
            scrollable(colm).height(Length::Fixed(320.0)).into()
        }
    }
}

// ── Timers ────────────────────────────────────────────────────────────────────

fn render_timers_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.systemd_detail;
    let header = row![
        section_header("Timers", None),
        refresh_button(SystemdDetailTopic::Timers),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let body = fetch_body(&d.timers, SystemdDetailTopic::Timers, |timers| {
        if timers.is_empty() {
            return empty_state("No timer units.", None);
        }
        let mut list = column![table_header(&["Timer", "State", "Last", "Next", ""])].spacing(2);
        for t in timers.iter().take(300) {
            let overdue: Element<'_, Message> = if t.overdue {
                container_cell(badge(
                    // reuse a warning tone for overdue
                    warn_color(),
                    "overdue",
                ))
            } else {
                cell("", 1)
            };
            list = list.push(
                row![
                    cell(&t.name, 3),
                    cell(&t.active_state, 1),
                    cell(&fmt_usec(t.last_trigger_usec), 2),
                    cell(&fmt_usec(t.next_elapse_usec), 2),
                    overdue,
                ]
                .spacing(space::SM),
            );
        }
        column![list, count_note(timers.len(), "timers")]
            .spacing(space::SM)
            .into()
    });

    column![card(column![header, body].spacing(space::SM))].spacing(space::MD)
}

// ── Sentinel / Expectations ───────────────────────────────────────────────────

fn render_sentinel_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let _ = state;
    let author = button(text("Author expectations").size(font::CAPTION))
        .on_press(Message::OpenExpectations)
        .style(iced::widget::button::primary);
    column![card(
        column![
            section_header("Sentinel", Some(author.into())),
            text(
                "The systemd sentinel raises alerts when declared expectations are \
                 violated — a service/target that must stay active, a timer that must \
                 fire within a window, a restart-rate ceiling, or any failed unit. \
                 Author expectations here; they hot-swap on the sensor via the \
                 expectations/set procedure."
            )
            .size(font::BODY)
            .style(dim),
            text("Firing sentinel alerts appear in the Alerts and Security views.")
                .size(font::CAPTION)
                .style(dim),
        ]
        .spacing(space::SM),
    )]
    .spacing(space::MD)
}

// ── Events ────────────────────────────────────────────────────────────────────

fn render_events_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.systemd_detail;
    let header = row![
        section_header("Control-plane timeline", None),
        refresh_button(SystemdDetailTopic::Events),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let body = fetch_body(&d.events, SystemdDetailTopic::Events, |events| {
        if events.is_empty() {
            return empty_state("No recent unit/job events.", None);
        }
        let mut list = column![].spacing(2);
        for e in events.iter().take(300) {
            list = list.push(event_row(e));
        }
        list.into()
    });

    column![card(column![header, body].spacing(space::SM))].spacing(space::MD)
}

fn event_row(e: &SystemdEventRecord) -> Element<'_, Message> {
    let unit = e.unit.as_deref().unwrap_or("");
    let detail = match (&e.from, &e.to, &e.job_result) {
        (Some(f), Some(t), _) => format!("{f} → {t}"),
        (_, _, Some(r)) => r.clone(),
        _ => String::new(),
    };
    row![
        cell(&fmt_unix(e.ts_unix), 2),
        cell(&e.kind, 2),
        cell(unit, 3),
        cell(&detail, 2),
    ]
    .spacing(space::SM)
    .into()
}

// ── Service-control audit timeline (#283) ─────────────────────────────────────

fn render_actions_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.systemd_detail;
    let header = row![
        section_header("Service-control audit", None),
        refresh_button(SystemdDetailTopic::Actions),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let body = fetch_body(&d.actions, SystemdDetailTopic::Actions, |actions| {
        if actions.is_empty() {
            return empty_state("No service actions have been attempted on this host.", None);
        }
        let mut list = column![table_header(&["When", "Verb", "Unit", "Outcome"])].spacing(2);
        for a in actions.iter().take(200) {
            list = list.push(action_row(a));
        }
        list.into()
    });

    let note = text(
        "Every attempt is recorded, refused ones included; the sensor also writes each to its audit log.",
    )
    .size(font::CAPTION)
    .style(dim);

    column![card(column![header, note, body].spacing(space::SM))].spacing(space::MD)
}

fn action_row(a: &zensight_common::action::ActionStatus) -> Element<'_, Message> {
    // Refusals and failures must be legible as such at a glance, not inferred
    // from an absent word.
    let (outcome, tone) = match (a.accepted, a.result.as_deref()) {
        (false, _) => (
            format!("refused: {}", a.error.as_deref().unwrap_or("no reason")),
            Tone::Bad,
        ),
        (true, Some("done")) | (true, Some("applied")) => {
            let hint = if a.needs_daemon_reload {
                " (needs daemon-reload)"
            } else {
                ""
            };
            (format!("done{hint}"), Tone::Good)
        }
        (true, Some(other)) => (other.to_string(), Tone::Bad),
        (true, None) => ("issued, outcome unknown".to_string(), Tone::Warn),
    };
    row![
        cell(&fmt_unix(a.ts_unix.max(0) as u64), 2),
        cell(a.verb.as_str(), 1),
        cell(if a.unit.is_empty() { "—" } else { &a.unit }, 3),
        toned(outcome, tone, 3),
    ]
    .spacing(space::SM)
    .into()
}

#[derive(Clone, Copy)]
enum Tone {
    Good,
    Warn,
    Bad,
}

fn toned<'a>(value: String, tone: Tone, portion: u16) -> Element<'a, Message> {
    let styled = move |t: &Theme| {
        let c = theme::colors(t);
        text::Style {
            color: Some(match tone {
                Tone::Good => c.status_healthy(),
                Tone::Warn => c.status_warning(),
                Tone::Bad => c.status_error(),
            }),
        }
    };
    text(value)
        .size(font::CAPTION)
        .width(Length::FillPortion(portion))
        .style(styled)
        .into()
}

// ── cgroups tree ──────────────────────────────────────────────────────────────

fn render_cgroups_tab(state: &DeviceDetailState) -> Column<'_, Message> {
    let d = &state.systemd_detail;
    let header = row![
        section_header("cgroup tree", None),
        refresh_button(SystemdDetailTopic::Cgroups),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let body = fetch_body(&d.cgroups, SystemdDetailTopic::Cgroups, |tree| match tree {
        Some(root) => {
            let mut rows: Vec<(usize, &CgroupNode)> = Vec::new();
            flatten_cgroup(root, 0, &mut rows);
            let mut list = column![table_header(&["Node", "Mem", "CPU", "Tasks"])].spacing(2);
            for (depth, node) in rows.iter().take(400) {
                let indent = "    ".repeat(*depth);
                let name = format!("{indent}{}", node.name);
                list = list.push(
                    row![
                        cell(&name, 4),
                        cell(&opt_bytes(node.mem_bytes), 1),
                        cell(&opt_usec(node.cpu_usec), 1),
                        cell(&opt_num(node.tasks), 1),
                    ]
                    .spacing(space::SM),
                );
            }
            column![list, count_note(rows.len(), "nodes")]
                .spacing(space::SM)
                .into()
        }
        None => empty_state("No cgroup subtree returned.", None),
    });

    column![card(column![header, body].spacing(space::SM))].spacing(space::MD)
}

fn flatten_cgroup<'a>(node: &'a CgroupNode, depth: usize, out: &mut Vec<(usize, &'a CgroupNode)>) {
    out.push((depth, node));
    for child in &node.children {
        flatten_cgroup(child, depth + 1, out);
    }
}

// ── Shared fetch/table helpers ────────────────────────────────────────────────

/// Render a `Fetch<T>` panel: idle → load button, loading → note, error →
/// message + retry, ready → the caller's content.
fn fetch_body<'a, T>(
    fetch: &'a Fetch<T>,
    topic: SystemdDetailTopic,
    ready: impl FnOnce(&'a T) -> Element<'a, Message>,
) -> Element<'a, Message> {
    match fetch {
        Fetch::Idle => empty_state(
            format!("{} are fetched on demand.", topic.label()),
            Some(load_button(topic, "Load")),
        ),
        Fetch::Loading => text("Loading…").size(font::BODY).style(dim).into(),
        Fetch::Error(e) => empty_state(
            format!("Query failed: {e}"),
            Some(load_button(topic, "Retry")),
        ),
        Fetch::Ready(v) => ready(v),
    }
}

fn refresh_button<'a>(topic: SystemdDetailTopic) -> Element<'a, Message> {
    load_button(topic, "Refresh")
}

fn load_button<'a>(topic: SystemdDetailTopic, label: &'a str) -> Element<'a, Message> {
    button(text(label).size(font::CAPTION))
        .on_press(Message::FetchSystemdDetail(topic))
        .padding([space::XS as u16, space::SM as u16])
        .into()
}

fn filter_chip<'a>(label: &'a str, active: bool, value: Option<&'a str>) -> Element<'a, Message> {
    filter_chip_owned(label.to_string(), active, value.map(str::to_string))
}

fn filter_chip_owned<'a>(
    label: String,
    active: bool,
    value: Option<String>,
) -> Element<'a, Message> {
    chip(label, active, Message::SystemdSetUnitFilter(value))
}

fn type_chip<'a>(label: &str, active: bool, value: Option<String>) -> Element<'a, Message> {
    chip(
        label.to_string(),
        active,
        Message::SystemdSetUnitTypeFilter(value),
    )
}

fn chip<'a>(label: String, active: bool, on_press: Message) -> Element<'a, Message> {
    let b = button(text(label).size(font::CAPTION))
        .on_press(on_press)
        .padding([space::XS as u16, space::SM as u16]);
    if active {
        b.style(iced::widget::button::primary).into()
    } else {
        b.style(iced::widget::button::text).into()
    }
}

/// Hand-rolled header for the Timers and cgroups tabs, which are small,
/// fixed-shape lists rather than searchable tables. The Units tab uses
/// [`DataTable`] instead.
fn table_header<'a>(labels: &[&'a str]) -> Element<'a, Message> {
    // The portion weights mirror the data rows (name columns wider).
    let mut r = row![].spacing(space::SM);
    let weights = header_weights(labels.len());
    for (label, w) in labels.iter().zip(weights) {
        r = r.push(
            text(*label)
                .size(font::CAPTION)
                .width(Length::FillPortion(w))
                .style(dim),
        );
    }
    r.into()
}

fn header_weights(n: usize) -> Vec<u16> {
    // Heuristic: first column widest; keep in sync with the per-row `cell` weights.
    match n {
        6 => vec![3, 1, 1, 1, 3, 2],
        5 => vec![3, 1, 1, 1, 3],
        4 => vec![4, 1, 1, 1],
        _ => vec![2; n],
    }
}

/// A bare table cell. Unlike [`cell`], it sets no width: `DataTable` wraps every
/// cell in a container of the column's width.
fn plain<'a>(value: &str) -> Element<'a, Message> {
    text(value.to_string()).size(font::CAPTION).into()
}

/// [`state_cell`] without the width, for `DataTable` columns.
fn state_text<'a>(state: &str) -> Element<'a, Message> {
    let owned = state.to_string();
    let styled = move |t: &Theme| {
        let c = theme::colors(t);
        let color = match owned.as_str() {
            "active" => c.status_healthy(),
            "failed" => c.status_error(),
            "activating" | "deactivating" | "reloading" => c.status_warning(),
            _ => c.text_muted(),
        };
        text::Style { color: Some(color) }
    };
    text(state.to_string())
        .size(font::CAPTION)
        .style(styled)
        .into()
}

/// A small action button. Without an `on_press` iced renders it disabled, which
/// is how a known-refused verb is shown: present, so the operator can see the
/// control exists, but visibly not available.
fn tiny_button<'a>(label: String, on_press: Option<Message>) -> iced::widget::Button<'a, Message> {
    let b = button(text(label).size(font::CAPTION)).padding([2, 6]);
    match on_press {
        Some(m) => b.on_press(m),
        None => b,
    }
}

/// Per-unit service-control cell (#283), rendered from the host's advertised
/// gate rather than optimistically.
///
/// Previously these buttons were always live, so on a read-only host — the
/// default — every click failed, and the operator learned that from an error
/// toast a second and a half later. Now the row says up front what this host
/// will accept for this unit.
fn action_cell<'a>(d: &'a SystemdDetailState, unit: &UnitRecord) -> Element<'a, Message> {
    // An armed action takes over the cell regardless of gate: it is mid-dialogue.
    if let Some((verb, armed)) = d.pending_action.as_ref()
        && armed == &unit.name
    {
        return row![
            text(format!("{verb}?")).size(font::CAPTION),
            tiny_button("confirm".into(), Some(Message::SystemdUnitActionConfirm)),
            tiny_button("cancel".into(), Some(Message::SystemdUnitActionCancel)),
        ]
        .spacing(space::XS)
        .align_y(iced::Alignment::Center)
        .into();
    }

    match d.action_gate(&unit.name) {
        // The whole column is dropped in this case; this arm only guards the
        // gap between the probe answering and the next render.
        ActionGate::Disabled => text("—").size(font::CAPTION).style(dim).into(),
        ActionGate::Busy(verb) => text(format!("{verb}ing…"))
            .size(font::CAPTION)
            .style(dim)
            .into(),
        // No buttons at all: unlike an allowlist refusal, this is not something
        // a host could ever permit.
        ActionGate::Template => text("template — act on an instance")
            .size(font::CAPTION)
            .style(dim)
            .into(),
        // Show the controls, inert, and say why — hiding them would silently
        // strip working buttons from a pre-1.4 sensor that does have actions on.
        ActionGate::Unknown => {
            disabled_verbs(&[Verb::Start, Verb::Stop, Verb::Restart], "probing…")
        }
        ActionGate::NotAllowed => {
            disabled_verbs(&[Verb::Start, Verb::Stop, Verb::Restart], "not allowlisted")
        }
        ActionGate::Allowed(verbs) => {
            let mut r = row![].spacing(space::XS).align_y(iced::Alignment::Center);
            for verb in verbs {
                r = r.push(tiny_button(
                    verb.to_string(),
                    Some(Message::SystemdUnitActionArm {
                        verb,
                        unit: unit.name.clone(),
                    }),
                ));
            }
            r.into()
        }
    }
}

/// Inert verb buttons plus the reason they are inert.
fn disabled_verbs<'a>(verbs: &[Verb], why: &'a str) -> Element<'a, Message> {
    let mut r = row![].spacing(space::XS).align_y(iced::Alignment::Center);
    for verb in verbs {
        r = r.push(tiny_button(verb.to_string(), None));
    }
    r.push(text(why).size(font::CAPTION).style(dim)).into()
}

/// The manager-wide daemon-reload control, armed and confirmed like a row action
/// but carrying no unit.
fn daemon_reload_control<'a>(pending: Option<&(Verb, String)>) -> Element<'a, Message> {
    match pending {
        Some((Verb::DaemonReload, _)) => row![
            text("daemon-reload?").size(font::CAPTION),
            tiny_button("confirm".into(), Some(Message::SystemdUnitActionConfirm)),
            tiny_button("cancel".into(), Some(Message::SystemdUnitActionCancel)),
        ]
        .spacing(space::XS)
        .align_y(iced::Alignment::Center)
        .into(),
        _ => tiny_button(
            "daemon-reload".into(),
            Some(Message::SystemdUnitActionArm {
                verb: Verb::DaemonReload,
                unit: String::new(),
            }),
        )
        .into(),
    }
}

fn cell<'a>(value: &str, portion: u16) -> Element<'a, Message> {
    text(value.to_string())
        .size(font::CAPTION)
        .width(Length::FillPortion(portion))
        .into()
}

fn container_cell<'a>(inner: Element<'a, Message>) -> Element<'a, Message> {
    iced::widget::container(inner)
        .width(Length::FillPortion(1))
        .into()
}

/// A unit-state cell tinted by state (green active / red failed / muted else).
fn count_note<'a>(n: usize, noun: &'a str) -> Element<'a, Message> {
    text(format!("{n} {noun}"))
        .size(font::CAPTION)
        .style(dim)
        .into()
}

// ── Metric + formatting helpers ───────────────────────────────────────────────

fn mval(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    match state.metrics.get(metric).map(|p| &p.value) {
        Some(TelemetryValue::Counter(v)) => Some(*v as f64),
        Some(TelemetryValue::Gauge(v)) => Some(*v),
        Some(TelemetryValue::Boolean(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Boot phases (firmware/loader/kernel/initrd/userspace) in milliseconds, from
/// the `boot/<phase>_usec` gauges. `total` is excluded (it's the sum).
fn boot_phases_ms(state: &DeviceDetailState) -> Vec<(String, f64)> {
    const PHASES: [&str; 5] = ["firmware", "loader", "kernel", "initrd", "userspace"];
    PHASES
        .iter()
        .filter_map(|p| {
            let usec = mval(state, &format!("boot/{p}_usec"))?;
            (usec > 0.0).then(|| (p.to_string(), usec / 1000.0))
        })
        .collect()
}

fn fmt_usec(usec: u64) -> String {
    if usec == 0 || usec == u64::MAX {
        return "—".to_string();
    }
    fmt_unix(usec / 1_000_000)
}

fn fmt_unix(secs: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(secs as i64, 0) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => secs.to_string(),
    }
}

fn opt_bytes(v: Option<u64>) -> String {
    v.map(human_bytes_u).unwrap_or_else(|| "—".to_string())
}
fn opt_usec(v: Option<u64>) -> String {
    v.map(|u| format!("{:.1}s", u as f64 / 1_000_000.0))
        .unwrap_or_else(|| "—".to_string())
}
fn opt_num(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string())
}

fn human_bytes(v: f64) -> String {
    human_bytes_u(v as u64)
}
fn human_bytes_u(v: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut val = v as f64;
    let mut i = 0;
    while val >= 1024.0 && i < UNITS.len() - 1 {
        val /= 1024.0;
        i += 1;
    }
    format!("{val:.1} {}", UNITS[i])
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

fn warn_color() -> iced::Color {
    // A theme-independent warning tone for the overdue badge (badge takes a Color).
    theme::SEVERITY_WARNING
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::{Protocol, TelemetryPoint};

    fn state_with(metrics: &[(&str, f64)]) -> DeviceDetailState {
        let mut s = DeviceDetailState::new(DeviceId::fixture(Protocol::Systemd, "server01"));
        for (m, v) in metrics {
            s.metrics.insert(
                (*m).to_string(),
                TelemetryPoint::new("server01", Protocol::Systemd, *m, TelemetryValue::Gauge(*v)),
            );
        }
        s
    }

    #[test]
    fn boot_phases_exclude_total_and_zeros() {
        let s = state_with(&[
            ("boot/firmware_usec", 5_000_000.0),
            ("boot/kernel_usec", 800_000.0),
            ("boot/initrd_usec", 0.0),
            ("boot/total_usec", 32_000_000.0),
        ]);
        let phases = boot_phases_ms(&s);
        let names: Vec<_> = phases.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"firmware"));
        assert!(names.contains(&"kernel"));
        assert!(!names.contains(&"initrd")); // zero excluded
        assert!(!names.contains(&"total")); // total excluded
        // firmware 5_000_000 usec → 5000 ms
        assert_eq!(
            phases.iter().find(|(n, _)| n == "firmware").unwrap().1,
            5000.0
        );
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes_u(512), "512.0 B");
        assert_eq!(human_bytes_u(4096), "4.0 KiB");
        assert_eq!(human_bytes_u(5_000_000_000), "4.7 GiB");
    }

    #[test]
    fn fmt_usec_handles_sentinels() {
        assert_eq!(fmt_usec(0), "—");
        assert_eq!(fmt_usec(u64::MAX), "—");
    }

    // ── Identity pivots (#313) ────────────────────────────────────────────────

    use iced_test::simulator;
    use zensight_common::query_detail::UnitDetail;

    fn unit_detail(main_pid: Option<u32>, invocation: Option<&str>) -> UnitDetail {
        UnitDetail {
            name: "redis.service".into(),
            description: "Redis".into(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            fragment_path: None,
            active_enter_usec: 0,
            n_restarts: 1,
            mem_bytes: None,
            cpu_usec: None,
            tasks: None,
            exec_main_status: 0,
            requires: vec![],
            wants: vec![],
            after: vec![],
            before: vec![],
            recent_changes: vec![],
            main_pid,
            main_pid_start_time: main_pid.map(|_| 12345),
            invocation_id: invocation.map(String::from),
            control_group: Some("/system.slice/redis.service".into()),
        }
    }

    #[test]
    fn unit_detail_panel_pivots_carry_identity_keys() {
        let mut s = state_with(&[]);
        s.systemd_detail.selected_unit = Some("redis.service".into());
        s.systemd_detail.unit_detail =
            Fetch::Ready(unit_detail(Some(42), Some("deadbeefcafe12345678")));

        let mut ui = simulator(render_unit_detail_panel(&s, "redis.service"));
        let _ = ui.click("42 → process explorer");
        let _ = ui.click("Logs for this run");
        let msgs: Vec<Message> = ui.into_messages().collect();
        // MainPID chip carries the (pid, start_time) identity pair + the host.
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::PivotToProcess { host, pid: 42, start_time: Some(12345) }
                if host == "server01"
        )));
        // The logs chip carries the exact unit run.
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::OpenLogsForInvocation { unit, invocation_id }
                if unit == "redis.service" && invocation_id == "deadbeefcafe12345678"
        )));
    }

    #[test]
    fn unit_detail_panel_unresolvable_pivots_render_inert_text() {
        // No MainPID / no invocation id → plain text, never a dead button.
        let mut s = state_with(&[]);
        s.systemd_detail.selected_unit = Some("redis.service".into());
        s.systemd_detail.unit_detail = Fetch::Ready(unit_detail(None, None));

        let mut ui = simulator(render_unit_detail_panel(&s, "redis.service"));
        assert!(ui.find("— (not running)").is_ok());
        assert!(ui.find("no active run").is_ok());
        assert!(ui.find("Logs for this run").is_err());
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.is_empty());
    }
}
