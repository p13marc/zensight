//! Netring sensor specialized view — flows + per-app bandwidth.

use iced::Element;
use iced::widget::{Column, column, container, row, rule, scrollable, text};
use iced::{Length, Theme};
use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::theme;

/// Render the netring sensor specialized view.
pub fn netring_sensor_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let content = column![
        render_header(state),
        rule::horizontal(1),
        render_flows(state),
        rule::horizontal(1),
        render_tcp_health(state),
        rule::horizontal(1),
        render_bandwidth(state),
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    row![
        text(format!("Netring: {}", state.device_id.source)).size(22),
        text(format!("({} metrics)", state.metrics.len()))
            .size(12)
            .style(dim),
    ]
    .spacing(12)
    .into()
}

fn render_flows(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = text("Flows").size(18);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    column![
        title,
        row![
            cell("started (total)", 160),
            cell(&get("flow/started_total"), 100)
        ]
        .spacing(8),
        row![
            cell("ended (total)", 160),
            cell(&get("flow/ended_total"), 100)
        ]
        .spacing(8),
        row![cell("active", 160), cell(&get("flow/active"), 100)].spacing(8),
        row![
            cell("bytes (total)", 160),
            cell(&get("flow/bytes_total"), 100)
        ]
        .spacing(8),
        row![
            cell("packets (total)", 160),
            cell(&get("flow/packets_total"), 100)
        ]
        .spacing(8),
        row![
            cell("retransmits (total)", 160),
            cell(&get("flow/retransmits_total"), 100)
        ]
        .spacing(8),
    ]
    .spacing(4)
    .into()
}

/// TCP health: reset / connection-refused counters.
fn render_tcp_health(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = text("TCP Health").size(18);
    if !state.metrics.keys().any(|k| k.starts_with("tcp/")) {
        return column![title, text("No TCP reset data").size(13).style(dim)]
            .spacing(8)
            .into();
    }
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    column![
        title,
        row![cell("resets (total)", 160), cell(&get("tcp/resets_total"), 100)].spacing(8),
        row![
            cell("refused (total)", 160),
            cell(&get("tcp/refused_total"), 100)
        ]
        .spacing(8),
    ]
    .spacing(4)
    .into()
}

fn render_bandwidth(state: &DeviceDetailState) -> Element<'_, Message> {
    // Collect `bandwidth/<app>/bytes_per_sec` and sort by value desc.
    let mut rows: Vec<(String, f64)> = state
        .metrics
        .iter()
        .filter_map(|(metric, point)| {
            let app = metric
                .strip_prefix("bandwidth/")?
                .strip_suffix("/bytes_per_sec")?;
            let bps = match &point.value {
                TelemetryValue::Gauge(g) => *g,
                TelemetryValue::Counter(c) => *c as f64,
                _ => return None,
            };
            Some((app.to_string(), bps))
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    let title = text(format!("Top Talkers ({})", rows.len())).size(18);
    if rows.is_empty() {
        return column![title, text("No bandwidth data").size(13).style(dim)]
            .spacing(8)
            .into();
    }

    let mut list = Column::new()
        .spacing(4)
        .push(row![cell("application", 200), cell("bytes/sec", 140)].spacing(8));
    for (app, bps) in rows.iter().take(30) {
        list = list.push(row![cell(app, 200), cell(&format!("{bps:.0}"), 140)].spacing(8));
    }
    column![title, list].spacing(8).into()
}

fn cell<'a>(s: &str, width: u16) -> Element<'a, Message> {
    text(s.to_string())
        .size(12)
        .width(Length::Fixed(width as f32))
        .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

fn num(v: Option<&TelemetryValue>) -> String {
    match v {
        Some(TelemetryValue::Counter(c)) => c.to_string(),
        Some(TelemetryValue::Gauge(g)) => format!("{g:.0}"),
        Some(TelemetryValue::Text(s)) => s.clone(),
        Some(TelemetryValue::Boolean(b)) => b.to_string(),
        _ => "-".into(),
    }
}
