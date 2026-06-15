//! Netlink host specialized view — interfaces + TCP socket aggregates.

use std::collections::BTreeMap;

use iced::widget::{Column, column, container, row, rule, scrollable, text};
use iced::{Element, Length, Theme};
use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::theme;

/// Render the netlink host specialized view.
pub fn netlink_host_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let content = column![
        render_header(state),
        rule::horizontal(1),
        render_interfaces(state),
        rule::horizontal(1),
        render_sockets(state),
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
        text(format!("Netlink: {}", state.device_id.source)).size(22),
        text(format!("({} metrics)", state.metrics.len()))
            .size(12)
            .style(dim),
    ]
    .spacing(12)
    .into()
}

/// Group `iface/<name>/<stat>` metrics by interface name.
fn interfaces(state: &DeviceDetailState) -> BTreeMap<String, BTreeMap<String, &TelemetryValue>> {
    let mut map: BTreeMap<String, BTreeMap<String, &TelemetryValue>> = BTreeMap::new();
    for (metric, point) in &state.metrics {
        if let Some(rest) = metric.strip_prefix("iface/")
            && let Some((name, stat)) = rest.split_once('/')
        {
            map.entry(name.to_string())
                .or_default()
                .insert(stat.to_string(), &point.value);
        }
    }
    map
}

fn render_interfaces(state: &DeviceDetailState) -> Element<'_, Message> {
    let ifaces = interfaces(state);
    let title = text(format!("Interfaces ({})", ifaces.len())).size(18);

    if ifaces.is_empty() {
        return column![title, text("No interface data").size(13).style(dim)]
            .spacing(8)
            .into();
    }

    // Header row.
    let mut list = Column::new().spacing(4).push(
        row![
            cell("interface", 140),
            cell("state", 80),
            cell("mtu", 70),
            cell("rx bytes", 120),
            cell("tx bytes", 120),
            cell("rx drop", 90),
            cell("tx drop", 90),
        ]
        .spacing(8),
    );

    for (name, stats) in &ifaces {
        let st = stats
            .get("oper_state")
            .and_then(text_val)
            .unwrap_or_else(|| match stats.get("up").and_then(bool_val) {
                Some(true) => "up".to_string(),
                Some(false) => "down".to_string(),
                None => "-".into(),
            });
        list = list.push(
            row![
                cell(name, 140),
                cell(&st, 80),
                cell(&num(stats.get("mtu").copied()), 70),
                cell(&num(stats.get("rx_bytes").copied()), 120),
                cell(&num(stats.get("tx_bytes").copied()), 120),
                cell(&num(stats.get("rx_dropped").copied()), 90),
                cell(&num(stats.get("tx_dropped").copied()), 90),
            ]
            .spacing(8),
        );
    }

    column![title, list].spacing(8).into()
}

fn render_sockets(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = text("TCP Sockets").size(18);
    let get = |m: &str| num(state.metrics.get(m).map(|p| &p.value));
    let line =
        |label: &str, metric: &str| row![cell(label, 180), cell(&get(metric), 100)].spacing(8);

    if !state.metrics.keys().any(|k| k.starts_with("sockets/tcp/")) {
        return column![title, text("No socket data").size(13).style(dim)]
            .spacing(8)
            .into();
    }

    column![
        title,
        line("established", "sockets/tcp/established"),
        line("listen", "sockets/tcp/listen"),
        line("time_wait", "sockets/tcp/time_wait"),
        line("syn_sent", "sockets/tcp/syn_sent"),
        line("close_wait", "sockets/tcp/close_wait"),
        line("retransmits (total)", "sockets/tcp/retransmits_total"),
        line("max RTT (us)", "sockets/tcp/max_rtt_us"),
    ]
    .spacing(4)
    .into()
}

// ---- small helpers ----------------------------------------------------------

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

fn text_val(v: &&TelemetryValue) -> Option<String> {
    match v {
        TelemetryValue::Text(s) => Some(s.clone()),
        _ => None,
    }
}

fn bool_val(v: &&TelemetryValue) -> Option<bool> {
    match v {
        TelemetryValue::Boolean(b) => Some(*b),
        _ => None,
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
