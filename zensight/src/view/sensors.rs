//! Sensors view — surfaces sensor health that was previously collected into app
//! state (`sensor_health`) but never shown. One card per sensor with a health
//! badge, device counts, last-poll latency, error rate, and throughput.
//!
//! See docs/plans/gui/04-features-and-stubs.md (F2).

use std::collections::{HashMap, VecDeque};

use iced::widget::{column, container, row, scrollable, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};

use zensight_common::{ErrorReport, HealthSnapshot, HealthStatus};

use crate::message::Message;
use crate::view::blob_fetch::{BlobFetch, download_section};
use crate::view::components::{card, empty_state, section_header};
use crate::view::dir_fetch::{DirFetch, dir_section};
use crate::view::formatting::format_timestamp;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the sensors view. `blob_fetch`/`active_prefix` drive the per-sensor
/// debug-report download control (#197); `dir_fetch`/`snapshot_dirs`/`dir_prefix`
/// drive the Tier-2 directory-snapshot download control (#199 follow-up).
#[allow(clippy::too_many_arguments)]
pub fn sensors_view<'a>(
    sensor_health: &'a HashMap<String, HealthSnapshot>,
    recent_errors: &'a HashMap<String, VecDeque<ErrorReport>>,
    blob_fetch: &'a BlobFetch,
    active_prefix: Option<&'a str>,
    dir_fetch: &'a DirFetch,
    snapshot_dirs: &'a HashMap<String, Vec<String>>,
    dir_prefix: Option<&'a str>,
) -> Element<'a, Message> {
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
        let errors = recent_errors.get(&snap.sensor);
        list = list.push(card(sensor_card(
            snap,
            errors,
            blob_fetch,
            active_prefix,
            dir_fetch,
            snapshot_dirs,
            dir_prefix,
        )));
    }

    container(scrollable(list))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[allow(clippy::too_many_arguments)]
fn sensor_card<'a>(
    snap: &'a HealthSnapshot,
    errors: Option<&'a VecDeque<ErrorReport>>,
    blob_fetch: &'a BlobFetch,
    active_prefix: Option<&'a str>,
    dir_fetch: &'a DirFetch,
    snapshot_dirs: &'a HashMap<String, Vec<String>>,
    dir_prefix: Option<&'a str>,
) -> Element<'a, Message> {
    let header = section_header(snap.sensor.clone(), Some(health_badge(snap.status)));

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

    let mut col = column![header, stats].spacing(space::SM);

    // Debug-report download control (#197). The key prefix is `zensight/<sensor>`.
    let key_prefix = format!("zensight/{}", snap.sensor);
    col = col.push(download_section(blob_fetch, &key_prefix, active_prefix));

    // Tier-2 directory-snapshot download control (#199 follow-up), if this sensor
    // advertises any directories.
    if let Some(dirs) = snapshot_dirs.get(&key_prefix) {
        col = col.push(dir_section(dir_fetch, &key_prefix, dirs, dir_prefix));
    }

    // Recent errors (newest first), if any have arrived for this sensor.
    if let Some(errors) = errors.filter(|e| !e.is_empty()) {
        col = col.push(
            text(format!("Recent errors ({})", errors.len()))
                .size(font::CAPTION)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme::colors(theme).text_muted()),
                }),
        );
        for report in errors.iter().rev().take(5) {
            let when = format_timestamp(report.timestamp);
            let dev = report.device.as_deref().unwrap_or("-");
            let line = format!(
                "{when}  [{:?}] {dev}: {}",
                report.error_type, report.message
            );
            col = col.push(
                text(line)
                    .size(font::CAPTION)
                    .style(|theme: &Theme| text::Style {
                        color: Some(theme::colors(theme).danger()),
                    }),
            );
        }
    }

    col.into()
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
        text(label)
            .size(font::CAPTION)
            .style(|theme: &Theme| text::Style {
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
