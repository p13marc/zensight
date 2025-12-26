//! Sparkline widget for mini inline charts.

use iced::widget::canvas::{self, Cache, Frame, Geometry, Path, Stroke};
use iced::widget::{Canvas, container};
use iced::{Element, Length, Point, Rectangle, Renderer, Size, Theme};

/// A mini sparkline chart for inline display.
pub struct Sparkline {
    /// Data points (just values, time is implicit).
    data: Vec<f64>,
    /// Width of the sparkline.
    width: f32,
    /// Height of the sparkline.
    height: f32,
    /// Line color.
    color: iced::Color,
    /// Canvas cache for rendering.
    cache: Cache,
}

impl Sparkline {
    /// Create a new sparkline with data.
    pub fn new(data: Vec<f64>) -> Self {
        Self {
            data,
            width: 80.0,
            height: 20.0,
            color: iced::Color::from_rgb(0.3, 0.7, 0.9),
            cache: Cache::new(),
        }
    }

    /// Set the dimensions.
    pub fn with_size(mut self, width: f32, height: f32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Set the line color.
    pub fn with_color(mut self, color: iced::Color) -> Self {
        self.color = color;
        self
    }

    /// Render the sparkline as an Iced element.
    pub fn view<'a, Message: 'a + Clone>(self) -> Element<'a, Message> {
        let sparkline_widget = SparklineWidget {
            data: self.data,
            color: self.color,
            cache: self.cache,
        };

        container(
            Canvas::new(sparkline_widget)
                .width(Length::Fixed(self.width))
                .height(Length::Fixed(self.height)),
        )
        .into()
    }
}

/// Internal canvas widget for sparkline rendering.
struct SparklineWidget {
    data: Vec<f64>,
    color: iced::Color,
    cache: Cache,
}

impl<Message> canvas::Program<Message, Theme, Renderer> for SparklineWidget {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            self.draw_sparkline(frame, bounds.size());
        });

        vec![geometry]
    }
}

impl SparklineWidget {
    fn draw_sparkline(&self, frame: &mut Frame, size: Size) {
        if self.data.is_empty() {
            return;
        }

        let width = size.width;
        let height = size.height;
        let padding = 2.0;

        // Find min/max for scaling
        let min = self.data.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = self.data.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = (max - min).max(0.001); // Avoid division by zero

        let effective_height = height - padding * 2.0;
        let effective_width = width - padding * 2.0;

        // Build the path
        let mut builder = canvas::path::Builder::new();
        let point_count = self.data.len();

        for (i, &value) in self.data.iter().enumerate() {
            let x = if point_count > 1 {
                padding + (i as f32 / (point_count - 1) as f32) * effective_width
            } else {
                padding + effective_width / 2.0
            };

            let normalized = ((value - min) / range) as f32;
            let y = padding + effective_height - normalized * effective_height;

            if i == 0 {
                builder.move_to(Point::new(x, y));
            } else {
                builder.line_to(Point::new(x, y));
            }
        }

        let path = builder.build();

        // Draw the line
        frame.stroke(
            &path,
            Stroke::default().with_color(self.color).with_width(1.5),
        );

        // Draw a small dot at the last point
        if let Some(&last_value) = self.data.last() {
            let x = padding + effective_width;
            let normalized = ((last_value - min) / range) as f32;
            let y = padding + effective_height - normalized * effective_height;

            let dot = Path::circle(Point::new(x, y), 2.0);
            frame.fill(&dot, self.color);
        }
    }
}
