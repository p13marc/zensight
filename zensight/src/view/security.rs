//! Security view — a network-anomaly lens over sensor-pushed alerts.
//!
//! ZenSight's first security surface: it filters the `external` alert set to
//! [`AlertKind::Anomaly`] (port scans, beacons, DGA, ...) and presents them
//! grouped by detector with the evidence each detector emitted, a by-source
//! rollup, a severity filter, and a per-anomaly drill-down (#48).

use std::collections::BTreeMap;

use iced::widget::{Column, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;
use zensight_common::{Alert, AlertKind, AlertSeverity};

use crate::message::Message;
use crate::view::alerts::{AlertsState, Severity};
use crate::view::components::badge;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::tokens::font;

/// View-local state for the Security view (#48): a severity filter and the
/// currently expanded anomaly (by `alert_key`).
#[derive(Debug, Default, Clone)]
pub struct SecurityState {
    /// When true, Info-severity anomalies (benign scans, inconclusive DGA) are
    /// hidden from the cards and feed.
    pub hide_info: bool,
    /// The anomaly whose evidence is expanded in the drill-down, if any.
    pub selected: Option<String>,
}

/// Labels shown elsewhere (summary/source), so we don't repeat them as evidence.
const NON_EVIDENCE_LABELS: &[&str] = &["src", "dst", "proto"];

/// Render the security view.
pub fn security_view<'a>(alerts: &'a AlertsState, sec: &'a SecurityState) -> Element<'a, Message> {
    let mut anomalies: Vec<&Alert> = alerts
        .active_external()
        .into_iter()
        .filter(|a| a.kind == AlertKind::Anomaly)
        .filter(|a| !sec.hide_info || a.severity != AlertSeverity::Info)
        .collect();
    // Stable order: severity desc, then most recent.
    anomalies.sort_by(|a, b| {
        sev_rank(b.severity)
            .cmp(&sev_rank(a.severity))
            .then(b.timestamp.cmp(&a.timestamp))
    });

    let content = column![
        render_header(anomalies.len(), sec),
        rule::horizontal(1),
        render_by_source(&anomalies),
        rule::horizontal(1),
        render_by_detector(&anomalies, sec),
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn sev_rank(s: AlertSeverity) -> u8 {
    match s {
        AlertSeverity::Critical => 3,
        AlertSeverity::Warning => 2,
        AlertSeverity::Info => 1,
    }
}

fn render_header<'a>(count: usize, sec: &SecurityState) -> Element<'a, Message> {
    let back = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseSecurity)
    .style(iced::widget::button::secondary);

    // Cross-link back to the operational Alerts surface (#39).
    let all_alerts = button(text("← All alerts").size(13))
        .on_press(Message::OpenAlerts)
        .style(iced::widget::button::secondary);

    // Severity filter toggle (#48).
    let filter = button(
        text(if sec.hide_info {
            "Show info"
        } else {
            "Hide info"
        })
        .size(13),
    )
    .on_press(Message::ToggleSecurityHideInfo)
    .style(if sec.hide_info {
        iced::widget::button::primary
    } else {
        iced::widget::button::secondary
    });

    let header_row = row![
        back,
        text("Security — Network Anomalies").size(22),
        text(format!("({count} active)")).size(13).style(dim),
        all_alerts,
        filter,
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    let subtitle = text("Network security anomalies — port scans, beacons, DGA, floods")
        .size(font::CAPTION)
        .style(dim);

    column![header_row, subtitle].spacing(4).into()
}

/// Rank offending sources by anomaly count.
fn render_by_source<'a>(anomalies: &[&'a Alert]) -> Element<'a, Message> {
    let title = text("Top offenders").size(18);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for a in anomalies {
        let src = a
            .labels
            .get("src")
            .cloned()
            .unwrap_or_else(|| a.source.clone());
        *counts.entry(src).or_default() += 1;
    }
    if counts.is_empty() {
        return column![title, text("No anomalies").size(13).style(dim)]
            .spacing(8)
            .into();
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    let mut list = Column::new().spacing(4);
    for (src, n) in ranked.iter().take(20) {
        list = list.push(
            row![
                text(src.clone()).size(13).width(Length::Fixed(240.0)),
                text(format!("{n} anomalies")).size(12).style(dim),
            ]
            .spacing(10),
        );
    }
    column![title, list].spacing(8).into()
}

/// Group anomalies by detector (`rule`) and render one card per detector, each
/// row clickable to expand its evidence labels (#48).
fn render_by_detector<'a>(anomalies: &[&'a Alert], sec: &'a SecurityState) -> Element<'a, Message> {
    let title = text("By detector").size(18);
    if anomalies.is_empty() {
        return column![
            title,
            text("Quiet — no active anomalies").size(13).style(dim)
        ]
        .spacing(8)
        .into();
    }

    // Preserve the incoming (severity-sorted) order within each detector group.
    let mut groups: Vec<(String, Vec<&Alert>)> = Vec::new();
    for a in anomalies {
        match groups.iter_mut().find(|(r, _)| *r == a.rule) {
            Some((_, v)) => v.push(a),
            None => groups.push((a.rule.clone(), vec![a])),
        }
    }
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let mut col = Column::new().spacing(12).push(title);
    for (rule_name, group) in groups {
        let top_sev = group
            .iter()
            .map(|a| a.severity)
            .max_by_key(|s| sev_rank(*s))
            .unwrap_or(AlertSeverity::Info);
        let header = row![
            badge(Severity::from(top_sev).color(), rule_name.clone()),
            text(format!("{} detection(s)", group.len()))
                .size(font::CAPTION)
                .style(dim),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let mut card_col = Column::new().spacing(4).push(header);
        for a in group {
            card_col = card_col.push(render_anomaly_row(a, sec));
        }
        let card_body: Element<'a, Message> = card_col.into();
        col = col.push(crate::view::components::card(card_body));
    }
    col.into()
}

/// One anomaly row: clickable to toggle its evidence drill-down (#48).
fn render_anomaly_row<'a>(a: &'a Alert, sec: &SecurityState) -> Element<'a, Message> {
    let key = a.alert_key();
    let expanded = sec.selected.as_deref() == Some(key.as_str());
    let sev: Severity = a.severity.into();
    let color = sev.color();

    let summary_line = row![
        text(if expanded { "▾" } else { "▸" }).size(11),
        text(a.summary.clone()).size(13).width(Length::Fixed(420.0)),
        text(sev.name())
            .size(11)
            .style(move |_t: &Theme| text::Style { color: Some(color) }),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Clicking toggles the drill-down (select, or deselect if already open).
    let toggle = button(summary_line)
        .on_press(Message::SelectAnomaly(if expanded {
            None
        } else {
            Some(key)
        }))
        .style(iced::widget::button::text)
        .padding(2);

    if !expanded {
        return toggle.into();
    }

    // Drill-down: endpoints + every evidence label the detector emitted (the
    // "why it fired" the view used to discard).
    let mut detail = Column::new().spacing(2);
    if let Some(src) = a.labels.get("src") {
        detail = detail.push(evidence_line("src", src));
    }
    if let Some(dst) = a.labels.get("dst") {
        detail = detail.push(evidence_line("dst", dst));
    }
    let mut evidence: Vec<(&String, &String)> = a
        .labels
        .iter()
        .filter(|(k, _)| !NON_EVIDENCE_LABELS.contains(&k.as_str()))
        .collect();
    evidence.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in evidence {
        detail = detail.push(evidence_line(k, v));
    }
    if a.labels.is_empty() {
        detail = detail.push(text("no evidence labels").size(font::CAPTION).style(dim));
    }

    column![toggle, container(detail).padding([4, 20]),]
        .spacing(2)
        .into()
}

fn evidence_line<'a>(k: &str, v: &str) -> Element<'a, Message> {
    row![
        text(format!("{k}:"))
            .size(11)
            .width(Length::Fixed(160.0))
            .style(dim),
        text(v.to_string()).size(11),
    ]
    .spacing(8)
    .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

/// Count of active anomaly alerts (for the sidebar badge).
pub fn anomaly_count(alerts: &AlertsState) -> usize {
    alerts
        .external
        .values()
        .filter(|a| a.kind == AlertKind::Anomaly && a.severity != AlertSeverity::Info)
        .count()
}
