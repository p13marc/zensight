//! Netlink overview — aggregates interface/socket/route health across hosts.

use std::collections::HashMap;

use iced::widget::{column, row, text};
use iced::{Alignment, Element, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;
use crate::view::theme;

#[derive(Default)]
struct NetlinkAgg {
    ifaces_total: usize,
    ifaces_up: usize,
    established: u64,
    default_routes: usize,
}

fn num(value: &TelemetryValue) -> f64 {
    match value {
        TelemetryValue::Counter(c) => *c as f64,
        TelemetryValue::Gauge(g) => *g,
        TelemetryValue::Boolean(true) => 1.0,
        TelemetryValue::Boolean(false) => 0.0,
        _ => 0.0,
    }
}

fn aggregate(devices: &HashMap<&DeviceId, &DeviceState>) -> NetlinkAgg {
    let mut agg = NetlinkAgg::default();
    for state in devices.values() {
        for (key, point) in &state.metrics {
            if let Some(rest) = key.strip_prefix("iface/")
                && rest.ends_with("/up")
            {
                agg.ifaces_total += 1;
                if matches!(point.value, TelemetryValue::Boolean(true)) {
                    agg.ifaces_up += 1;
                }
            } else if key == "sockets/tcp/established" {
                agg.established += num(&point.value) as u64;
            } else if key == "routes/default_v4_present"
                && matches!(point.value, TelemetryValue::Boolean(true))
            {
                agg.default_routes += 1;
            }
        }
    }
    agg
}

/// Render the netlink overview.
pub fn netlink_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return muted("No netlink hosts available");
    }

    let healthy = devices.values().filter(|d| d.is_healthy).count();
    let agg = aggregate(devices);

    let summary = row![
        stat("Hosts", devices.len().to_string()),
        status_stat("Online", healthy, StatusLedState::Active),
        status_stat("Offline", devices.len() - healthy, StatusLedState::Inactive),
        stat(
            "Interfaces up",
            format!("{}/{}", agg.ifaces_up, agg.ifaces_total)
        ),
        stat("TCP established", agg.established.to_string()),
        stat(
            "Default route",
            format!("{}/{}", agg.default_routes, devices.len())
        ),
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
