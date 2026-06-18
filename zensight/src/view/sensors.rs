//! Sensors view — surfaces sensor health that was previously collected into app
//! state (`sensor_health`) but never shown. One card per sensor with a health
//! badge, device counts, last-poll latency, error rate, and throughput.
//!
//! See docs/plans/gui/04-features-and-stubs.md (F2).

use std::collections::HashMap;

use iced::widget::{column, container, row, scrollable, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};

use zensight_common::{HealthSnapshot, HealthStatus};

use crate::message::Message;
use crate::view::components::{card, empty_state, section_header};
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the sensors view.
pub fn sensors_view(sensor_health: &HashMap<String, HealthSnapshot>) -> Element<'_, Message> {
    let title = text("Sensors").size(font::TITLE);

    if sensor_health.is_empty() {
        let body = empty_state("No sensor health received yet.", None);
        return container(column![title, body].spacing(space::MD))
            .padding(space::LG)
            .into();
    }

    // Stable order by sensor name.
    let mut sensors: Vec<&HealthSnapshot> = sensor_health.values().collect();
    sensors.sort_by(|a, b| a.sensor.cmp(&b.sensor));

    let mut list = column![title].spacing(space::MD).padding(space::LG);
    for snap in sensors {
        list = list.push(card(sensor_card(snap)));
    }

    container(scrollable(list))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn sensor_card(snap: &HealthSnapshot) -> Element<'_, Message> {
    let header = section_header(
        snap.sensor.clone(),
        Some(health_badge(snap.status)),
    );

    let stats = row![
        stat("Devices", format!("{}", snap.devices_total)),
        stat("Responding", format!("{}", snap.devices_responding)),
        stat("Failed", format!("{}", snap.devices_failed)),
        stat("Last poll", format!("{} ms", snap.last_poll_duration_ms)),
        stat("Errors/hr", format!("{}", snap.errors_last_hour)),
        stat("Metrics", format!("{}", snap.metrics_published)),
        stat("Uptime", human_uptime(snap.uptime_secs)),
    ]
    .spacing(space::LG)
    .align_y(Alignment::Center);

    column![header, stats].spacing(space::SM).into()
}

/// A status badge (colored dot + label), themed so the dot color follows the
/// active theme's status palette. Never color-alone (the label is always shown).
fn health_badge<'a>(status: HealthStatus) -> Element<'a, Message> {
    let label = match status {
        HealthStatus::Healthy => "Healthy",
        HealthStatus::Degraded => "Degraded",
        HealthStatus::Unhealthy => "Unhealthy",
        HealthStatus::Starting => "Starting",
        HealthStatus::Error => "Error",
    };
    let dot = container(text(""))
        .width(10)
        .height(10)
        .style(move |theme: &Theme| {
            let c = theme::colors(theme);
            let color: Color = match status {
                HealthStatus::Healthy => c.status_healthy(),
                HealthStatus::Degraded => c.status_degraded(),
                HealthStatus::Unhealthy | HealthStatus::Error => c.status_error(),
                HealthStatus::Starting => c.status_unknown(),
            };
            container::Style {
                background: Some(Background::Color(color)),
                border: Border::default().rounded(5.0),
                ..Default::default()
            }
        });
    row![dot, text(label).size(font::CAPTION)]
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .into()
}

fn stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(font::CAPTION).style(|theme: &Theme| text::Style {
            color: Some(theme::colors(theme).text_muted()),
        }),
        text(value).size(font::EMPHASIS),
    ]
    .spacing(2)
    .into()
}

/// Compact uptime: `42s` / `12m` / `3h` / `2d`.
fn human_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_formats() {
        assert_eq!(human_uptime(45), "45s");
        assert_eq!(human_uptime(120), "2m");
        assert_eq!(human_uptime(7200), "2h");
        assert_eq!(human_uptime(172800), "2d");
    }
}
