//! Netring overview — aggregates flow/bandwidth/TCP-health across sensors.

use std::collections::HashMap;

use iced::widget::{column, row, text};
use iced::{Alignment, Element, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;
use crate::view::theme;

#[derive(Default)]
struct NetringAgg {
    active_flows: u64,
    ended_total: u64,
    bytes_total: u64,
    tcp_resets: u64,
}

fn num(value: &TelemetryValue) -> u64 {
    match value {
        TelemetryValue::Counter(c) => *c,
        TelemetryValue::Gauge(g) => *g as u64,
        _ => 0,
    }
}

fn aggregate(devices: &HashMap<&DeviceId, &DeviceState>) -> NetringAgg {
    let mut agg = NetringAgg::default();
    for state in devices.values() {
        for (key, point) in &state.metrics {
            match key.as_str() {
                "flow/active" => agg.active_flows += num(&point.value),
                "flow/ended_total" => agg.ended_total += num(&point.value),
                "flow/bytes_total" => agg.bytes_total += num(&point.value),
                "tcp/resets_total" => agg.tcp_resets += num(&point.value),
                _ => {}
            }
        }
    }
    agg
}

/// Format a byte count compactly (B/KB/MB/GB).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render the netring overview.
pub fn netring_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return muted("No netring sensors available");
    }

    let healthy = devices.values().filter(|d| d.is_healthy).count();
    let agg = aggregate(devices);

    let summary = row![
        stat("Sensors", devices.len().to_string()),
        status_stat("Online", healthy, StatusLedState::Active),
        status_stat("Offline", devices.len() - healthy, StatusLedState::Inactive),
        stat("Active flows", agg.active_flows.to_string()),
        stat("Flows ended", agg.ended_total.to_string()),
        stat("Traffic", human_bytes(agg.bytes_total)),
        stat("TCP resets", agg.tcp_resets.to_string()),
    ]
    .spacing(25)
    .align_y(Alignment::Center);

    column![summary].spacing(8).into()
}

fn muted<'a>(s: &'a str) -> Element<'a, Message> {
    text(s)
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
}

fn stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        text(value).size(16)
    ]
    .spacing(2)
    .into()
}

fn status_stat<'a>(label: &'a str, count: usize, state: StatusLedState) -> Element<'a, Message> {
    let led = StatusLed::new(state).with_size(10.0);
    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        row![led.view(), text(count.to_string()).size(16)]
            .spacing(6)
            .align_y(Alignment::Center)
    ]
    .spacing(2)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
