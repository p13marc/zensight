//! Security view — a network-anomaly lens over sensor-pushed alerts.
//!
//! ZenSight's first security surface: it filters the `external` alert set to
//! [`AlertKind::Anomaly`] (port scans, beacons, DGA, ...), with a by-source
//! rollup and a live feed.

use std::collections::BTreeMap;

use iced::widget::{Column, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;
use zensight_common::{Alert, AlertKind, AlertSeverity};

use crate::message::Message;
use crate::view::alerts::{AlertsState, Severity};
use crate::view::icons::{self, IconSize};
use crate::view::theme;

/// Render the security view.
pub fn security_view(alerts: &AlertsState) -> Element<'_, Message> {
    let anomalies: Vec<&Alert> = alerts
        .active_external()
        .into_iter()
        .filter(|a| a.kind == AlertKind::Anomaly)
        .collect();

    let content = column![
        render_header(anomalies.len()),
        rule::horizontal(1),
        render_by_source(&anomalies),
        rule::horizontal(1),
        render_feed(&anomalies),
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_header<'a>(count: usize) -> Element<'a, Message> {
    let back = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseSecurity)
    .style(iced::widget::button::secondary);

    row![
        back,
        text("Security — Network Anomalies").size(22),
        text(format!("({count} active)")).size(13).style(dim),
    ]
    .spacing(15)
    .align_y(Alignment::Center)
    .into()
}

/// Rank offending sources by anomaly count.
fn render_by_source<'a>(anomalies: &[&'a Alert]) -> Element<'a, Message> {
    let title = text("By source").size(18);
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

fn render_feed<'a>(anomalies: &[&'a Alert]) -> Element<'a, Message> {
    let title = text("Live feed").size(18);
    if anomalies.is_empty() {
        return column![
            title,
            text("Quiet — no active anomalies").size(13).style(dim)
        ]
        .spacing(8)
        .into();
    }
    let mut list = Column::new().spacing(5);
    for a in anomalies {
        let sev: Severity = a.severity.into();
        let icon: Element<'a, Message> = match sev {
            Severity::Critical => icons::status_error(IconSize::Small),
            Severity::Warning => icons::status_warning(IconSize::Small),
            Severity::Info => icons::info(IconSize::Small),
        };
        let color = sev.color();
        list = list.push(
            row![
                icon,
                text(a.rule.clone())
                    .size(11)
                    .width(Length::Fixed(120.0))
                    .style(move |_t: &Theme| text::Style { color: Some(color) }),
                text(a.summary.clone()).size(13).width(Length::Fixed(360.0)),
                text(sev.name()).size(11),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    column![title, list].spacing(8).into()
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
