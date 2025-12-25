//! Time-series chart component using Iced canvas.

use iced::mouse;
use iced::widget::canvas::{self, Cache, Canvas, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use zensight_common::TelemetryValue;

use super::formatting::{format_time_offset, format_value};

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
}

impl TimeWindow {
    /// Get the window duration in milliseconds.
    pub fn duration_ms(&self) -> i64 {
        match self {
            TimeWindow::OneMinute => 60_000,
            TimeWindow::FiveMinutes => 5 * 60_000,
            TimeWindow::FifteenMinutes => 15 * 60_000,
            TimeWindow::OneHour => 60 * 60_000,
        }
    }

    /// Get the display label.
    pub fn label(&self) -> &'static str {
        match self {
            TimeWindow::OneMinute => "1m",
            TimeWindow::FiveMinutes => "5m",
            TimeWindow::FifteenMinutes => "15m",
            TimeWindow::OneHour => "1h",
        }
    }

    /// Get all time window options.
    pub fn all() -> &'static [TimeWindow] {
        &[
            TimeWindow::OneMinute,
            TimeWindow::FiveMinutes,
            TimeWindow::FifteenMinutes,
            TimeWindow::OneHour,
        ]
    }
}

/// State for the time-series chart.
#[derive(Debug)]
pub struct ChartState {
    /// The data points to display.
    data: Vec<DataPoint>,
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
}

impl ChartState {
    /// Create a new chart state.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            data: Vec::new(),
            time_window: TimeWindow::default(),
            title: title.into(),
            cache: Cache::new(),
            min_value: 0.0,
            max_value: 1.0,
            current_time: current_timestamp(),
        }
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

    /// Get visible data points within the current time window.
    fn visible_data(&self) -> impl Iterator<Item = &DataPoint> {
        let cutoff = self.current_time - self.time_window.duration_ms();
        self.data.iter().filter(move |p| p.timestamp >= cutoff)
    }

    /// Recalculate min/max bounds.
    fn recalculate_bounds(&mut self) {
        let cutoff = self.current_time - self.time_window.duration_ms();
        let visible: Vec<_> = self.data.iter().filter(|p| p.timestamp >= cutoff).collect();

        if visible.is_empty() {
            self.min_value = 0.0;
            self.max_value = 1.0;
            return;
        }

        let min_val = visible
            .iter()
            .map(|p| p.value)
            .fold(f64::INFINITY, f64::min);
        let max_val = visible
            .iter()
            .map(|p| p.value)
            .fold(f64::NEG_INFINITY, f64::max);

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

impl<'a> canvas::Program<crate::message::Message> for Chart<'a> {
    type State = ();

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

        // Draw time window label
        let window_label = Text {
            content: format!("Window: {}", self.state.time_window.label()),
            position: Point::new(size.width - padding - 80.0, 10.0),
            color: Color::from_rgb(0.6, 0.6, 0.6),
            size: 12.0.into(),
            ..Text::default()
        };
        frame.fill_text(window_label);

        // Collect visible data
        let visible_data: Vec<_> = self.state.visible_data().collect();

        if visible_data.is_empty() {
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

        // Calculate time range
        let time_end = self.state.current_time;
        let time_start = time_end - self.state.time_window.duration_ms();
        let time_range = (time_end - time_start) as f64;

        // Calculate value range
        let value_min = self.state.min_value;
        let value_max = self.state.max_value;
        let value_range = value_max - value_min;

        // Draw grid lines
        self.draw_grid(
            frame,
            padding,
            chart_width,
            chart_height,
            value_min,
            value_max,
        );

        // Draw the data line
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
            let x =
                padding + ((point.timestamp - time_start) as f64 / time_range) as f32 * chart_width;
            let y = padding + chart_height
                - ((point.value - value_min) / value_range) as f32 * chart_height;

            let dot = Path::circle(Point::new(x, y), 3.0);
            frame.fill(&dot, Color::from_rgb(0.3, 0.8, 1.0));
        }

        // Draw stats
        let stats = self.state.stats();
        self.draw_stats(frame, size, padding, &stats);
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

            // Time label
            let time_offset =
                self.state.time_window.duration_ms() as f64 * (1.0 - i as f64 / num_v_lines as f64);
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

    /// Draw statistics overlay.
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
}
