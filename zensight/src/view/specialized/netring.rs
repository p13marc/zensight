//! Netring sensor specialized view — flows + per-app bandwidth.

use iced::Element;
use iced::widget::{Column, button, column, container, row, scrollable, text};
use iced::{Length, Theme};
use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::components::{card, empty_state, section_header};
use crate::view::device::DeviceDetailState;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Render the netring sensor specialized view.
pub fn netring_sensor_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let content = column![
        render_header(state),
        card(render_flows(state)),
        card(render_tcp_health(state)),
        card(render_bandwidth(state)),
        card(render_flow_detail(state)),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    row![
        text(format!("Netring: {}", state.device_id.source)).size(font::TITLE),
        text(format!("({} metrics)", state.metrics.len()))
            .size(font::CAPTION)
            .style(dim),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center)
    .into()
}

fn render_flows(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Flows", None);
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
    let title = section_header("TCP Health", None);
    if !state.metrics.keys().any(|k| k.starts_with("tcp/")) {
        return column![title, empty_state("No TCP reset data", None)]
            .spacing(space::SM)
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

/// On-demand recent-flow detail: a fetch button + the fetched flow table (P2 —
/// pulled from the sensor's `@/query/flows` channel, never streamed).
fn render_flow_detail(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = section_header("Recent Flows (on demand)", None);
    let fetch = button(text("Fetch Flows").size(font::CAPTION))
        .on_press(Message::FetchNetringFlows)
        .padding([4, 10]);
    let mut col = column![title, fetch].spacing(10);

    if let Some(flows) = &state.netring_detail.flows {
        let mut list = Column::new().spacing(3).push(
            row![
                cell("src", 190),
                cell("dst", 190),
                cell("proto", 60),
                cell("bytes", 90),
                cell("dur_ms", 80),
                cell("reason", 90),
            ]
            .spacing(8),
        );
        for f in flows.iter().take(200) {
            list = list.push(
                row![
                    cell(&f.src, 190),
                    cell(&f.dst, 190),
                    cell(&f.proto, 60),
                    cell(&f.bytes.to_string(), 90),
                    cell(&f.duration_ms.to_string(), 80),
                    cell(&f.reason, 90),
                ]
                .spacing(8),
            );
        }
        col = col
            .push(text(format!("{} flows", flows.len())).size(font::EMPHASIS))
            .push(list);
    }
    col.into()
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

    let title = section_header(format!("Top Talkers ({})", rows.len()), None);
    if rows.is_empty() {
        return column![title, empty_state("No bandwidth data", None)]
            .spacing(space::SM)
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
