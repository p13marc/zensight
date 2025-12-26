//! gNMI streaming telemetry specialized view.
//!
//! Displays OpenConfig-style hierarchical data with a path browser,
//! active subscriptions, and path-based navigation.

use std::collections::HashMap;

use iced::widget::{Column, Row, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::icons::{self, IconSize};

/// Render the gNMI streaming telemetry specialized view.
pub fn gnmi_streaming_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let device_info = render_device_info(state);
    let subscriptions = render_subscriptions(state);
    let path_browser = render_path_browser(state);

    let content = column![
        header,
        rule::horizontal(1),
        device_info,
        rule::horizontal(1),
        subscriptions,
        rule::horizontal(1),
        path_browser,
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and target info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let device_name = text(&state.device_id.source).size(24);

    let metric_count = text(format!("{} paths", state.metrics.len())).size(14);

    row![back_button, protocol_icon, device_name, metric_count]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render device information section.
fn render_device_info(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut info_items: Vec<Element<'_, Message>> = Vec::new();

    // Try to get system info from common OpenConfig paths
    let system_paths = [
        ("hostname", "system/config/hostname"),
        ("hostname", "system/state/hostname"),
        ("model", "components/component[name=chassis]/state/part-no"),
        ("software", "system/state/software-version"),
        (
            "vendor",
            "components/component[name=chassis]/state/mfg-name",
        ),
    ];

    for (label, path) in system_paths {
        if let Some(value) = get_metric_text(state, path) {
            info_items.push(
                row![text(format!("{}:", label)).size(12), text(value).size(12)]
                    .spacing(8)
                    .into(),
            );
        }
    }

    // Also check labels
    if let Some(point) = state.metrics.values().next()
        && let Some(target) = point.labels.get("target") {
            info_items.push(
                row![text("Target:").size(12), text(target).size(12)]
                    .spacing(8)
                    .into(),
            );
        }

    if info_items.is_empty() {
        info_items.push(
            text("gNMI Target")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
                })
                .into(),
        );
    }

    container(Row::with_children(info_items).spacing(30))
        .padding(10)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render active subscriptions section.
fn render_subscriptions(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::subscription(IconSize::Medium),
        text("Active Subscriptions").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // Infer subscriptions from metric paths
    let mut subscription_prefixes: HashMap<String, usize> = HashMap::new();

    for key in state.metrics.keys() {
        // Get the first two path segments as a subscription prefix
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() >= 2 {
            let prefix = format!("/{}/{}", parts[0], parts[1]);
            *subscription_prefixes.entry(prefix).or_insert(0) += 1;
        } else if !parts.is_empty() {
            let prefix = format!("/{}", parts[0]);
            *subscription_prefixes.entry(prefix).or_insert(0) += 1;
        }
    }

    let mut sorted_subs: Vec<_> = subscription_prefixes.into_iter().collect();
    sorted_subs.sort_by(|a, b| a.0.cmp(&b.0));

    let is_empty = sorted_subs.is_empty();
    let mut sub_list = Column::new().spacing(4);

    for (prefix, count) in sorted_subs.into_iter().take(10) {
        let sub_row = row![
            text("•").size(12).style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.4, 0.8, 0.4)),
            }),
            text(prefix).size(12).style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.6, 0.8, 0.9)),
            }),
            text(format!("({} paths)", count))
                .size(10)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                }),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        sub_list = sub_list.push(sub_row);
    }

    if is_empty {
        sub_list = sub_list.push(text("No active subscriptions").size(12).style(
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            },
        ));
    }

    column![title, sub_list].spacing(10).into()
}

/// Render the path browser as a sorted list of paths with values.
fn render_path_browser(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::tree(IconSize::Medium), text("Path Browser").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    // Collect and sort paths
    let mut paths: Vec<(String, String)> = state
        .metrics
        .iter()
        .map(|(path, point)| (path.clone(), format_value(&point.value)))
        .collect();
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let is_empty = paths.is_empty();
    let mut path_list = Column::new().spacing(2);

    for (path, value) in paths.into_iter().take(100) {
        // Count depth for indentation
        let depth = path.matches('/').count();
        let indent_str = "  ".repeat(depth.min(6));

        // Get the last segment for display
        let last_segment = path.split('/').next_back().unwrap_or(&path);

        // Highlight keys like [name=value]
        let name_style = if last_segment.contains('[') {
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.8, 0.6, 0.4)),
            }
        } else {
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.6, 0.8, 0.9)),
            }
        };

        let value_display = if value.len() > 40 {
            format!("{}...", &value[..37])
        } else {
            value
        };

        let path_row = row![
            text(indent_str).size(10),
            text("○").size(10).style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.7, 0.9)),
            }),
            text(last_segment.to_string()).size(11).style(name_style),
            text(format!(": {}", value_display))
                .size(10)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.8, 0.5)),
                }),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let row_container = container(path_row).padding(2).width(Length::Fill);
        path_list = path_list.push(row_container);
    }

    if is_empty {
        path_list = path_list.push(text("No paths available").size(12).style(|_theme: &Theme| {
            text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            }
        }));
    }

    let scroll = scrollable(path_list)
        .width(Length::Fill)
        .height(Length::FillPortion(1));

    column![title, scroll]
        .spacing(10)
        .height(Length::Fill)
        .into()
}

fn get_metric_text(state: &DeviceDetailState, metric: &str) -> Option<String> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Text(s) => Some(s.clone()),
            _ => None,
        })
}

fn format_value(value: &TelemetryValue) -> String {
    match value {
        TelemetryValue::Counter(v) => format!("{}", v),
        TelemetryValue::Gauge(v) => {
            if v.fract() == 0.0 {
                format!("{:.0}", v)
            } else {
                format!("{:.2}", v)
            }
        }
        TelemetryValue::Text(s) => s.clone(),
        TelemetryValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        TelemetryValue::Binary(data) => format!("<{} bytes>", data.len()),
    }
}

fn section_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgb(
            0.12, 0.12, 0.14,
        ))),
        border: iced::Border {
            color: iced::Color::from_rgb(0.25, 0.25, 0.3),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::Protocol;

    #[test]
    fn test_format_value() {
        assert_eq!(format_value(&TelemetryValue::Counter(42)), "42");
        assert_eq!(format_value(&TelemetryValue::Gauge(3.14)), "3.14");
        assert_eq!(format_value(&TelemetryValue::Gauge(100.0)), "100");
        assert_eq!(
            format_value(&TelemetryValue::Text("hello".to_string())),
            "hello"
        );
        assert_eq!(format_value(&TelemetryValue::Boolean(true)), "true");
        assert_eq!(format_value(&TelemetryValue::Boolean(false)), "false");
        assert_eq!(
            format_value(&TelemetryValue::Binary(vec![1, 2, 3])),
            "<3 bytes>"
        );
    }

    #[test]
    fn test_gnmi_view_renders() {
        let device_id = DeviceId::new(Protocol::Gnmi, "spine01");
        let state = DeviceDetailState::new(device_id);
        let _view = gnmi_streaming_view(&state);
    }

    #[test]
    fn test_gnmi_view_with_metrics() {
        let device_id = DeviceId::new(Protocol::Gnmi, "spine01");
        let mut state = DeviceDetailState::new(device_id);

        // Add some test metrics
        use zensight_common::TelemetryPoint;
        state.update(TelemetryPoint::new(
            "spine01",
            Protocol::Gnmi,
            "interfaces/interface/state/name",
            TelemetryValue::Text("eth0".to_string()),
        ));
        state.update(TelemetryPoint::new(
            "spine01",
            Protocol::Gnmi,
            "interfaces/interface/state/counters/in-octets",
            TelemetryValue::Counter(1234567),
        ));

        // Should render without panicking
        let _view = gnmi_streaming_view(&state);

        // Verify metrics are stored
        assert_eq!(state.metrics.len(), 2);
    }
}
