//! systemd host specialized view (#281) — a tabbed surface (Overview · Units ·
//! Timers · Sentinel · Events · cgroups) over the sensor's streamed aggregates
//! and `@/query/*` channels. Reuses the tabbed foundation built for
//! netlink/netring (epic #257/#270).

use iced::widget::{Column, button, column, row, scrollable, text};
use iced::{Element, Length, Theme};
use zensight_common::TelemetryValue;
use zensight_common::query_detail::{CgroupNode, UnitRecord};

use crate::message::Message;
use crate::view::components::{TabItem, badge, card, empty_state, section_header, tabbed_view};
use crate::view::device::DeviceDetailState;
use crate::view::specialized::SpecializedTab;
use crate::view::specialized::fetch::Fetch;
use crate::view::specialized::systemd_detail::{SystemdDetailTopic, SystemdEventRecord};
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
    let header = row![
        section_header("Units", None),
        refresh_button(SystemdDetailTopic::Units),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);

    let filters = row![
        filter_chip("all", d.unit_state_filter.is_none(), None),
        filter_chip(
            "active",
            d.unit_state_filter.as_deref() == Some("active"),
            Some("active")
        ),
        filter_chip(
            "failed",
            d.unit_state_filter.as_deref() == Some("failed"),
            Some("failed")
        ),
        filter_chip(
            "inactive",
            d.unit_state_filter.as_deref() == Some("inactive"),
            Some("inactive"),
        ),
    ]
    .spacing(space::XS);

    let body = fetch_body(&d.units, SystemdDetailTopic::Units, |units| {
        let filter = d.unit_state_filter.as_deref();
        let rows: Vec<&UnitRecord> = units
            .iter()
            .filter(|u| filter.is_none_or(|f| u.active_state == f))
            .collect();
        if rows.is_empty() {
            return empty_state("No matching units.", None);
        }
        let mut list = column![table_header(&[
            "Unit",
            "Active",
            "Sub",
            "Load",
            "Description",
            "Actions"
        ])]
        .spacing(2);
        for u in rows.iter().take(400) {
            list = list.push(
                row![
                    cell(&u.name, 3),
                    state_cell(&u.active_state, 1),
                    cell(&u.sub_state, 1),
                    cell(&u.load_state, 1),
                    cell(&u.description, 3),
                    action_cell(u, d.pending_action.as_ref()),
                ]
                .spacing(space::SM)
                .align_y(iced::Alignment::Center),
            );
        }
        column![list, count_note(rows.len(), "units")]
            .spacing(space::SM)
            .into()
    });

    column![card(column![header, filters, body].spacing(space::SM))].spacing(space::MD)
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
                 Author expectations here; they hot-swap on the sensor via \
                 @/commands/expectations."
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
    let mut b = button(text(label).size(font::CAPTION))
        .on_press(Message::SystemdSetUnitFilter(value.map(str::to_string)))
        .padding([space::XS as u16, space::SM as u16]);
    b = if active {
        b.style(iced::widget::button::primary)
    } else {
        b.style(iced::widget::button::text)
    };
    b.into()
}

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

/// Per-unit service-control cell (#283): start/stop/restart buttons that arm an
/// inline confirm — the sensor side is allowlisted and off by default, so the
/// confirmed command may still come back rejected (surfaced via toast).
fn action_cell<'a>(unit: &UnitRecord, pending: Option<&(String, String)>) -> Element<'a, Message> {
    let tiny = |label: &'a str| button(text(label).size(font::CAPTION)).padding([2, 6]);
    let inner: Element<'a, Message> = match pending {
        Some((verb, armed_unit)) if armed_unit == &unit.name => row![
            text(format!("{verb}?")).size(font::CAPTION),
            tiny("confirm").on_press(Message::SystemdUnitActionConfirm),
            tiny("cancel").on_press(Message::SystemdUnitActionCancel),
        ]
        .spacing(space::XS)
        .align_y(iced::Alignment::Center)
        .into(),
        _ => {
            let arm = |verb: &str| Message::SystemdUnitActionArm {
                verb: verb.to_string(),
                unit: unit.name.clone(),
            };
            row![
                tiny("start").on_press(arm("start")),
                tiny("stop").on_press(arm("stop")),
                tiny("restart").on_press(arm("restart")),
            ]
            .spacing(space::XS)
            .into()
        }
    };
    iced::widget::container(inner)
        .width(Length::FillPortion(2))
        .into()
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
fn state_cell<'a>(state: &str, portion: u16) -> Element<'a, Message> {
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
        .width(Length::FillPortion(portion))
        .style(styled)
        .into()
}

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
        let mut s = DeviceDetailState::new(DeviceId {
            protocol: Protocol::Systemd,
            source: "server01".into(),
        });
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
}
