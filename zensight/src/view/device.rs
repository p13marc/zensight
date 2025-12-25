//! Device detail view showing all metrics for a selected device.

use std::collections::HashMap;

use iced::widget::{Column, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::{TelemetryPoint, TelemetryValue};

use crate::message::{DeviceId, Message};

/// State for the device detail view.
#[derive(Debug, Clone)]
pub struct DeviceDetailState {
    /// The device being viewed.
    pub device_id: DeviceId,
    /// All metrics for this device (metric name -> telemetry point).
    pub metrics: HashMap<String, TelemetryPoint>,
    /// Metric history (for potential graphing, limited size).
    pub history: HashMap<String, Vec<TelemetryPoint>>,
    /// Maximum history size per metric.
    pub max_history: usize,
}

impl DeviceDetailState {
    /// Create a new device detail state.
    pub fn new(device_id: DeviceId) -> Self {
        Self {
            device_id,
            metrics: HashMap::new(),
            history: HashMap::new(),
            max_history: 100,
        }
    }

    /// Update with a new telemetry point.
    pub fn update(&mut self, point: TelemetryPoint) {
        let metric_name = point.metric.clone();

        // Update current value
        self.metrics.insert(metric_name.clone(), point.clone());

        // Update history
        let history = self.history.entry(metric_name).or_insert_with(Vec::new);
        history.push(point);

        // Trim history if needed
        if history.len() > self.max_history {
            history.remove(0);
        }
    }

    /// Get metrics sorted by name.
    pub fn sorted_metrics(&self) -> Vec<(&String, &TelemetryPoint)> {
        let mut metrics: Vec<_> = self.metrics.iter().collect();
        metrics.sort_by(|a, b| a.0.cmp(b.0));
        metrics
    }
}

/// Render the device detail view.
pub fn device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let metrics = render_metrics_list(state);

    let content = column![header, rule::horizontal(1), metrics]
        .spacing(10)
        .padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and device info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(text("<- Back").size(14))
        .on_press(Message::ClearSelection)
        .style(iced::widget::button::secondary);

    let protocol_badge = text(format!("[{}]", state.device_id.protocol)).size(14);
    let device_name = text(&state.device_id.source).size(24);
    let metric_count = text(format!("{} metrics", state.metrics.len())).size(14);

    row![back_button, protocol_badge, device_name, metric_count]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render the list of all metrics.
fn render_metrics_list(state: &DeviceDetailState) -> Element<'_, Message> {
    let metrics = state.sorted_metrics();

    if metrics.is_empty() {
        return container(text("No metrics received yet...").size(16))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    }

    let mut metric_list = Column::new().spacing(8);

    for (name, point) in metrics {
        metric_list = metric_list.push(render_metric_row(name, point, state));
    }

    scrollable(metric_list)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render a single metric row.
fn render_metric_row(
    name: &str,
    point: &TelemetryPoint,
    state: &DeviceDetailState,
) -> Element<'static, Message> {
    let metric_name = text(name.to_string()).size(14);
    let value_text = format_value(&point.value);
    let value = text(value_text.clone()).size(14);

    // Type indicator
    let type_indicator = text(format!("({})", value_type_name(&point.value)))
        .size(11)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        });

    // Timestamp (relative or absolute)
    let timestamp = format_timestamp(point.timestamp);
    let time_text = text(timestamp)
        .size(11)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        });

    // History indicator (if we have history)
    let history_indicator = if let Some(history) = state.history.get(name) {
        if history.len() > 1 {
            let trend = calculate_trend(history);
            text(trend).size(14)
        } else {
            text("").size(14)
        }
    } else {
        text("").size(14)
    };

    // Labels (if any)
    let mut row_content = row![
        metric_name,
        value,
        type_indicator,
        history_indicator,
        time_text
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    if !point.labels.is_empty() {
        let labels_str: String = point
            .labels
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        let labels_text = text(format!("[{}]", labels_str))
            .size(10)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.4, 0.4, 0.6)),
            });
        row_content = row_content.push(labels_text);
    }

    container(row_content)
        .width(Length::Fill)
        .padding(8)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgb(
                0.15, 0.15, 0.15,
            ))),
            border: iced::Border {
                color: iced::Color::from_rgb(0.3, 0.3, 0.3),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Format a telemetry value for display.
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
        TelemetryValue::Text(s) => {
            if s.len() > 50 {
                format!("{}...", &s[..47])
            } else {
                s.clone()
            }
        }
        TelemetryValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        TelemetryValue::Binary(data) => format!("<{} bytes>", data.len()),
    }
}

/// Get the type name for a telemetry value.
fn value_type_name(value: &TelemetryValue) -> &'static str {
    match value {
        TelemetryValue::Counter(_) => "counter",
        TelemetryValue::Gauge(_) => "gauge",
        TelemetryValue::Text(_) => "text",
        TelemetryValue::Boolean(_) => "bool",
        TelemetryValue::Binary(_) => "binary",
    }
}

/// Format a timestamp for display.
fn format_timestamp(timestamp_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let diff_ms = now - timestamp_ms;

    if diff_ms < 1000 {
        "just now".to_string()
    } else if diff_ms < 60_000 {
        format!("{}s ago", diff_ms / 1000)
    } else if diff_ms < 3_600_000 {
        format!("{}m ago", diff_ms / 60_000)
    } else {
        format!("{}h ago", diff_ms / 3_600_000)
    }
}

/// Calculate a simple trend indicator for numeric values.
fn calculate_trend(history: &[TelemetryPoint]) -> String {
    if history.len() < 2 {
        return String::new();
    }

    let last = &history[history.len() - 1];
    let prev = &history[history.len() - 2];

    let (last_val, prev_val) = match (&last.value, &prev.value) {
        (TelemetryValue::Counter(a), TelemetryValue::Counter(b)) => (*a as f64, *b as f64),
        (TelemetryValue::Gauge(a), TelemetryValue::Gauge(b)) => (*a, *b),
        _ => return String::new(),
    };

    if last_val > prev_val {
        "\u{2191}".to_string() // Up arrow
    } else if last_val < prev_val {
        "\u{2193}".to_string() // Down arrow
    } else {
        "\u{2192}".to_string() // Right arrow (stable)
    }
}
