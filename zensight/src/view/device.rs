//! Device detail view showing all metrics for a selected device.

use std::collections::{HashMap, VecDeque};

use iced::widget::{
    Row, column, container, row, rule, scrollable, table, text, text_input, tooltip,
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

/// Threshold for marking individual metrics as stale (60 seconds in ms).
const METRIC_STALE_THRESHOLD_MS: i64 = 60_000;

/// A row in the metrics table, containing pre-formatted data for display.
/// This struct is Clone so it can be used with the table widget.
#[derive(Debug, Clone)]
struct MetricTableRow {
    /// Metric name.
    name: String,
    /// Formatted value for display.
    value: String,
    /// Full value (if truncated).
    full_value: Option<String>,
    /// Type name (Counter, Gauge, Text, etc.).
    type_name: String,
    /// Formatted timestamp.
    timestamp: String,
    /// Whether this metric is chartable (numeric).
    is_chartable: bool,
    /// Whether this metric is currently in the chart.
    is_in_chart: bool,
    /// Trend indicator: "up", "down", "stable", or empty.
    trend: String,
    /// Whether this metric is stale (not updated recently).
    is_stale: bool,
}

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
    pub history: HashMap<String, VecDeque<TelemetryPoint>>,
    /// Pre-restart history seeded from the local tiered store (#22), keyed by
    /// metric name. Merged ahead of live `history` when a chart is opened so a
    /// device view opens pre-populated with trends that survived restart.
    pub seeded_history: HashMap<String, Vec<crate::store::Sample>>,
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
    /// On-demand netlink detail tables (sockets/routes/neighbors), fetched lazily
    /// from the sensor's query channel when the user drills in.
    pub netlink_detail: crate::view::specialized::netlink_detail::NetlinkDetailState,
    /// On-demand netring flow detail, fetched lazily from `@/query/flows`.
    pub netring_detail: crate::view::specialized::netring_detail::NetringDetailState,
    /// Whether the chart panel is expanded to a taller height (#36).
    pub chart_expanded: bool,
    /// Text-input buffer for the custom relative window in minutes (#36).
    pub chart_custom_input: String,
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
            seeded_history: HashMap::new(),
            max_history,
            selected_metric: None,
            chart: ChartState::new(format!("{}", device_id)),
            metric_filter: String::new(),
            pending_filter: String::new(),
            pending_filter_time: 0,
            netlink_detail: Default::default(),
            netring_detail: Default::default(),
            chart_expanded: false,
            chart_custom_input: String::new(),
        }
    }

    /// Toggle the chart panel between default and expanded height (#36).
    pub fn toggle_chart_expand(&mut self) {
        self.chart_expanded = !self.chart_expanded;
    }

    /// Apply a custom relative window from the text input (#36). Empty input or
    /// an unparseable value clears the custom window.
    pub fn set_chart_custom_minutes(&mut self, input: String) {
        self.chart_custom_input = input;
        match self.chart_custom_input.trim().parse::<f64>() {
            Ok(minutes) => self.chart.set_custom_duration_minutes(minutes),
            Err(_) => self.chart.set_custom_duration_minutes(0.0),
        }
    }

    /// Update the max history setting.
    pub fn set_max_history(&mut self, max_history: usize) {
        self.max_history = max_history;
        // Trim existing history if needed
        for history in self.history.values_mut() {
            while history.len() > max_history {
                history.pop_front();
            }
        }
    }

    /// Update with a new telemetry point.
    pub fn update(&mut self, point: TelemetryPoint) {
        let metric_name = point.metric.clone();

        // Derive the chart data point up front (cheap: timestamp + value) so we
        // can move `point` into history below without re-deriving (#40).
        let data_point = DataPoint::from_telemetry(point.timestamp, &point.value);

        // Update current value (one clone — the snapshot map needs its own copy).
        self.metrics.insert(metric_name.clone(), point.clone());

        // Update the chart while we still hold `metric_name`.
        if let Some(dp) = data_point {
            // Single-series mode.
            if self.selected_metric.as_deref() == Some(metric_name.as_str()) {
                self.chart.push(dp.clone());
            }
            // Comparison mode (multi-series).
            if self.chart.has_series(&metric_name) {
                self.chart.push_to_series(&metric_name, dp);
            }
        }

        // Update history — move the original `point` in (its last use, no clone).
        let history = self.history.entry(metric_name).or_default();
        history.push_back(point);

        // Trim history if needed.
        if history.len() > self.max_history {
            history.pop_front();
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

        // Populate chart with stored history (pre-restart) + live history.
        let data_points = self.chart_points_for(&metric_name);
        if !data_points.is_empty() {
            self.chart.set_data(data_points);
        }
    }

    /// Build chart points for a metric, merging restart-survived store samples
    /// (older) ahead of the in-memory live history (newer), deduplicated by
    /// timestamp so the live point wins where they overlap. #22.
    fn chart_points_for(&self, metric_name: &str) -> Vec<DataPoint> {
        let mut points: Vec<DataPoint> = Vec::new();
        // Earliest live timestamp — store samples at/after it are superseded by live.
        let live_start = self
            .history
            .get(metric_name)
            .and_then(|h| h.front())
            .map(|p| p.timestamp);
        if let Some(seeded) = self.seeded_history.get(metric_name) {
            for s in seeded {
                if live_start.is_none_or(|start| s.ts < start) {
                    points.push(DataPoint::new(s.ts, s.value));
                }
            }
        }
        if let Some(history) = self.history.get(metric_name) {
            points.extend(
                history
                    .iter()
                    .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value)),
            );
        }
        points
    }

    /// Seed restart-survived history loaded from the store (#22). Stored per
    /// metric; merged into a chart when that metric is selected.
    pub fn seed_history(&mut self, series: Vec<(String, Vec<crate::store::Sample>)>) {
        for (metric, samples) in series {
            if samples.is_empty() {
                continue;
            }
            self.seeded_history.insert(metric.clone(), samples);
            // If this metric's chart is already open, refresh it with the seed.
            if self.selected_metric.as_deref() == Some(metric.as_str()) {
                let points = self.chart_points_for(&metric);
                self.chart.set_data(points);
            }
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

    /// Export the full per-metric **time series** to CSV (#37) — every point the
    /// view holds, not just the latest snapshot. One row per (metric, sample),
    /// sorted by metric then timestamp, so the trend on screen is exportable.
    pub fn export_history_to_csv(&self) -> String {
        let mut csv = String::new();
        csv.push_str("timestamp,protocol,source,metric,value,type\n");

        let mut names: Vec<&String> = self.history.keys().collect();
        names.sort();
        for name in names {
            let Some(history) = self.history.get(name) else {
                continue;
            };
            for point in history.iter() {
                let value_str = format_value_for_export(&point.value);
                let type_str = value_type_name(&point.value);
                csv.push_str(&format!(
                    "{},{},{},{},{},{}\n",
                    point.timestamp,
                    point.protocol,
                    escape_csv(&point.source),
                    escape_csv(&point.metric),
                    escape_csv(&value_str),
                    type_str
                ));
            }
        }
        csv
    }

    /// Export the full per-metric time series to JSON (#37): a map of metric name
    /// to its ordered list of telemetry points.
    pub fn export_history_to_json(&self) -> String {
        let mut ordered: std::collections::BTreeMap<&String, Vec<&TelemetryPoint>> =
            std::collections::BTreeMap::new();
        for (name, history) in &self.history {
            ordered.insert(name, history.iter().collect());
        }
        serde_json::to_string_pretty(&ordered).unwrap_or_else(|_| "{}".to_string())
    }

    /// Whether there is any time-series history to export (#37).
    pub fn has_history(&self) -> bool {
        self.history.values().any(|h| !h.is_empty())
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
        // Wrap it with the shared nav header so every device screen has a Back
        // button + consistent chrome (specialized views don't render their own).
        return with_device_nav(state, specialized_view);
    }

    // Fall back to generic view
    generic_device_view(state)
}

/// Wrap a specialized device view with the shared navigation header (Back +
/// device identity + export buttons). Specialized views render only their domain
/// content, so this guarantees consistent navigation chrome across every device.
fn with_device_nav<'a>(
    state: &'a DeviceDetailState,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    column![
        container(render_header(state)).padding([12, 20]),
        rule::horizontal(1),
        content,
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Render the device detail view with syslog filter state.
///
/// This is used when the device is a syslog source and we need to pass
/// the filter state for the specialized view.
pub fn device_view_with_syslog_filter<'a>(
    state: &'a DeviceDetailState,
    syslog_filter: &'a specialized::SyslogFilterState,
) -> Element<'a, Message> {
    use zensight_common::Protocol;

    // For syslog devices, use the specialized view with filter state
    if state.device_id.protocol == Protocol::Syslog {
        return with_device_nav(state, specialized::syslog_view(state, syslog_filter));
    }

    // For other protocols, use the standard device view
    device_view(state)
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

    // #35: step through the current filtered device set without returning to the
    // dashboard between hops.
    let prev_button = button(text("‹").size(16))
        .on_press(Message::SelectAdjacentDevice { forward: false })
        .padding([4, 10])
        .style(iced::widget::button::secondary);
    let next_button = button(text("›").size(16))
        .on_press(Message::SelectAdjacentDevice { forward: true })
        .padding([4, 10])
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
        prev_button,
        next_button,
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

    // Custom relative window input (#36): "last N minutes", overrides presets.
    let custom_input = text_input("min", &state.chart_custom_input)
        .on_input(Message::SetChartCustomMinutes)
        .width(Length::Fixed(64.0))
        .size(11);
    let custom_window = row![text("Custom:").size(11), custom_input]
        .spacing(4)
        .align_y(Alignment::Center);

    // Expand/collapse the chart height (#36): no more fixed 200px sliver.
    let expand_button = button(
        text(if state.chart_expanded {
            "Collapse"
        } else {
            "Expand"
        })
        .size(11),
    )
    .on_press(Message::ToggleChartExpand)
    .style(iced::widget::button::secondary);

    let header = row![
        chart_title,
        time_buttons,
        custom_window,
        expand_button,
        close_button
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    // Inline per-series legend with toggle/remove (#36) — manage a comparison
    // without switching back to the metrics table.
    let legend: Element<'_, Message> = if state.is_comparison_mode() {
        let mut legend_row = Row::new().spacing(12).align_y(Alignment::Center);
        for series in state.chart.series() {
            let (r, g, b) = series.color;
            let swatch_color = iced::Color::from_rgb(r, g, b);
            let swatch = container(text(""))
                .width(10)
                .height(10)
                .style(move |_t: &Theme| container::Style {
                    background: Some(iced::Background::Color(swatch_color)),
                    border: iced::Border::default().rounded(2.0),
                    ..Default::default()
                });
            let name = series.name.clone();
            let toggle = button(text(if series.visible { "shown" } else { "hidden" }).size(10))
                .on_press(Message::ToggleMetricVisibility(name.clone()))
                .style(iced::widget::button::text);
            let remove = button(text("×").size(12))
                .on_press(Message::RemoveMetricFromChart(name.clone()))
                .style(iced::widget::button::text);
            legend_row = legend_row.push(
                row![swatch, text(name).size(11), toggle, remove]
                    .spacing(4)
                    .align_y(Alignment::Center),
            );
        }
        legend_row.into()
    } else {
        column![].into()
    };

    // Default to a usable height; expand for detailed inspection. The custom
    // window doesn't change height — only the visible time range.
    let chart_height = if state.chart_expanded { 520.0 } else { 320.0 };
    let chart: Element<'_, Message> = chart_view(&state.chart, chart_height);

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

    let chart_container = container(
        column![header, legend, chart, stats_row]
            .spacing(10)
            .padding(10),
    )
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

/// Convert metrics to table rows.
fn build_metric_table_rows(state: &DeviceDetailState) -> Vec<MetricTableRow> {
    state
        .sorted_metrics()
        .into_iter()
        .map(|(name, point)| {
            let (value, full_value) = format_value_display_with_full(&point.value);
            let trend = if let Some(history) = state.history.get(name) {
                if history.len() > 1 {
                    compute_trend(history)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let is_stale = (current_timestamp() - point.timestamp) > METRIC_STALE_THRESHOLD_MS;

            MetricTableRow {
                name: name.to_string(),
                value,
                full_value,
                type_name: value_type_name(&point.value).to_string(),
                timestamp: format_timestamp(point.timestamp),
                is_chartable: state.is_metric_chartable(name),
                is_in_chart: state.is_metric_in_chart(name),
                trend,
                is_stale,
            }
        })
        .collect()
}

/// Compute trend direction from history. Panic-proof: reads the last two points
/// via a reverse iterator, so any history length (0, 1, …) is handled by the
/// `else` branch rather than by indexing.
fn compute_trend(history: &VecDeque<TelemetryPoint>) -> String {
    let mut recent = history.iter().rev();
    let (Some(last), Some(prev)) = (recent.next(), recent.next()) else {
        return String::new();
    };

    match (&last.value, &prev.value) {
        (TelemetryValue::Gauge(a), TelemetryValue::Gauge(b)) => {
            if a > b {
                "↑".to_string()
            } else if a < b {
                "↓".to_string()
            } else {
                "→".to_string()
            }
        }
        (TelemetryValue::Counter(a), TelemetryValue::Counter(b)) => {
            if a > b {
                "↑".to_string()
            } else if a < b {
                "↓".to_string()
            } else {
                "→".to_string()
            }
        }
        _ => String::new(),
    }
}

/// Render the list of all metrics using a table widget.
fn render_metrics_list(state: &DeviceDetailState) -> Element<'_, Message> {
    let total_count = state.total_metric_count();
    let table_rows = build_metric_table_rows(state);
    let filtered_count = table_rows.len();

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

    if table_rows.is_empty() {
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

    // Build table columns
    // Note: closures consume MetricTableRow, so we clone strings for owned values
    let name_column = table::column(
        text("Metric").size(12),
        |row: MetricTableRow| -> Element<'_, Message> {
            let name = row.name.clone();
            let name_display = row.name;
            // Make the name clickable to select for chart
            if row.is_chartable {
                button(text(name_display).size(12))
                    .on_press(Message::SelectMetricForChart(name))
                    .style(if row.is_in_chart {
                        iced::widget::button::primary
                    } else {
                        iced::widget::button::text
                    })
                    .padding(0)
                    .into()
            } else {
                text(name_display).size(12).into()
            }
        },
    )
    .width(Length::FillPortion(3));

    let value_column = table::column(
        text("Value").size(12),
        |row: MetricTableRow| -> Element<'_, Message> {
            let value = row.value;
            let is_stale = row.is_stale;
            let value_widget = text(value).size(12).style(move |theme: &Theme| {
                if is_stale {
                    text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    }
                } else {
                    text::Style::default()
                }
            });
            if let Some(full) = row.full_value {
                tooltip(
                    value_widget,
                    container(text(full).size(11))
                        .padding(6)
                        .max_width(400.0)
                        .style(container::rounded_box),
                    tooltip::Position::Bottom,
                )
                .into()
            } else {
                value_widget.into()
            }
        },
    )
    .width(Length::FillPortion(2));

    let type_column = table::column(
        text("Type").size(12),
        |row: MetricTableRow| -> Element<'_, Message> {
            let type_name = row.type_name;
            text(type_name)
                .size(11)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).text_dimmed()),
                })
                .into()
        },
    )
    .width(80);

    let trend_column = table::column(
        text("Trend").size(12),
        |row: MetricTableRow| -> Element<'_, Message> {
            let trend = row.trend;
            let color = match trend.as_str() {
                "↑" => iced::Color::from_rgb(0.2, 0.8, 0.2),
                "↓" => iced::Color::from_rgb(0.9, 0.2, 0.2),
                _ => iced::Color::from_rgb(0.5, 0.5, 0.5),
            };
            text(trend)
                .size(14)
                .style(move |_: &Theme| text::Style { color: Some(color) })
                .into()
        },
    )
    .width(50);

    let time_column = table::column(
        text("Updated").size(12),
        |row: MetricTableRow| -> Element<'_, Message> {
            let timestamp = row.timestamp;
            let is_stale = row.is_stale;
            if is_stale {
                row![
                    text(timestamp).size(11).style(|theme: &Theme| text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    }),
                    text("stale").size(9).style(|_theme: &Theme| text::Style {
                        color: Some(iced::Color::from_rgb(0.7, 0.4, 0.1)),
                    })
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            } else {
                text(timestamp)
                    .size(11)
                    .style(|theme: &Theme| text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    })
                    .into()
            }
        },
    )
    .width(120);

    let actions_column = table::column(
        text("").size(12),
        |row: MetricTableRow| -> Element<'_, Message> {
            if row.is_chartable {
                let metric_name = row.name.clone();
                button(text(if row.is_in_chart { "−" } else { "+" }).size(11))
                    .on_press(if row.is_in_chart {
                        Message::RemoveMetricFromChart(metric_name)
                    } else {
                        Message::AddMetricToChart(metric_name)
                    })
                    .style(if row.is_in_chart {
                        iced::widget::button::danger
                    } else {
                        iced::widget::button::secondary
                    })
                    .padding([2, 8])
                    .into()
            } else {
                text("").into()
            }
        },
    )
    .width(40);

    let metrics_table = table(
        [
            name_column,
            value_column,
            type_column,
            trend_column,
            time_column,
            actions_column,
        ],
        table_rows,
    )
    .padding(6)
    .padding_y(4);

    column![
        search_row,
        scrollable(metrics_table)
            .width(Length::Fill)
            .height(Length::Fill)
    ]
    .spacing(10)
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
    fn test_history_export_is_time_series_not_snapshot() {
        let device_id = DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        };
        let mut state = DeviceDetailState::new(device_id);

        // Three samples of the same metric over time.
        for (ts, v) in [(1000, 1.0), (2000, 2.0), (3000, 3.0)] {
            let mut p = make_test_point("cpu/usage");
            p.timestamp = ts;
            p.value = TelemetryValue::Gauge(v);
            state.update(p);
        }

        assert!(state.has_history());
        let csv = state.export_history_to_csv();
        // header + 3 data rows (the trend), not a single snapshot row.
        let rows = csv.lines().count();
        assert_eq!(rows, 4, "expected header + 3 samples, got:\n{csv}");
        assert!(csv.contains("1000,"));
        assert!(csv.contains("3000,"));

        // The latest-snapshot export keeps only one row per metric.
        let snapshot = state.export_to_csv();
        assert_eq!(snapshot.lines().count(), 2);
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

    fn point_at(metric: &str, value: f64, ts: i64) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: ts,
            source: "test".to_string(),
            protocol: Protocol::Snmp,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(value),
            labels: std::collections::HashMap::new(),
        }
    }

    fn device() -> DeviceDetailState {
        DeviceDetailState::new(DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        })
    }

    #[test]
    fn seeded_history_prepended_to_chart() {
        let mut state = device();
        // Live history starts at ts=5000.
        state.update(point_at("cpu", 50.0, 5_000));
        state.update(point_at("cpu", 55.0, 6_000));
        // Pre-restart samples from the store, older than live.
        state.seed_history(vec![(
            "cpu".to_string(),
            vec![
                crate::store::Sample {
                    ts: 1_000,
                    value: 10.0,
                },
                crate::store::Sample {
                    ts: 2_000,
                    value: 20.0,
                },
            ],
        )]);
        state.select_metric("cpu".to_string());
        let pts = state.chart.data();
        // 2 seeded + 2 live, oldest first.
        assert_eq!(pts.len(), 4);
        assert_eq!(pts[0].timestamp, 1_000);
        assert_eq!(pts[0].value, 10.0);
        assert_eq!(pts[3].timestamp, 6_000);
    }

    #[test]
    fn seeded_history_overlap_excluded_by_live() {
        let mut state = device();
        state.update(point_at("cpu", 50.0, 2_000));
        // Seed includes a sample at the same ts as live (2000) and one after — both
        // are at/after the live start so the live point wins (no duplicate at 2000).
        state.seed_history(vec![(
            "cpu".to_string(),
            vec![
                crate::store::Sample {
                    ts: 1_000,
                    value: 10.0,
                },
                crate::store::Sample {
                    ts: 2_000,
                    value: 99.0,
                },
                crate::store::Sample {
                    ts: 3_000,
                    value: 99.0,
                },
            ],
        )]);
        state.select_metric("cpu".to_string());
        let pts = state.chart.data();
        // Only the seeded ts=1000 (before live start) + the single live point.
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].timestamp, 1_000);
        assert_eq!(pts[1].timestamp, 2_000);
        assert_eq!(pts[1].value, 50.0);
    }

    #[test]
    fn seed_history_refreshes_open_chart() {
        let mut state = device();
        state.update(point_at("cpu", 50.0, 5_000));
        state.select_metric("cpu".to_string());
        assert_eq!(state.chart.data().len(), 1);
        // Seeding after the chart is open refreshes it in place.
        state.seed_history(vec![(
            "cpu".to_string(),
            vec![crate::store::Sample {
                ts: 1_000,
                value: 10.0,
            }],
        )]);
        assert_eq!(state.chart.data().len(), 2);
    }
}
