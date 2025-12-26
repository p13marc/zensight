//! Time-series chart component using Iced canvas.

use iced::keyboard;
use iced::mouse;
use iced::widget::canvas::{
    self, Action, Cache, Canvas, Event, Frame, Geometry, Path, Stroke, Text,
};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use zensight_common::TelemetryValue;

use super::formatting::{format_time_offset, format_value};

/// Minimum zoom level (100% = no zoom).
pub const MIN_ZOOM: f32 = 1.0;
/// Maximum zoom level (10x zoom).
pub const MAX_ZOOM: f32 = 10.0;
/// Zoom step for keyboard (+/-) and scroll.
pub const ZOOM_STEP: f32 = 0.25;

/// A data point for the chart.
#[derive(Debug, Clone)]
pub struct DataPoint {
    /// Timestamp in milliseconds.
    pub timestamp: i64,
    /// Value (must be numeric).
    pub value: f64,
}

impl DataPoint {
    /// Create a new data point.
    pub fn new(timestamp: i64, value: f64) -> Self {
        Self { timestamp, value }
    }

    /// Try to create a data point from a telemetry value.
    pub fn from_telemetry(timestamp: i64, value: &TelemetryValue) -> Option<Self> {
        let numeric_value = match value {
            TelemetryValue::Counter(v) => *v as f64,
            TelemetryValue::Gauge(v) => *v,
            _ => return None,
        };
        Some(Self::new(timestamp, numeric_value))
    }
}

/// Predefined colors for chart series.
pub const SERIES_COLORS: &[(f32, f32, f32)] = &[
    (0.2, 0.7, 1.0), // Blue (primary)
    (1.0, 0.5, 0.2), // Orange
    (0.3, 0.9, 0.4), // Green
    (0.9, 0.3, 0.6), // Pink
    (0.6, 0.4, 0.9), // Purple
    (1.0, 0.9, 0.2), // Yellow
    (0.2, 0.9, 0.9), // Cyan
    (0.9, 0.5, 0.5), // Coral
];

/// A data series for multi-metric comparison.
#[derive(Debug, Clone)]
pub struct DataSeries {
    /// Name/identifier for the series.
    pub name: String,
    /// Data points for this series.
    pub data: Vec<DataPoint>,
    /// Color for this series (RGB, 0.0-1.0).
    pub color: (f32, f32, f32),
    /// Whether this series is visible.
    pub visible: bool,
}

impl DataSeries {
    /// Create a new data series.
    pub fn new(name: impl Into<String>, color: (f32, f32, f32)) -> Self {
        Self {
            name: name.into(),
            data: Vec::new(),
            color,
            visible: true,
        }
    }

    /// Create a new data series with auto-assigned color.
    pub fn with_index(name: impl Into<String>, index: usize) -> Self {
        let color = SERIES_COLORS[index % SERIES_COLORS.len()];
        Self::new(name, color)
    }

    /// Add a data point to the series.
    pub fn push(&mut self, point: DataPoint) {
        self.data.push(point);
    }

    /// Set all data points.
    pub fn set_data(&mut self, data: Vec<DataPoint>) {
        self.data = data;
    }

    /// Get visible data points within a time range.
    pub fn visible_data(&self, start: i64, end: i64) -> impl Iterator<Item = &DataPoint> {
        self.data
            .iter()
            .filter(move |p| p.timestamp >= start && p.timestamp <= end)
    }

    /// Toggle visibility.
    pub fn toggle_visibility(&mut self) {
        self.visible = !self.visible;
    }
}

/// Time window for the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeWindow {
    /// 1 minute window.
    OneMinute,
    /// 5 minute window (default).
    #[default]
    FiveMinutes,
    /// 15 minute window.
    FifteenMinutes,
    /// 1 hour window.
    OneHour,
    /// 6 hour window.
    SixHours,
    /// 24 hour window.
    TwentyFourHours,
    /// 7 day window.
    SevenDays,
}

impl TimeWindow {
    /// Get the window duration in milliseconds.
    pub fn duration_ms(&self) -> i64 {
        match self {
            TimeWindow::OneMinute => 60_000,
            TimeWindow::FiveMinutes => 5 * 60_000,
            TimeWindow::FifteenMinutes => 15 * 60_000,
            TimeWindow::OneHour => 60 * 60_000,
            TimeWindow::SixHours => 6 * 60 * 60_000,
            TimeWindow::TwentyFourHours => 24 * 60 * 60_000,
            TimeWindow::SevenDays => 7 * 24 * 60 * 60_000,
        }
    }

    /// Get the display label.
    pub fn label(&self) -> &'static str {
        match self {
            TimeWindow::OneMinute => "1m",
            TimeWindow::FiveMinutes => "5m",
            TimeWindow::FifteenMinutes => "15m",
            TimeWindow::OneHour => "1h",
            TimeWindow::SixHours => "6h",
            TimeWindow::TwentyFourHours => "24h",
            TimeWindow::SevenDays => "7d",
        }
    }

    /// Get all time window options.
    pub fn all() -> &'static [TimeWindow] {
        &[
            TimeWindow::OneMinute,
            TimeWindow::FiveMinutes,
            TimeWindow::FifteenMinutes,
            TimeWindow::OneHour,
            TimeWindow::SixHours,
            TimeWindow::TwentyFourHours,
            TimeWindow::SevenDays,
        ]
    }
}

/// Pan step as fraction of visible range.
pub const PAN_STEP: f64 = 0.25;

/// A horizontal threshold line on the chart.
#[derive(Debug, Clone)]
pub struct ThresholdLine {
    /// The value at which to draw the line.
    pub value: f64,
    /// Label for the threshold.
    pub label: String,
    /// Color of the line (RGB).
    pub color: (f32, f32, f32),
    /// Whether this is a warning (dashed) or critical (solid) threshold.
    pub is_warning: bool,
}

impl ThresholdLine {
    /// Create a critical threshold (solid red line).
    pub fn critical(value: f64, label: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
            color: (1.0, 0.3, 0.3),
            is_warning: false,
        }
    }

    /// Create a warning threshold (dashed orange line).
    pub fn warning(value: f64, label: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
            color: (1.0, 0.7, 0.2),
            is_warning: true,
        }
    }

    /// Create a baseline/target threshold (dashed green line).
    pub fn baseline(value: f64, label: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
            color: (0.3, 0.8, 0.3),
            is_warning: true,
        }
    }
}

/// State for the time-series chart.
#[derive(Debug)]
pub struct ChartState {
    /// The data points to display (single-series mode for backward compat).
    data: Vec<DataPoint>,
    /// Multiple data series for comparison mode.
    series: Vec<DataSeries>,
    /// Current time window.
    time_window: TimeWindow,
    /// Chart title/metric name.
    title: String,
    /// Cache for the chart geometry.
    cache: Cache,
    /// Minimum value in the data.
    min_value: f64,
    /// Maximum value in the data.
    max_value: f64,
    /// Current timestamp (for calculating visible range).
    current_time: i64,
    /// Current zoom level (1.0 = 100%, 2.0 = 200%, etc.).
    zoom_level: f32,
    /// Pan offset as fraction of visible range (0.0 = centered on now, positive = looking at past).
    pan_offset: f64,
    /// Whether zoom feedback should be shown.
    show_zoom_feedback: bool,
    /// Timestamp when zoom feedback was triggered.
    zoom_feedback_time: i64,
    /// Whether pan feedback should be shown.
    show_pan_feedback: bool,
    /// Timestamp when pan feedback was triggered.
    pan_feedback_time: i64,
    /// Drag start position for mouse panning.
    drag_start: Option<f32>,
    /// Pan offset when drag started.
    drag_start_offset: f64,
    /// Threshold/baseline lines to display on the chart.
    thresholds: Vec<ThresholdLine>,
}

impl ChartState {
    /// Create a new chart state.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            data: Vec::new(),
            series: Vec::new(),
            time_window: TimeWindow::default(),
            title: title.into(),
            cache: Cache::new(),
            min_value: 0.0,
            max_value: 1.0,
            current_time: current_timestamp(),
            zoom_level: 1.0,
            pan_offset: 0.0,
            show_zoom_feedback: false,
            zoom_feedback_time: 0,
            show_pan_feedback: false,
            pan_feedback_time: 0,
            drag_start: None,
            drag_start_offset: 0.0,
            thresholds: Vec::new(),
        }
    }

    /// Add a threshold line to the chart.
    pub fn add_threshold(&mut self, threshold: ThresholdLine) {
        self.thresholds.push(threshold);
        self.cache.clear();
    }

    /// Clear all threshold lines.
    pub fn clear_thresholds(&mut self) {
        self.thresholds.clear();
        self.cache.clear();
    }

    /// Set threshold lines from alert rules.
    pub fn set_thresholds(&mut self, thresholds: Vec<ThresholdLine>) {
        self.thresholds = thresholds;
        self.cache.clear();
    }

    /// Get current thresholds.
    pub fn thresholds(&self) -> &[ThresholdLine] {
        &self.thresholds
    }

    // ========== Multi-series management ==========

    /// Check if chart is in multi-series mode.
    pub fn is_multi_series(&self) -> bool {
        !self.series.is_empty()
    }

    /// Get the number of series.
    pub fn series_count(&self) -> usize {
        self.series.len()
    }

    /// Get all series.
    pub fn series(&self) -> &[DataSeries] {
        &self.series
    }

    /// Add a new series to the chart.
    pub fn add_series(&mut self, name: impl Into<String>) -> usize {
        let name = name.into();
        // Check if series already exists
        if let Some(idx) = self.series.iter().position(|s| s.name == name) {
            return idx;
        }
        let index = self.series.len();
        self.series.push(DataSeries::with_index(name, index));
        self.cache.clear();
        index
    }

    /// Add a series with pre-populated data.
    pub fn add_series_with_data(&mut self, name: impl Into<String>, data: Vec<DataPoint>) -> usize {
        let name = name.into();
        // Check if series already exists
        if let Some(idx) = self.series.iter().position(|s| s.name == name) {
            self.series[idx].set_data(data);
            self.recalculate_bounds();
            self.cache.clear();
            return idx;
        }
        let index = self.series.len();
        let mut series = DataSeries::with_index(&name, index);
        series.set_data(data);
        self.series.push(series);
        self.recalculate_bounds();
        self.cache.clear();
        index
    }

    /// Remove a series by name.
    pub fn remove_series(&mut self, name: &str) -> bool {
        if let Some(idx) = self.series.iter().position(|s| s.name == name) {
            self.series.remove(idx);
            // Re-assign colors to maintain consistency
            for (i, series) in self.series.iter_mut().enumerate() {
                series.color = SERIES_COLORS[i % SERIES_COLORS.len()];
            }
            self.recalculate_bounds();
            self.cache.clear();
            true
        } else {
            false
        }
    }

    /// Check if a series exists by name.
    pub fn has_series(&self, name: &str) -> bool {
        self.series.iter().any(|s| s.name == name)
    }

    /// Toggle series visibility.
    pub fn toggle_series_visibility(&mut self, name: &str) {
        if let Some(series) = self.series.iter_mut().find(|s| s.name == name) {
            series.toggle_visibility();
            self.cache.clear();
        }
    }

    /// Push a data point to a specific series.
    pub fn push_to_series(&mut self, name: &str, point: DataPoint) {
        if let Some(series) = self.series.iter_mut().find(|s| s.name == name) {
            series.push(point);
            self.recalculate_bounds();
            self.cache.clear();
        }
    }

    /// Clear all series.
    pub fn clear_series(&mut self) {
        self.series.clear();
        self.cache.clear();
    }

    /// Get series names.
    pub fn series_names(&self) -> Vec<&str> {
        self.series.iter().map(|s| s.name.as_str()).collect()
    }

    // ========== Zoom controls ==========

    /// Get the current zoom level.
    pub fn zoom_level(&self) -> f32 {
        self.zoom_level
    }

    /// Zoom in by one step.
    pub fn zoom_in(&mut self) {
        self.set_zoom(self.zoom_level + ZOOM_STEP);
    }

    /// Zoom out by one step.
    pub fn zoom_out(&mut self) {
        self.set_zoom(self.zoom_level - ZOOM_STEP);
    }

    /// Set the zoom level (clamped to valid range).
    pub fn set_zoom(&mut self, level: f32) {
        let new_level = level.clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_level - self.zoom_level).abs() > 0.001 {
            self.zoom_level = new_level;
            self.show_zoom_feedback = true;
            self.zoom_feedback_time = current_timestamp();
            self.cache.clear();
        }
    }

    /// Reset zoom to 100%.
    pub fn reset_zoom(&mut self) {
        self.set_zoom(1.0);
        self.pan_offset = 0.0;
    }

    /// Get the current pan offset.
    pub fn pan_offset(&self) -> f64 {
        self.pan_offset
    }

    /// Pan left (back in time) by one step.
    pub fn pan_left(&mut self) {
        self.set_pan(self.pan_offset + PAN_STEP);
    }

    /// Pan right (forward in time) by one step.
    pub fn pan_right(&mut self) {
        self.set_pan(self.pan_offset - PAN_STEP);
    }

    /// Set the pan offset (clamped to valid range).
    /// Offset 0.0 = viewing "now", positive = viewing past.
    pub fn set_pan(&mut self, offset: f64) {
        // Can't pan into the future (offset < 0)
        // Limit how far back we can pan based on available data
        let max_offset = self.max_pan_offset();
        let new_offset = offset.clamp(0.0, max_offset);

        if (new_offset - self.pan_offset).abs() > 0.001 {
            self.pan_offset = new_offset;
            self.show_pan_feedback = true;
            self.pan_feedback_time = current_timestamp();
            self.cache.clear();
        }
    }

    /// Calculate maximum pan offset based on data range.
    fn max_pan_offset(&self) -> f64 {
        // Find oldest data point across single-series data and all multi-series data
        let mut oldest = i64::MAX;

        // Check single-series data
        if !self.data.is_empty()
            && let Some(min_ts) = self.data.iter().map(|p| p.timestamp).min() {
                oldest = oldest.min(min_ts);
            }

        // Check multi-series data
        for series in &self.series {
            if let Some(min_ts) = series.data.iter().map(|p| p.timestamp).min() {
                oldest = oldest.min(min_ts);
            }
        }

        if oldest == i64::MAX {
            return 0.0;
        }

        let duration = self.effective_duration_ms();
        if duration == 0 {
            return 0.0;
        }

        // How many "screens" worth of data do we have?
        let data_span = self.current_time - oldest;
        let screens = data_span as f64 / duration as f64;

        // Allow panning back to see oldest data (minus one screen width)
        (screens - 1.0).max(0.0)
    }

    /// Reset pan to view current time.
    pub fn reset_pan(&mut self) {
        self.set_pan(0.0);
    }

    /// Whether the chart is panned away from "now".
    pub fn is_panned(&self) -> bool {
        self.pan_offset > 0.001
    }

    /// Start dragging for pan.
    pub fn start_drag(&mut self, x: f32) {
        self.drag_start = Some(x);
        self.drag_start_offset = self.pan_offset;
    }

    /// Update pan during drag.
    pub fn update_drag(&mut self, x: f32, chart_width: f32) {
        if let Some(start_x) = self.drag_start {
            // Calculate how much we've dragged as fraction of chart width
            let delta_x = start_x - x; // Positive = dragged left = pan into past
            let delta_fraction = delta_x as f64 / chart_width as f64;

            // Apply to pan offset
            self.set_pan(self.drag_start_offset + delta_fraction);
        }
    }

    /// End dragging.
    pub fn end_drag(&mut self) {
        self.drag_start = None;
    }

    /// Whether currently dragging.
    pub fn is_dragging(&self) -> bool {
        self.drag_start.is_some()
    }

    /// Check if zoom feedback should be hidden (after timeout).
    pub fn update_zoom_feedback(&mut self) {
        if self.show_zoom_feedback {
            let elapsed = current_timestamp() - self.zoom_feedback_time;
            if elapsed > 1500 {
                // Hide after 1.5 seconds
                self.show_zoom_feedback = false;
                self.cache.clear();
            }
        }
    }

    /// Check if pan feedback should be hidden (after timeout).
    pub fn update_pan_feedback(&mut self) {
        if self.show_pan_feedback {
            let elapsed = current_timestamp() - self.pan_feedback_time;
            if elapsed > 1500 {
                self.show_pan_feedback = false;
                self.cache.clear();
            }
        }
    }

    /// Whether zoom feedback overlay should be shown.
    pub fn should_show_zoom_feedback(&self) -> bool {
        self.show_zoom_feedback
    }

    /// Whether pan feedback overlay should be shown.
    pub fn should_show_pan_feedback(&self) -> bool {
        self.show_pan_feedback
    }

    /// Set the time window.
    pub fn set_time_window(&mut self, window: TimeWindow) {
        if self.time_window != window {
            self.time_window = window;
            self.cache.clear();
        }
    }

    /// Get the current time window.
    pub fn time_window(&self) -> TimeWindow {
        self.time_window
    }

    /// Add a data point.
    pub fn push(&mut self, point: DataPoint) {
        self.data.push(point);
        self.recalculate_bounds();
        self.cache.clear();
    }

    /// Set all data points.
    pub fn set_data(&mut self, data: Vec<DataPoint>) {
        self.data = data;
        self.recalculate_bounds();
        self.cache.clear();
    }

    /// Update the current time (call on tick).
    pub fn update_time(&mut self) {
        let new_time = current_timestamp();
        if new_time != self.current_time {
            self.current_time = new_time;
            self.cache.clear();
        }
    }

    /// Get the effective time window duration accounting for zoom.
    fn effective_duration_ms(&self) -> i64 {
        (self.time_window.duration_ms() as f64 / self.zoom_level as f64) as i64
    }

    /// Get the visible time range (start, end) accounting for zoom and pan.
    fn visible_time_range(&self) -> (i64, i64) {
        let duration = self.effective_duration_ms();
        let end = self.current_time - (self.pan_offset * duration as f64) as i64;
        let start = end - duration;
        (start, end)
    }

    /// Get visible data points within the current time window (with zoom).
    fn visible_data(&self) -> impl Iterator<Item = &DataPoint> {
        let (start, end) = self.visible_time_range();
        self.data
            .iter()
            .filter(move |p| p.timestamp >= start && p.timestamp <= end)
    }

    /// Recalculate min/max bounds.
    fn recalculate_bounds(&mut self) {
        let (start, end) = self.visible_time_range();

        let mut min_val = f64::INFINITY;
        let mut max_val = f64::NEG_INFINITY;

        // Check single-series data
        for p in self
            .data
            .iter()
            .filter(|p| p.timestamp >= start && p.timestamp <= end)
        {
            min_val = min_val.min(p.value);
            max_val = max_val.max(p.value);
        }

        // Check multi-series data (only visible series)
        for series in &self.series {
            if !series.visible {
                continue;
            }
            for p in series
                .data
                .iter()
                .filter(|p| p.timestamp >= start && p.timestamp <= end)
            {
                min_val = min_val.min(p.value);
                max_val = max_val.max(p.value);
            }
        }

        if min_val == f64::INFINITY || max_val == f64::NEG_INFINITY {
            self.min_value = 0.0;
            self.max_value = 1.0;
            return;
        }

        self.min_value = min_val;
        self.max_value = max_val;

        // Add some padding
        let range = self.max_value - self.min_value;
        if range < 0.001 {
            // Very small range, add artificial padding
            self.min_value -= 0.5;
            self.max_value += 0.5;
        } else {
            let padding = range * 0.1;
            self.min_value -= padding;
            self.max_value += padding;
        }
    }

    /// Get statistics for the visible data.
    pub fn stats(&self) -> ChartStats {
        let visible: Vec<_> = self.visible_data().collect();

        if visible.is_empty() {
            return ChartStats::default();
        }

        let sum: f64 = visible.iter().map(|p| p.value).sum();
        let count = visible.len();
        let avg = sum / count as f64;

        let min = visible
            .iter()
            .map(|p| p.value)
            .fold(f64::INFINITY, f64::min);
        let max = visible
            .iter()
            .map(|p| p.value)
            .fold(f64::NEG_INFINITY, f64::max);

        let current = visible.last().map(|p| p.value);

        ChartStats {
            min,
            max,
            avg,
            current,
            count,
        }
    }
}

/// Statistics for the chart data.
#[derive(Debug, Clone, Default)]
pub struct ChartStats {
    /// Minimum value.
    pub min: f64,
    /// Maximum value.
    pub max: f64,
    /// Average value.
    pub avg: f64,
    /// Current (most recent) value.
    pub current: Option<f64>,
    /// Number of data points.
    pub count: usize,
}

/// Chart widget that renders the time-series data.
pub struct Chart<'a> {
    state: &'a ChartState,
}

impl<'a> Chart<'a> {
    /// Create a new chart widget.
    pub fn new(state: &'a ChartState) -> Self {
        Self { state }
    }
}

/// Internal state for chart interaction.
#[derive(Debug, Clone, Default)]
pub struct ChartInteraction {
    /// Whether Ctrl key is pressed.
    ctrl_pressed: bool,
    /// Whether mouse is being dragged.
    dragging: bool,
}

impl<'a> canvas::Program<crate::message::Message> for Chart<'a> {
    type State = ChartInteraction;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<crate::message::Message>> {
        match event {
            // Track Ctrl key state
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.ctrl_pressed = modifiers.control();
                None
            }

            // Handle keyboard zoom (+/- and 0 for reset) and pan (arrow keys)
            Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => {
                match key {
                    keyboard::Key::Character(c) => {
                        let c_str = c.as_str();
                        if c_str == "+" || c_str == "=" {
                            return Some(Action::publish(crate::message::Message::ChartZoomIn));
                        } else if c_str == "-" || c_str == "_" {
                            return Some(Action::publish(crate::message::Message::ChartZoomOut));
                        } else if c_str == "0" {
                            return Some(Action::publish(crate::message::Message::ChartZoomReset));
                        }
                    }
                    keyboard::Key::Named(keyboard::key::Named::ArrowLeft) => {
                        return Some(Action::publish(crate::message::Message::ChartPanLeft));
                    }
                    keyboard::Key::Named(keyboard::key::Named::ArrowRight) => {
                        return Some(Action::publish(crate::message::Message::ChartPanRight));
                    }
                    keyboard::Key::Named(keyboard::key::Named::Home) => {
                        return Some(Action::publish(crate::message::Message::ChartPanReset));
                    }
                    _ => {}
                }
                None
            }

            // Handle Ctrl+scroll for zoom
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if state.ctrl_pressed && cursor.is_over(bounds) {
                    let zoom_delta = match delta {
                        mouse::ScrollDelta::Lines { y, .. } => *y,
                        mouse::ScrollDelta::Pixels { y, .. } => *y / 50.0,
                    };

                    if zoom_delta > 0.0 {
                        return Some(Action::publish(crate::message::Message::ChartZoomIn));
                    } else if zoom_delta < 0.0 {
                        return Some(Action::publish(crate::message::Message::ChartZoomOut));
                    }
                }
                None
            }

            // Handle mouse drag for panning
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if cursor.is_over(bounds)
                    && let Some(pos) = cursor.position() {
                        state.dragging = true;
                        return Some(Action::publish(crate::message::Message::ChartDragStart(
                            pos.x,
                        )));
                    }
                None
            }

            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if state.dragging {
                    let chart_width = bounds.width - 100.0;
                    return Some(Action::publish(crate::message::Message::ChartDragUpdate(
                        position.x,
                        chart_width,
                    )));
                }
                None
            }

            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.dragging {
                    state.dragging = false;
                    return Some(Action::publish(crate::message::Message::ChartDragEnd));
                }
                None
            }

            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geometry = self.state.cache.draw(renderer, bounds.size(), |frame| {
            self.draw_chart(frame, bounds.size());
        });

        vec![geometry]
    }
}

impl<'a> Chart<'a> {
    /// Draw the chart onto the frame.
    fn draw_chart(&self, frame: &mut Frame, size: Size) {
        let padding = 50.0;
        let chart_width = size.width - padding * 2.0;
        let chart_height = size.height - padding * 2.0;

        if chart_width <= 0.0 || chart_height <= 0.0 {
            return;
        }

        // Draw background
        let background = Path::rectangle(Point::ORIGIN, size);
        frame.fill(&background, Color::from_rgb(0.1, 0.1, 0.12));

        // Draw chart area background
        let chart_bg = Path::rectangle(
            Point::new(padding, padding),
            Size::new(chart_width, chart_height),
        );
        frame.fill(&chart_bg, Color::from_rgb(0.08, 0.08, 0.1));

        // Draw title
        let title = Text {
            content: self.state.title.clone(),
            position: Point::new(padding, 10.0),
            color: Color::WHITE,
            size: 14.0.into(),
            ..Text::default()
        };
        frame.fill_text(title);

        // Draw time window and zoom label
        let zoom_pct = (self.state.zoom_level * 100.0) as i32;
        let window_label = Text {
            content: format!(
                "Window: {} | Zoom: {}%",
                self.state.time_window.label(),
                zoom_pct
            ),
            position: Point::new(size.width - padding - 140.0, 10.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: 12.0.into(),
            ..Text::default()
        };
        frame.fill_text(window_label);

        // Draw zoom feedback overlay if recently changed
        if self.state.show_zoom_feedback {
            self.draw_zoom_feedback(frame, size, zoom_pct);
        }

        // Draw pan feedback overlay if recently changed
        if self.state.show_pan_feedback {
            self.draw_pan_feedback(frame, size);
        }

        // Draw "panned" indicator if not viewing current time
        if self.state.is_panned() {
            self.draw_pan_indicator(frame, size, padding);
        }

        // Calculate time range (with zoom)
        let (time_start, time_end) = self.state.visible_time_range();
        let time_range = (time_end - time_start) as f64;

        // Calculate value range
        let value_min = self.state.min_value;
        let value_max = self.state.max_value;
        let value_range = value_max - value_min;

        // Check if we have any data to display
        let has_single_series_data = !self.state.data.is_empty();
        let has_multi_series_data = self.state.series.iter().any(|s| !s.data.is_empty());

        if !has_single_series_data && !has_multi_series_data {
            // Draw "no data" message
            let no_data = Text {
                content: "No data".to_string(),
                position: Point::new(size.width / 2.0 - 30.0, size.height / 2.0),
                color: Color::from_rgb(0.5, 0.5, 0.5),
                size: 16.0.into(),
                ..Text::default()
            };
            frame.fill_text(no_data);
            return;
        }

        // Draw grid lines
        self.draw_grid(
            frame,
            padding,
            chart_width,
            chart_height,
            value_min,
            value_max,
        );

        // Draw threshold lines
        self.draw_thresholds(
            frame,
            padding,
            chart_width,
            chart_height,
            value_min,
            value_max,
        );

        // Draw multi-series data if present
        if has_multi_series_data {
            self.draw_multi_series(
                frame,
                padding,
                chart_width,
                chart_height,
                time_start,
                time_end,
                time_range,
                value_min,
                value_range,
            );

            // Draw legend for multi-series
            self.draw_legend(frame, padding, chart_height);
        } else {
            // Draw single-series data (backward compatibility)
            let visible_data: Vec<_> = self.state.visible_data().collect();

            if visible_data.len() >= 2 {
                let mut path_builder = canvas::path::Builder::new();
                let mut first = true;

                for point in &visible_data {
                    let x = padding
                        + ((point.timestamp - time_start) as f64 / time_range) as f32 * chart_width;
                    let y = padding + chart_height
                        - ((point.value - value_min) / value_range) as f32 * chart_height;

                    if first {
                        path_builder.move_to(Point::new(x, y));
                        first = false;
                    } else {
                        path_builder.line_to(Point::new(x, y));
                    }
                }

                let path = path_builder.build();
                frame.stroke(
                    &path,
                    Stroke::default()
                        .with_color(Color::from_rgb(0.2, 0.7, 1.0))
                        .with_width(2.0),
                );
            }

            // Draw data points
            for point in &visible_data {
                let x = padding
                    + ((point.timestamp - time_start) as f64 / time_range) as f32 * chart_width;
                let y = padding + chart_height
                    - ((point.value - value_min) / value_range) as f32 * chart_height;

                let dot = Path::circle(Point::new(x, y), 3.0);
                frame.fill(&dot, Color::from_rgb(0.3, 0.8, 1.0));
            }

            // Draw stats for single series
            let stats = self.state.stats();
            self.draw_stats(frame, size, padding, &stats);
        }
    }

    /// Draw grid lines and labels.
    fn draw_grid(
        &self,
        frame: &mut Frame,
        padding: f32,
        chart_width: f32,
        chart_height: f32,
        value_min: f64,
        value_max: f64,
    ) {
        let grid_color = Color::from_rgb(0.2, 0.2, 0.25);
        let label_color = Color::from_rgb(0.5, 0.5, 0.5);

        // Horizontal grid lines (value axis)
        let num_h_lines = 5;
        let value_range = value_max - value_min;

        for i in 0..=num_h_lines {
            let y = padding + (i as f32 / num_h_lines as f32) * chart_height;
            let value = value_max - (i as f64 / num_h_lines as f64) * value_range;

            // Grid line
            let line = Path::line(Point::new(padding, y), Point::new(padding + chart_width, y));
            frame.stroke(
                &line,
                Stroke::default().with_color(grid_color).with_width(1.0),
            );

            // Value label
            let label = Text {
                content: format_value(value),
                position: Point::new(5.0, y - 6.0),
                color: label_color,
                size: 10.0.into(),
                ..Text::default()
            };
            frame.fill_text(label);
        }

        // Vertical grid lines (time axis)
        let num_v_lines = 4;

        for i in 0..=num_v_lines {
            let x = padding + (i as f32 / num_v_lines as f32) * chart_width;

            // Grid line
            let line = Path::line(
                Point::new(x, padding),
                Point::new(x, padding + chart_height),
            );
            frame.stroke(
                &line,
                Stroke::default().with_color(grid_color).with_width(1.0),
            );

            // Time label (use effective duration for zoom)
            let time_offset =
                self.state.effective_duration_ms() as f64 * (1.0 - i as f64 / num_v_lines as f64);
            let label_text = format_time_offset(time_offset as i64);

            let label = Text {
                content: label_text,
                position: Point::new(x - 15.0, padding + chart_height + 15.0),
                color: label_color,
                size: 10.0.into(),
                ..Text::default()
            };
            frame.fill_text(label);
        }
    }

    /// Draw zoom feedback overlay.
    fn draw_zoom_feedback(&self, frame: &mut Frame, size: Size, zoom_pct: i32) {
        // Semi-transparent background box
        let box_width = 120.0;
        let box_height = 50.0;
        let box_x = (size.width - box_width) / 2.0;
        let box_y = (size.height - box_height) / 2.0;

        let bg = Path::rectangle(Point::new(box_x, box_y), Size::new(box_width, box_height));
        frame.fill(&bg, Color::from_rgba(0.0, 0.0, 0.0, 0.7));

        // Border
        frame.stroke(
            &bg,
            Stroke::default()
                .with_color(Color::from_rgb(0.3, 0.8, 1.0))
                .with_width(2.0),
        );

        // Zoom icon (magnifying glass approximation)
        let icon_text = if zoom_pct > 100 {
            "+"
        } else if zoom_pct < 100 {
            "-"
        } else {
            ""
        };
        let zoom_text = Text {
            content: format!("{}{}%", icon_text, zoom_pct),
            position: Point::new(box_x + box_width / 2.0 - 25.0, box_y + 15.0),
            color: Color::WHITE,
            size: 20.0.into(),
            ..Text::default()
        };
        frame.fill_text(zoom_text);

        // Hint text
        let hint = Text {
            content: "Ctrl+Scroll or +/-".to_string(),
            position: Point::new(box_x + 10.0, box_y + box_height - 15.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: 9.0.into(),
            ..Text::default()
        };
        frame.fill_text(hint);
    }

    /// Draw pan feedback overlay.
    fn draw_pan_feedback(&self, frame: &mut Frame, size: Size) {
        let box_width = 140.0;
        let box_height = 50.0;
        let box_x = (size.width - box_width) / 2.0;
        let box_y = (size.height - box_height) / 2.0;

        let bg = Path::rectangle(Point::new(box_x, box_y), Size::new(box_width, box_height));
        frame.fill(&bg, Color::from_rgba(0.0, 0.0, 0.0, 0.7));

        frame.stroke(
            &bg,
            Stroke::default()
                .with_color(Color::from_rgb(0.3, 0.8, 1.0))
                .with_width(2.0),
        );

        // Pan direction indicator
        let (icon, label) = if self.state.pan_offset > 0.001 {
            ("<< Past", format!("{:.0}%", self.state.pan_offset * 100.0))
        } else {
            (">> Now", "Live".to_string())
        };

        let pan_text = Text {
            content: format!("{} {}", icon, label),
            position: Point::new(box_x + 15.0, box_y + 15.0),
            color: Color::WHITE,
            size: 16.0.into(),
            ..Text::default()
        };
        frame.fill_text(pan_text);

        let hint = Text {
            content: "Drag or Arrow keys".to_string(),
            position: Point::new(box_x + 15.0, box_y + box_height - 15.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: 9.0.into(),
            ..Text::default()
        };
        frame.fill_text(hint);
    }

    /// Draw pan indicator when viewing historical data.
    fn draw_pan_indicator(&self, frame: &mut Frame, _size: Size, padding: f32) {
        // Draw a "PAUSED - Viewing History" badge at top left
        let badge_width = 150.0;
        let badge_height = 22.0;
        let badge_x = padding;
        let badge_y = padding - 25.0;

        let bg = Path::rectangle(
            Point::new(badge_x, badge_y),
            Size::new(badge_width, badge_height),
        );
        frame.fill(&bg, Color::from_rgba(1.0, 0.6, 0.0, 0.8));

        let text = Text {
            content: "PAUSED - Viewing Past".to_string(),
            position: Point::new(badge_x + 8.0, badge_y + 4.0),
            color: Color::BLACK,
            size: 11.0.into(),
            ..Text::default()
        };
        frame.fill_text(text);

        // Draw "Return to Now" button hint
        let hint = Text {
            content: "(Home to reset)".to_string(),
            position: Point::new(badge_x + badge_width + 10.0, badge_y + 4.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: 10.0.into(),
            ..Text::default()
        };
        frame.fill_text(hint);
    }

    /// Draw threshold/baseline lines.
    fn draw_thresholds(
        &self,
        frame: &mut Frame,
        padding: f32,
        chart_width: f32,
        chart_height: f32,
        value_min: f64,
        value_max: f64,
    ) {
        let value_range = value_max - value_min;
        if value_range <= 0.0 {
            return;
        }

        for threshold in &self.state.thresholds {
            // Skip if threshold is outside visible range
            if threshold.value < value_min || threshold.value > value_max {
                continue;
            }

            // Calculate Y position
            let y = padding + chart_height
                - ((threshold.value - value_min) / value_range) as f32 * chart_height;

            let color = Color::from_rgb(threshold.color.0, threshold.color.1, threshold.color.2);

            // Draw the line
            let line = Path::line(Point::new(padding, y), Point::new(padding + chart_width, y));

            // Use different widths: thinner for warning/baseline, thicker for critical
            let stroke = if threshold.is_warning {
                Stroke::default().with_color(color).with_width(1.5)
            } else {
                Stroke::default().with_color(color).with_width(2.5)
            };

            frame.stroke(&line, stroke);

            // Draw label
            let label = Text {
                content: format!("{} ({})", threshold.label, format_value(threshold.value)),
                position: Point::new(padding + 5.0, y - 12.0),
                color,
                size: 10.0.into(),
                ..Text::default()
            };
            frame.fill_text(label);
        }
    }

    /// Draw statistics overlay.
    /// Draw multiple data series.
    #[allow(clippy::too_many_arguments)]
    fn draw_multi_series(
        &self,
        frame: &mut Frame,
        padding: f32,
        chart_width: f32,
        chart_height: f32,
        time_start: i64,
        time_end: i64,
        time_range: f64,
        value_min: f64,
        value_range: f64,
    ) {
        for series in &self.state.series {
            if !series.visible {
                continue;
            }

            let visible_data: Vec<_> = series.visible_data(time_start, time_end).collect();

            if visible_data.len() < 2 {
                continue;
            }

            // Draw the line
            let mut path_builder = canvas::path::Builder::new();
            let mut first = true;

            for point in &visible_data {
                let x = padding
                    + ((point.timestamp - time_start) as f64 / time_range) as f32 * chart_width;
                let y = padding + chart_height
                    - ((point.value - value_min) / value_range) as f32 * chart_height;

                if first {
                    path_builder.move_to(Point::new(x, y));
                    first = false;
                } else {
                    path_builder.line_to(Point::new(x, y));
                }
            }

            let path = path_builder.build();
            let color = Color::from_rgb(series.color.0, series.color.1, series.color.2);
            frame.stroke(&path, Stroke::default().with_color(color).with_width(2.0));

            // Draw data points
            for point in &visible_data {
                let x = padding
                    + ((point.timestamp - time_start) as f64 / time_range) as f32 * chart_width;
                let y = padding + chart_height
                    - ((point.value - value_min) / value_range) as f32 * chart_height;

                // Slightly brighter color for points
                let point_color = Color::from_rgb(
                    (series.color.0 + 0.1).min(1.0),
                    (series.color.1 + 0.1).min(1.0),
                    (series.color.2 + 0.1).min(1.0),
                );
                let dot = Path::circle(Point::new(x, y), 3.0);
                frame.fill(&dot, point_color);
            }
        }
    }

    /// Draw legend for multi-series chart.
    fn draw_legend(&self, frame: &mut Frame, padding: f32, chart_height: f32) {
        let legend_x = padding + 10.0;
        let legend_y = padding + chart_height + 25.0;
        let item_width = 120.0;
        let max_items_per_row = 4;

        for (i, series) in self.state.series.iter().enumerate() {
            let row = i / max_items_per_row;
            let col = i % max_items_per_row;
            let x = legend_x + col as f32 * item_width;
            let y = legend_y + row as f32 * 16.0;

            // Color indicator box
            let color = if series.visible {
                Color::from_rgb(series.color.0, series.color.1, series.color.2)
            } else {
                Color::from_rgb(0.4, 0.4, 0.4) // Dimmed for hidden series
            };
            let box_path = Path::rectangle(Point::new(x, y), Size::new(12.0, 12.0));
            frame.fill(&box_path, color);

            // Series name (truncated if needed)
            let name = if series.name.len() > 12 {
                format!("{}...", &series.name[..9])
            } else {
                series.name.clone()
            };

            let text_color = if series.visible {
                Color::from_rgb(0.8, 0.8, 0.8)
            } else {
                Color::from_rgb(0.5, 0.5, 0.5)
            };

            let label = Text {
                content: name,
                position: Point::new(x + 16.0, y),
                color: text_color,
                size: 10.0.into(),
                ..Text::default()
            };
            frame.fill_text(label);
        }
    }

    fn draw_stats(&self, frame: &mut Frame, size: Size, padding: f32, stats: &ChartStats) {
        let stats_x = size.width - padding - 100.0;
        let stats_y = padding + 10.0;
        let line_height = 14.0;

        let stats_lines = [
            format!(
                "Current: {}",
                stats.current.map_or("-".to_string(), format_value)
            ),
            format!("Min: {}", format_value(stats.min)),
            format!("Max: {}", format_value(stats.max)),
            format!("Avg: {}", format_value(stats.avg)),
            format!("Points: {}", stats.count),
        ];

        for (i, line) in stats_lines.iter().enumerate() {
            let text = Text {
                content: line.clone(),
                position: Point::new(stats_x, stats_y + i as f32 * line_height),
                color: Color::from_rgb(0.7, 0.7, 0.7),
                size: 11.0.into(),
                ..Text::default()
            };
            frame.fill_text(text);
        }
    }
}

/// Create a chart element.
pub fn chart_view(state: &ChartState) -> Element<'_, crate::message::Message> {
    Canvas::new(Chart::new(state))
        .width(Length::Fill)
        .height(Length::Fixed(200.0))
        .into()
}

/// Get the current timestamp in milliseconds.
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_window_duration() {
        assert_eq!(TimeWindow::OneMinute.duration_ms(), 60_000);
        assert_eq!(TimeWindow::FiveMinutes.duration_ms(), 300_000);
        assert_eq!(TimeWindow::FifteenMinutes.duration_ms(), 900_000);
        assert_eq!(TimeWindow::OneHour.duration_ms(), 3_600_000);
    }

    #[test]
    fn test_data_point_from_telemetry() {
        let counter = TelemetryValue::Counter(42);
        let gauge = TelemetryValue::Gauge(3.14);
        let text = TelemetryValue::Text("hello".to_string());

        assert!(DataPoint::from_telemetry(1000, &counter).is_some());
        assert!(DataPoint::from_telemetry(1000, &gauge).is_some());
        assert!(DataPoint::from_telemetry(1000, &text).is_none());
    }

    #[test]
    fn test_chart_state_push() {
        let mut chart = ChartState::new("test");
        assert!(chart.data.is_empty());

        chart.push(DataPoint::new(1000, 10.0));
        assert_eq!(chart.data.len(), 1);

        chart.push(DataPoint::new(2000, 20.0));
        assert_eq!(chart.data.len(), 2);
    }

    #[test]
    fn test_chart_stats() {
        let mut chart = ChartState::new("test");
        chart.current_time = 10000;

        chart.push(DataPoint::new(5000, 10.0));
        chart.push(DataPoint::new(6000, 20.0));
        chart.push(DataPoint::new(7000, 15.0));

        let stats = chart.stats();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 20.0);
        assert_eq!(stats.avg, 15.0);
        assert_eq!(stats.current, Some(15.0));
    }

    #[test]
    fn test_format_value() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(3.14159), "3.14");
        assert_eq!(format_value(1500.0), "1.5K");
        assert_eq!(format_value(2500000.0), "2.5M");
    }

    #[test]
    fn test_zoom_in_out() {
        let mut chart = ChartState::new("test");
        assert_eq!(chart.zoom_level(), 1.0);

        chart.zoom_in();
        assert_eq!(chart.zoom_level(), 1.25);

        chart.zoom_in();
        assert_eq!(chart.zoom_level(), 1.5);

        chart.zoom_out();
        assert_eq!(chart.zoom_level(), 1.25);

        chart.reset_zoom();
        assert_eq!(chart.zoom_level(), 1.0);
    }

    #[test]
    fn test_zoom_limits() {
        let mut chart = ChartState::new("test");

        // Can't zoom below MIN_ZOOM
        chart.set_zoom(0.5);
        assert_eq!(chart.zoom_level(), MIN_ZOOM);

        // Can't zoom above MAX_ZOOM
        chart.set_zoom(15.0);
        assert_eq!(chart.zoom_level(), MAX_ZOOM);
    }

    #[test]
    fn test_effective_duration_with_zoom() {
        let mut chart = ChartState::new("test");
        chart.set_time_window(TimeWindow::FiveMinutes);

        // At 1x zoom, effective duration = 5 minutes
        assert_eq!(chart.effective_duration_ms(), 300_000);

        // At 2x zoom, effective duration = 2.5 minutes
        chart.set_zoom(2.0);
        assert_eq!(chart.effective_duration_ms(), 150_000);

        // At 4x zoom, effective duration = 1.25 minutes
        chart.set_zoom(4.0);
        assert_eq!(chart.effective_duration_ms(), 75_000);
    }

    #[test]
    fn test_pan_left_right() {
        let mut chart = ChartState::new("test");
        chart.current_time = 1_000_000;

        // Add some historical data so we can pan back
        for i in 0..100 {
            chart.push(DataPoint::new(chart.current_time - i * 10_000, i as f64));
        }

        // Initially at "now" (offset 0)
        assert_eq!(chart.pan_offset(), 0.0);
        assert!(!chart.is_panned());

        // Pan left (back in time)
        chart.pan_left();
        assert!((chart.pan_offset() - PAN_STEP).abs() < 0.001);
        assert!(chart.is_panned());

        // Pan right (forward in time)
        chart.pan_right();
        assert_eq!(chart.pan_offset(), 0.0);
        assert!(!chart.is_panned());
    }

    #[test]
    fn test_pan_limits() {
        let mut chart = ChartState::new("test");
        chart.current_time = 100_000;

        // With no data, can't pan back
        chart.pan_left();
        assert_eq!(chart.pan_offset(), 0.0);

        // Can't pan into the future
        chart.set_pan(-1.0);
        assert_eq!(chart.pan_offset(), 0.0);
    }

    #[test]
    fn test_pan_reset() {
        let mut chart = ChartState::new("test");
        chart.current_time = 1_000_000;

        // Add historical data
        for i in 0..100 {
            chart.push(DataPoint::new(chart.current_time - i * 10_000, i as f64));
        }

        // Pan back
        chart.pan_left();
        chart.pan_left();
        assert!(chart.is_panned());

        // Reset
        chart.reset_pan();
        assert!(!chart.is_panned());
        assert_eq!(chart.pan_offset(), 0.0);
    }

    #[test]
    fn test_drag_panning() {
        let mut chart = ChartState::new("test");
        chart.current_time = 1_000_000;

        // Add historical data
        for i in 0..100 {
            chart.push(DataPoint::new(chart.current_time - i * 10_000, i as f64));
        }

        // Start drag at x=500
        chart.start_drag(500.0);
        assert!(chart.is_dragging());

        // Drag left (to x=400) should pan into the past
        chart.update_drag(400.0, 500.0);
        assert!(chart.pan_offset() > 0.0);

        // End drag
        chart.end_drag();
        assert!(!chart.is_dragging());
    }

    #[test]
    fn test_threshold_lines() {
        let mut chart = ChartState::new("test");

        // Initially no thresholds
        assert!(chart.thresholds().is_empty());

        // Add thresholds
        chart.add_threshold(ThresholdLine::critical(100.0, "Critical"));
        chart.add_threshold(ThresholdLine::warning(80.0, "Warning"));
        chart.add_threshold(ThresholdLine::baseline(50.0, "Target"));

        assert_eq!(chart.thresholds().len(), 3);

        // Check threshold properties
        let critical = &chart.thresholds()[0];
        assert_eq!(critical.value, 100.0);
        assert_eq!(critical.label, "Critical");
        assert!(!critical.is_warning);

        let warning = &chart.thresholds()[1];
        assert_eq!(warning.value, 80.0);
        assert!(warning.is_warning);

        // Clear thresholds
        chart.clear_thresholds();
        assert!(chart.thresholds().is_empty());
    }

    #[test]
    fn test_set_thresholds() {
        let mut chart = ChartState::new("test");

        let thresholds = vec![
            ThresholdLine::critical(100.0, "Max"),
            ThresholdLine::baseline(0.0, "Min"),
        ];

        chart.set_thresholds(thresholds);
        assert_eq!(chart.thresholds().len(), 2);
    }

    // ========== Multi-series tests ==========

    #[test]
    fn test_multi_series_add_remove() {
        let mut chart = ChartState::new("comparison");

        // Initially not in multi-series mode
        assert!(!chart.is_multi_series());
        assert_eq!(chart.series_count(), 0);

        // Add first series
        let idx = chart.add_series("metric1");
        assert_eq!(idx, 0);
        assert!(chart.is_multi_series());
        assert_eq!(chart.series_count(), 1);
        assert!(chart.has_series("metric1"));

        // Add second series
        let idx = chart.add_series("metric2");
        assert_eq!(idx, 1);
        assert_eq!(chart.series_count(), 2);

        // Adding same series again returns existing index
        let idx = chart.add_series("metric1");
        assert_eq!(idx, 0);
        assert_eq!(chart.series_count(), 2);

        // Remove a series
        assert!(chart.remove_series("metric1"));
        assert_eq!(chart.series_count(), 1);
        assert!(!chart.has_series("metric1"));
        assert!(chart.has_series("metric2"));

        // Remove non-existent series returns false
        assert!(!chart.remove_series("nonexistent"));
    }

    #[test]
    fn test_multi_series_with_data() {
        let mut chart = ChartState::new("comparison");
        chart.current_time = 100_000;

        let data1 = vec![
            DataPoint::new(50_000, 10.0),
            DataPoint::new(60_000, 20.0),
            DataPoint::new(70_000, 30.0),
        ];

        let data2 = vec![DataPoint::new(50_000, 100.0), DataPoint::new(60_000, 200.0)];

        chart.add_series_with_data("series1", data1);
        chart.add_series_with_data("series2", data2);

        assert_eq!(chart.series_count(), 2);
        assert_eq!(chart.series()[0].data.len(), 3);
        assert_eq!(chart.series()[1].data.len(), 2);
    }

    #[test]
    fn test_multi_series_push_to_series() {
        let mut chart = ChartState::new("comparison");

        chart.add_series("metric1");
        chart.add_series("metric2");

        // Push to specific series
        chart.push_to_series("metric1", DataPoint::new(1000, 10.0));
        chart.push_to_series("metric1", DataPoint::new(2000, 20.0));
        chart.push_to_series("metric2", DataPoint::new(1500, 15.0));

        assert_eq!(chart.series()[0].data.len(), 2);
        assert_eq!(chart.series()[1].data.len(), 1);
    }

    #[test]
    fn test_multi_series_visibility() {
        let mut chart = ChartState::new("comparison");

        chart.add_series("metric1");
        chart.add_series("metric2");

        // All series visible by default
        assert!(chart.series()[0].visible);
        assert!(chart.series()[1].visible);

        // Toggle visibility
        chart.toggle_series_visibility("metric1");
        assert!(!chart.series()[0].visible);
        assert!(chart.series()[1].visible);

        // Toggle back
        chart.toggle_series_visibility("metric1");
        assert!(chart.series()[0].visible);
    }

    #[test]
    fn test_multi_series_colors() {
        let mut chart = ChartState::new("comparison");

        // Each series gets a distinct color
        chart.add_series("s1");
        chart.add_series("s2");
        chart.add_series("s3");

        let c1 = chart.series()[0].color;
        let c2 = chart.series()[1].color;
        let c3 = chart.series()[2].color;

        // Colors should be different
        assert_ne!(c1, c2);
        assert_ne!(c2, c3);
        assert_ne!(c1, c3);
    }

    #[test]
    fn test_multi_series_clear() {
        let mut chart = ChartState::new("comparison");

        chart.add_series("s1");
        chart.add_series("s2");
        assert_eq!(chart.series_count(), 2);

        chart.clear_series();
        assert_eq!(chart.series_count(), 0);
        assert!(!chart.is_multi_series());
    }

    #[test]
    fn test_data_series_visible_data() {
        let mut series = DataSeries::new("test", (1.0, 0.0, 0.0));

        series.push(DataPoint::new(100, 1.0));
        series.push(DataPoint::new(200, 2.0));
        series.push(DataPoint::new(300, 3.0));
        series.push(DataPoint::new(400, 4.0));

        // Query visible range
        let visible: Vec<_> = series.visible_data(150, 350).collect();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].value, 2.0);
        assert_eq!(visible[1].value, 3.0);
    }
}
