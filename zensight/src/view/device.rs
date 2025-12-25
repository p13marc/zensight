//! Device detail view showing all metrics for a selected device.

use std::collections::HashMap;

use iced::widget::{Column, Row, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::{TelemetryPoint, TelemetryValue};

use crate::message::{DeviceId, Message};
use crate::view::chart::{ChartState, DataPoint, TimeWindow, chart_view};

/// State for the device detail view.
#[derive(Debug)]
pub struct DeviceDetailState {
    /// The device being viewed.
    pub device_id: DeviceId,
    /// All metrics for this device (metric name -> telemetry point).
    pub metrics: HashMap<String, TelemetryPoint>,
    /// Metric history (for graphing).
    pub history: HashMap<String, Vec<TelemetryPoint>>,
    /// Maximum history size per metric.
    pub max_history: usize,
    /// Currently selected metric for the chart (if any).
    pub selected_metric: Option<String>,
    /// Chart state for the selected metric.
    pub chart: ChartState,
}

impl DeviceDetailState {
    /// Create a new device detail state.
    pub fn new(device_id: DeviceId) -> Self {
        Self {
            device_id: device_id.clone(),
            metrics: HashMap::new(),
            history: HashMap::new(),
            max_history: 500,
            selected_metric: None,
            chart: ChartState::new(format!("{}", device_id)),
        }
    }

    /// Update with a new telemetry point.
    pub fn update(&mut self, point: TelemetryPoint) {
        let metric_name = point.metric.clone();

        // Update current value
        self.metrics.insert(metric_name.clone(), point.clone());

        // Update history
        let history = self.history.entry(metric_name.clone()).or_default();
        history.push(point.clone());

        // Trim history if needed
        if history.len() > self.max_history {
            history.remove(0);
        }

        // Update chart if this metric is selected
        if self.selected_metric.as_ref() == Some(&metric_name) {
            if let Some(data_point) = DataPoint::from_telemetry(point.timestamp, &point.value) {
                self.chart.push(data_point);
            }
        }
    }

    /// Select a metric for charting.
    pub fn select_metric(&mut self, metric_name: String) {
        self.selected_metric = Some(metric_name.clone());
        self.chart = ChartState::new(&metric_name);

        // Populate chart with existing history
        if let Some(history) = self.history.get(&metric_name) {
            let data_points: Vec<DataPoint> = history
                .iter()
                .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value))
                .collect();
            self.chart.set_data(data_points);
        }
    }

    /// Clear the chart selection.
    pub fn clear_chart_selection(&mut self) {
        self.selected_metric = None;
    }

    /// Set the chart time window.
    pub fn set_time_window(&mut self, window: TimeWindow) {
        self.chart.set_time_window(window);
    }

    /// Update the chart time (call on tick).
    pub fn update_chart_time(&mut self) {
        self.chart.update_time();
    }

    /// Get metrics sorted by name.
    pub fn sorted_metrics(&self) -> Vec<(&String, &TelemetryPoint)> {
        let mut metrics: Vec<_> = self.metrics.iter().collect();
        metrics.sort_by(|a, b| a.0.cmp(b.0));
        metrics
    }

    /// Check if a metric is numeric (can be charted).
    pub fn is_metric_chartable(&self, metric_name: &str) -> bool {
        if let Some(point) = self.metrics.get(metric_name) {
            matches!(
                point.value,
                TelemetryValue::Counter(_) | TelemetryValue::Gauge(_)
            )
        } else {
            false
        }
    }
}

/// Render the device detail view.
pub fn device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);

    // If a metric is selected for charting, show the chart
    let chart_section = if state.selected_metric.is_some() {
        render_chart_section(state)
    } else {
        column![].into()
    };

    let metrics = render_metrics_list(state);

    let content = column![header, rule::horizontal(1), chart_section, metrics]
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

/// Render the chart section.
fn render_chart_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let metric_name = state.selected_metric.as_ref().unwrap();

    // Chart header with close button and time window buttons
    let close_button = button(text("X").size(12))
        .on_press(Message::ClearChartSelection)
        .style(iced::widget::button::secondary);

    let chart_title = text(format!("Chart: {}", metric_name)).size(14);

    // Time window buttons
    let time_buttons: Element<'_, Message> = Row::with_children(
        TimeWindow::all()
            .iter()
            .map(|&window| {
                let is_selected = state.chart.time_window() == window;
                let btn = button(text(window.label()).size(11))
                    .on_press(Message::SetChartTimeWindow(window))
                    .style(if is_selected {
                        iced::widget::button::primary
                    } else {
                        iced::widget::button::secondary
                    });
                btn.into()
            })
            .collect::<Vec<_>>(),
    )
    .spacing(5)
    .into();

    let header = row![chart_title, time_buttons, close_button]
        .spacing(15)
        .align_y(Alignment::Center);

    // The chart canvas
    let chart: Element<'_, Message> = chart_view(&state.chart);

    // Stats row
    let stats = state.chart.stats();
    let stats_row = row![
        text(format!(
            "Current: {}",
            stats.current.map_or("-".to_string(), |v| format_value(v))
        ))
        .size(12),
        text(format!("Min: {}", format_value(stats.min))).size(12),
        text(format!("Max: {}", format_value(stats.max))).size(12),
        text(format!("Avg: {}", format_value(stats.avg))).size(12),
        text(format!("Points: {}", stats.count)).size(12),
    ]
    .spacing(20);

    let chart_container = container(column![header, chart, stats_row].spacing(10).padding(10))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgb(
                0.12, 0.12, 0.14,
            ))),
            border: iced::Border {
                color: iced::Color::from_rgb(0.25, 0.25, 0.3),
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        })
        .width(Length::Fill);

    column![chart_container, rule::horizontal(1)]
        .spacing(10)
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
    let value_text = format_value_display(&point.value);
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

    // Chart button (only for numeric types)
    let chart_button: Element<'static, Message> = if state.is_metric_chartable(name) {
        let is_selected = state.selected_metric.as_ref() == Some(&name.to_string());
        let btn_text = if is_selected { "Charted" } else { "Chart" };
        button(text(btn_text).size(11))
            .on_press(Message::SelectMetricForChart(name.to_string()))
            .style(if is_selected {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            })
            .into()
    } else {
        text("").into()
    };

    // Labels (if any)
    let mut row_content = row![
        metric_name,
        value,
        type_indicator,
        history_indicator,
        chart_button,
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
fn format_value_display(value: &TelemetryValue) -> String {
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

/// Format a numeric value for stats display.
fn format_value(value: f64) -> String {
    if value.abs() >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value.abs() >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        format!("{:.2}", value)
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
