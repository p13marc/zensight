//! Device detail view showing all metrics for a selected device.

use std::collections::HashMap;

use iced::widget::{
    Column, Row, column, container, row, rule, scrollable, text, text_input, tooltip,
};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{TelemetryPoint, TelemetryValue};

use crate::app::DEVICE_SEARCH_ID;
use crate::message::{DeviceId, Message};
use crate::view::chart::{ChartState, DataPoint, TimeWindow, chart_view};
use crate::view::formatting::{format_timestamp, format_value};
use crate::view::icons::{self, IconSize};
use crate::view::specialized;

/// Debounce delay for metric search input in milliseconds.
const SEARCH_DEBOUNCE_MS: i64 = 300;

/// Get the current timestamp in milliseconds.
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

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
    /// Search filter for metrics (applied after debounce).
    pub metric_filter: String,
    /// Pending search filter (user input).
    pub pending_filter: String,
    /// Timestamp when pending filter was last updated.
    pub pending_filter_time: i64,
}

impl DeviceDetailState {
    /// Create a new device detail state.
    pub fn new(device_id: DeviceId) -> Self {
        Self::with_max_history(device_id, 500)
    }

    /// Create a new device detail state with configurable max history.
    pub fn with_max_history(device_id: DeviceId, max_history: usize) -> Self {
        Self {
            device_id: device_id.clone(),
            metrics: HashMap::new(),
            history: HashMap::new(),
            max_history,
            selected_metric: None,
            chart: ChartState::new(format!("{}", device_id)),
            metric_filter: String::new(),
            pending_filter: String::new(),
            pending_filter_time: 0,
        }
    }

    /// Update the max history setting.
    pub fn set_max_history(&mut self, max_history: usize) {
        self.max_history = max_history;
        // Trim existing history if needed
        for history in self.history.values_mut() {
            while history.len() > max_history {
                history.remove(0);
            }
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

        // Update chart if this metric is selected (single-series mode)
        if self.selected_metric.as_ref() == Some(&metric_name)
            && let Some(data_point) = DataPoint::from_telemetry(point.timestamp, &point.value)
        {
            self.chart.push(data_point);
        }

        // Update chart if this metric is in comparison mode (multi-series)
        if self.chart.has_series(&metric_name)
            && let Some(data_point) = DataPoint::from_telemetry(point.timestamp, &point.value)
        {
            self.chart.push_to_series(&metric_name, data_point);
        }
    }

    /// Select a metric for charting (single-metric mode).
    pub fn select_metric(&mut self, metric_name: String) {
        // If already in multi-series mode with this metric, just switch to single mode
        if self.chart.is_multi_series() {
            self.chart.clear_series();
        }

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
        self.chart.clear_series();
    }

    /// Add a metric to the comparison chart (multi-series mode).
    pub fn add_metric_to_chart(&mut self, metric_name: String) {
        // Check if metric is chartable
        if !self.is_metric_chartable(&metric_name) {
            return;
        }

        // If this is the first metric being added in comparison mode,
        // set a generic title
        if !self.chart.is_multi_series() && self.selected_metric.is_none() {
            self.chart = ChartState::new("Metric Comparison");
        }

        // Clear single-series data when switching to multi-series
        if self.selected_metric.is_some() && !self.chart.is_multi_series() {
            // Convert current single metric to a series
            if let Some(ref current_metric) = self.selected_metric
                && let Some(history) = self.history.get(current_metric)
            {
                let data_points: Vec<DataPoint> = history
                    .iter()
                    .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value))
                    .collect();
                self.chart
                    .add_series_with_data(current_metric.clone(), data_points);
            }
            self.selected_metric = None;
            self.chart.set_data(Vec::new()); // Clear single-series data
        }

        // Add new series with historical data
        if let Some(history) = self.history.get(&metric_name) {
            let data_points: Vec<DataPoint> = history
                .iter()
                .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value))
                .collect();
            self.chart.add_series_with_data(&metric_name, data_points);
        } else {
            self.chart.add_series(&metric_name);
        }
    }

    /// Remove a metric from the comparison chart.
    pub fn remove_metric_from_chart(&mut self, metric_name: &str) {
        self.chart.remove_series(metric_name);

        // If only one series left, could switch back to single mode (optional)
        // For now, keep in multi-series mode even with one series
    }

    /// Toggle visibility of a metric in the comparison chart.
    pub fn toggle_metric_visibility(&mut self, metric_name: &str) {
        self.chart.toggle_series_visibility(metric_name);
    }

    /// Check if a metric is currently in the chart (single or multi-series).
    pub fn is_metric_in_chart(&self, metric_name: &str) -> bool {
        if self.chart.is_multi_series() {
            self.chart.has_series(metric_name)
        } else {
            self.selected_metric.as_ref() == Some(&metric_name.to_string())
        }
    }

    /// Check if in multi-series (comparison) mode.
    pub fn is_comparison_mode(&self) -> bool {
        self.chart.is_multi_series()
    }

    /// Get the number of metrics in comparison chart.
    pub fn comparison_count(&self) -> usize {
        self.chart.series_count()
    }

    /// Set the chart time window.
    pub fn set_time_window(&mut self, window: TimeWindow) {
        self.chart.set_time_window(window);
    }

    /// Zoom in on the chart.
    pub fn zoom_in(&mut self) {
        self.chart.zoom_in();
    }

    /// Zoom out on the chart.
    pub fn zoom_out(&mut self) {
        self.chart.zoom_out();
    }

    /// Reset chart zoom to 100%.
    pub fn reset_zoom(&mut self) {
        self.chart.reset_zoom();
    }

    /// Pan the chart left (back in time).
    pub fn pan_left(&mut self) {
        self.chart.pan_left();
    }

    /// Pan the chart right (forward in time).
    pub fn pan_right(&mut self) {
        self.chart.pan_right();
    }

    /// Reset chart pan to view current time.
    pub fn reset_pan(&mut self) {
        self.chart.reset_pan();
    }

    /// Start chart drag.
    pub fn start_drag(&mut self, x: f32) {
        self.chart.start_drag(x);
    }

    /// Update chart drag.
    pub fn update_drag(&mut self, x: f32, width: f32) {
        self.chart.update_drag(x, width);
    }

    /// End chart drag.
    pub fn end_drag(&mut self) {
        self.chart.end_drag();
    }

    /// Update the chart time and apply pending filter (call on tick).
    pub fn update_chart_time(&mut self) {
        self.chart.update_time();
        self.chart.update_zoom_feedback();
        self.chart.update_pan_feedback();
        self.apply_pending_filter();
    }

    /// Set the metric search filter (debounced).
    ///
    /// Updates the pending filter and timestamp. The actual filter
    /// is applied after the debounce delay via `apply_pending_filter`.
    pub fn set_metric_filter(&mut self, filter: String) {
        self.pending_filter = filter;
        self.pending_filter_time = current_timestamp();
    }

    /// Apply the pending filter if the debounce delay has elapsed.
    ///
    /// Returns `true` if the filter was applied (changed).
    pub fn apply_pending_filter(&mut self) -> bool {
        if self.pending_filter != self.metric_filter {
            let elapsed = current_timestamp() - self.pending_filter_time;
            if elapsed >= SEARCH_DEBOUNCE_MS {
                self.metric_filter = self.pending_filter.clone();
                return true;
            }
        }
        false
    }

    /// Get the current filter input (for display in the text input).
    pub fn filter_input(&self) -> &str {
        &self.pending_filter
    }

    /// Get metrics sorted by name, optionally filtered by the search string.
    pub fn sorted_metrics(&self) -> Vec<(&String, &TelemetryPoint)> {
        let filter_lower = self.metric_filter.to_lowercase();
        let mut metrics: Vec<_> = self
            .metrics
            .iter()
            .filter(|(name, _)| {
                if self.metric_filter.is_empty() {
                    true
                } else {
                    name.to_lowercase().contains(&filter_lower)
                }
            })
            .collect();
        metrics.sort_by(|a, b| a.0.cmp(b.0));
        metrics
    }

    /// Get the total metric count (unfiltered).
    pub fn total_metric_count(&self) -> usize {
        self.metrics.len()
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

    /// Export metrics to CSV format.
    pub fn export_to_csv(&self) -> String {
        let mut csv = String::new();

        // Header
        csv.push_str("timestamp,protocol,source,metric,value,type,labels\n");

        // Sort by metric name
        let mut metrics: Vec<_> = self.metrics.values().collect();
        metrics.sort_by(|a, b| a.metric.cmp(&b.metric));

        for point in metrics {
            let value_str = format_value_for_export(&point.value);
            let type_str = value_type_name(&point.value);
            let labels_str = point
                .labels
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(";");

            csv.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                point.timestamp,
                point.protocol,
                escape_csv(&point.source),
                escape_csv(&point.metric),
                escape_csv(&value_str),
                type_str,
                escape_csv(&labels_str)
            ));
        }

        csv
    }

    /// Export metrics to JSON format.
    pub fn export_to_json(&self) -> String {
        let mut metrics: Vec<_> = self.metrics.values().collect();
        metrics.sort_by(|a, b| a.metric.cmp(&b.metric));

        serde_json::to_string_pretty(&metrics).unwrap_or_else(|_| "[]".to_string())
    }
}

/// Escape a string for CSV (handle commas and quotes).
fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Format a value for export.
fn format_value_for_export(value: &TelemetryValue) -> String {
    match value {
        TelemetryValue::Counter(v) => v.to_string(),
        TelemetryValue::Gauge(v) => v.to_string(),
        TelemetryValue::Text(s) => s.clone(),
        TelemetryValue::Boolean(b) => b.to_string(),
        TelemetryValue::Binary(data) => format!("<{} bytes>", data.len()),
    }
}

/// Render the device detail view.
///
/// This function first tries to render a protocol-specific specialized view.
/// If no specialized view is available for the protocol, it falls back to
/// the generic device view.
pub fn device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    // Try to use a specialized view for this protocol
    if let Some(specialized_view) = specialized::specialized_view(state) {
        return specialized_view;
    }

    // Fall back to generic view
    generic_device_view(state)
}

/// Render the generic device detail view (fallback for protocols without specialized views).
pub fn generic_device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);

    // Show chart if a metric is selected (single) or in comparison mode (multi)
    let chart_section = if let Some(ref metric_name) = state.selected_metric {
        render_chart_section(state, Some(metric_name))
    } else if state.is_comparison_mode() {
        render_chart_section(state, None)
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
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let device_name = text(&state.device_id.source).size(24);
    let metric_count = text(format!("{} metrics", state.metrics.len())).size(14);

    let csv_button = button(
        row![icons::export(IconSize::Small), text("CSV").size(12)]
            .spacing(4)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ExportToCsv)
    .style(iced::widget::button::secondary);

    let json_button = button(
        row![icons::export(IconSize::Small), text("JSON").size(12)]
            .spacing(4)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ExportToJson)
    .style(iced::widget::button::secondary);

    row![
        back_button,
        protocol_icon,
        device_name,
        metric_count,
        csv_button,
        json_button
    ]
    .spacing(15)
    .align_y(Alignment::Center)
    .into()
}

/// Render the chart section.
fn render_chart_section<'a>(
    state: &'a DeviceDetailState,
    metric_name: Option<&'a str>,
) -> Element<'a, Message> {
    // Chart header with close button and time window buttons
    let close_button = button(icons::close(IconSize::Small))
        .on_press(Message::ClearChartSelection)
        .style(iced::widget::button::secondary);

    // Title depends on mode
    let title_text = if state.is_comparison_mode() {
        format!("Comparing {} metrics", state.comparison_count())
    } else if let Some(name) = metric_name {
        name.to_string()
    } else {
        "Chart".to_string()
    };

    let chart_title = row![icons::chart(IconSize::Medium), text(title_text).size(14)]
        .spacing(6)
        .align_y(Alignment::Center);

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
            stats.current.map_or("-".to_string(), format_value)
        ))
        .size(12),
        text(format!("Min: {}", format_value(stats.min))).size(12),
        text(format!("Max: {}", format_value(stats.max))).size(12),
        text(format!("Avg: {}", format_value(stats.avg))).size(12),
        text(format!("Points: {}", stats.count)).size(12),
    ]
    .spacing(20);

    let chart_container = container(column![header, chart, stats_row].spacing(10).padding(10))
        .style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.card_background())),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        })
        .width(Length::Fill);

    column![chart_container, rule::horizontal(1)]
        .spacing(10)
        .into()
}

/// Render the list of all metrics.
fn render_metrics_list(state: &DeviceDetailState) -> Element<'_, Message> {
    let total_count = state.total_metric_count();
    let metrics = state.sorted_metrics();
    let filtered_count = metrics.len();

    // Search filter input (with ID for keyboard focus)
    let search_input = text_input("Search metrics... (Ctrl+F)", state.filter_input())
        .id(DEVICE_SEARCH_ID.clone())
        .on_input(Message::SetMetricFilter)
        .size(14)
        .padding(8)
        .width(Length::Fixed(300.0));

    // Count indicator
    let count_text = if state.metric_filter.is_empty() {
        text(format!("{} metrics", total_count)).size(12)
    } else {
        text(format!("{} of {} metrics", filtered_count, total_count)).size(12)
    };

    let search_row = row![search_input, count_text]
        .spacing(15)
        .align_y(Alignment::Center);

    if total_count == 0 {
        return column![
            search_row,
            container(text("No metrics received yet...").size(16))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
        ]
        .spacing(10)
        .into();
    }

    if metrics.is_empty() {
        return column![
            search_row,
            container(text("No metrics match the filter").size(16))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
        ]
        .spacing(10)
        .into();
    }

    let mut metric_list = Column::new().spacing(8);

    for (name, point) in metrics {
        metric_list = metric_list.push(render_metric_row(name, point, state));
    }

    column![
        search_row,
        scrollable(metric_list)
            .width(Length::Fill)
            .height(Length::Fill)
    ]
    .spacing(10)
    .into()
}

/// Render a single metric row.
fn render_metric_row(
    name: &str,
    point: &TelemetryPoint,
    state: &DeviceDetailState,
) -> Element<'static, Message> {
    let metric_name = text(name.to_string()).size(14);
    let (value_text, full_value) = format_value_display_with_full(&point.value);
    let value_widget = text(value_text.clone()).size(14);

    // Wrap in tooltip if value was truncated
    let value: Element<'static, Message> = if let Some(ref full) = full_value {
        tooltip(
            value_widget,
            container(text(full.clone()).size(12))
                .padding(8)
                .max_width(400.0)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into()
    } else {
        value_widget.into()
    };

    // Type indicator
    let type_indicator = text(format!("({})", value_type_name(&point.value)))
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    // Timestamp (relative or absolute)
    let timestamp = format_timestamp(point.timestamp);
    let time_text = text(timestamp).size(11).style(|theme: &Theme| text::Style {
        color: Some(crate::view::theme::colors(theme).text_dimmed()),
    });

    // History indicator (if we have history)
    let history_indicator: Element<'static, Message> =
        if let Some(history) = state.history.get(name) {
            if history.len() > 1 {
                trend_icon(history)
            } else {
                text("").into()
            }
        } else {
            text("").into()
        };

    // Chart buttons (only for numeric types)
    let chart_buttons: Element<'static, Message> = if state.is_metric_chartable(name) {
        let is_in_chart = state.is_metric_in_chart(name);
        let is_comparison = state.is_comparison_mode();

        // Single chart button - switches to single-metric view
        let chart_btn = button(text("Chart").size(11))
            .on_press(Message::SelectMetricForChart(name.to_string()))
            .style(if !is_comparison && is_in_chart {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            });

        // Compare button - adds/removes from comparison view
        let compare_btn = if is_comparison && is_in_chart {
            // Already in comparison - show remove button
            button(text("−").size(11))
                .on_press(Message::RemoveMetricFromChart(name.to_string()))
                .style(iced::widget::button::danger)
        } else {
            // Not in comparison or not this metric - show add button
            button(text("+").size(11))
                .on_press(Message::AddMetricToChart(name.to_string()))
                .style(if is_comparison && is_in_chart {
                    iced::widget::button::primary
                } else {
                    iced::widget::button::secondary
                })
        };

        row![chart_btn, compare_btn].spacing(4).into()
    } else {
        text("").into()
    };

    // Labels (if any)
    let mut row_content = row![
        metric_name,
        value,
        type_indicator,
        history_indicator,
        chart_buttons,
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
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).secondary()),
            });
        row_content = row_content.push(labels_text);
    }

    container(row_content)
        .width(Length::Fill)
        .padding(8)
        .style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.row_background())),
                border: iced::Border {
                    color: colors.border_subtle(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// Format a telemetry value for display.
/// Returns (display_text, Option<full_text>) - full_text is Some if truncated.
fn format_value_display_with_full(value: &TelemetryValue) -> (String, Option<String>) {
    match value {
        TelemetryValue::Counter(v) => (format!("{}", v), None),
        TelemetryValue::Gauge(v) => {
            let display = if v.fract() == 0.0 {
                format!("{:.0}", v)
            } else {
                format!("{:.2}", v)
            };
            (display, None)
        }
        TelemetryValue::Text(s) => {
            if s.len() > 50 {
                (format!("{}...", &s[..47]), Some(s.clone()))
            } else {
                (s.clone(), None)
            }
        }
        TelemetryValue::Boolean(b) => (if *b { "true" } else { "false" }.to_string(), None),
        TelemetryValue::Binary(data) => (format!("<{} bytes>", data.len()), None),
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

/// Create a trend indicator icon for numeric values.
fn trend_icon(history: &[TelemetryPoint]) -> Element<'static, Message> {
    if history.len() < 2 {
        return text("").into();
    }

    let last = &history[history.len() - 1];
    let prev = &history[history.len() - 2];

    let (last_val, prev_val) = match (&last.value, &prev.value) {
        (TelemetryValue::Counter(a), TelemetryValue::Counter(b)) => (*a as f64, *b as f64),
        (TelemetryValue::Gauge(a), TelemetryValue::Gauge(b)) => (*a, *b),
        _ => return text("").into(),
    };

    if last_val > prev_val {
        icons::arrow_up(IconSize::Small)
    } else if last_val < prev_val {
        icons::arrow_down(IconSize::Small)
    } else {
        icons::arrow_stable(IconSize::Small)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::Protocol;

    fn make_test_point(metric: &str) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: 1000,
            source: "test".to_string(),
            protocol: Protocol::Snmp,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(42.0),
            labels: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_metric_filter_empty_returns_all() {
        let device_id = DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        };
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("cpu/usage"));
        state.update(make_test_point("memory/used"));
        state.update(make_test_point("disk/io"));

        // Empty filter returns all metrics
        assert_eq!(state.sorted_metrics().len(), 3);
        assert_eq!(state.total_metric_count(), 3);
    }

    #[test]
    fn test_metric_filter_substring_match() {
        let device_id = DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        };
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("cpu/usage"));
        state.update(make_test_point("cpu/temperature"));
        state.update(make_test_point("memory/used"));
        state.update(make_test_point("disk/io"));

        // Filter for "cpu" should return 2 metrics (after applying)
        state.set_metric_filter("cpu".to_string());
        // Directly set the applied filter for testing
        state.metric_filter = state.pending_filter.clone();

        let filtered = state.sorted_metrics();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|(name, _)| name.contains("cpu")));

        // Total count should still be 4
        assert_eq!(state.total_metric_count(), 4);
    }

    #[test]
    fn test_metric_filter_case_insensitive() {
        let device_id = DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        };
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("CPU/Usage"));
        state.update(make_test_point("memory/used"));

        // Filter should be case-insensitive (apply immediately for testing)
        state.set_metric_filter("cpu".to_string());
        state.metric_filter = state.pending_filter.clone();
        assert_eq!(state.sorted_metrics().len(), 1);

        state.set_metric_filter("CPU".to_string());
        state.metric_filter = state.pending_filter.clone();
        assert_eq!(state.sorted_metrics().len(), 1);

        state.set_metric_filter("CpU".to_string());
        state.metric_filter = state.pending_filter.clone();
        assert_eq!(state.sorted_metrics().len(), 1);
    }

    #[test]
    fn test_metric_filter_debounce() {
        let device_id = DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        };
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("cpu/usage"));
        state.update(make_test_point("memory/used"));

        // Set filter - should not apply immediately
        state.set_metric_filter("cpu".to_string());
        assert_eq!(state.filter_input(), "cpu");
        assert_eq!(state.metric_filter, ""); // Not applied yet

        // Simulate time passing by setting an old timestamp
        state.pending_filter_time = current_timestamp() - SEARCH_DEBOUNCE_MS - 1;

        // Now apply should work
        assert!(state.apply_pending_filter());
        assert_eq!(state.metric_filter, "cpu");
        assert_eq!(state.sorted_metrics().len(), 1);
    }
}
