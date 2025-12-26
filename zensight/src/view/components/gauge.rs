//! Gauge widget for displaying percentage or bounded values.

use iced::widget::{container, row, text};
use iced::{Alignment, Element, Length, Theme};

use crate::view::theme;

/// Style configuration for a gauge.
#[derive(Debug, Clone, Copy)]
pub struct GaugeStyle {
    /// Warning threshold (0.0 - 1.0).
    pub warning_threshold: f64,
    /// Critical threshold (0.0 - 1.0).
    pub critical_threshold: f64,
    /// Width of the gauge bar.
    pub width: f32,
}

impl Default for GaugeStyle {
    fn default() -> Self {
        Self {
            warning_threshold: 0.75,
            critical_threshold: 0.90,
            width: 150.0,
        }
    }
}

/// A gauge widget showing a value as a filled bar with optional thresholds.
pub struct Gauge {
    /// Current value (0.0 - 1.0 for percentage, or absolute).
    value: f64,
    /// Maximum value (for scaling).
    max: f64,
    /// Label to display.
    label: String,
    /// Unit suffix (e.g., "%", "GB").
    unit: String,
    /// Style configuration.
    style: GaugeStyle,
}

impl Gauge {
    /// Create a new gauge with percentage value (0-100).
    pub fn percentage(value: f64, label: impl Into<String>) -> Self {
        Self {
            value: value.clamp(0.0, 100.0),
            max: 100.0,
            label: label.into(),
            unit: "%".to_string(),
            style: GaugeStyle::default(),
        }
    }

    /// Create a new gauge with absolute values.
    pub fn absolute(
        value: f64,
        max: f64,
        label: impl Into<String>,
        unit: impl Into<String>,
    ) -> Self {
        Self {
            value: value.clamp(0.0, max),
            max,
            label: label.into(),
            unit: unit.into(),
            style: GaugeStyle::default(),
        }
    }

    /// Set custom thresholds.
    pub fn with_thresholds(mut self, warning: f64, critical: f64) -> Self {
        self.style.warning_threshold = warning;
        self.style.critical_threshold = critical;
        self
    }

    /// Set custom width.
    pub fn with_width(mut self, width: f32) -> Self {
        self.style.width = width;
        self
    }

    /// Render the gauge as an Iced element.
    pub fn view<'a, Message: 'a>(self) -> Element<'a, Message> {
        let ratio = if self.max > 0.0 {
            self.value / self.max
        } else {
            0.0
        };

        // Determine color based on thresholds
        let bar_color = if ratio >= self.style.critical_threshold {
            iced::Color::from_rgb(0.9, 0.2, 0.2) // Red
        } else if ratio >= self.style.warning_threshold {
            iced::Color::from_rgb(0.9, 0.7, 0.2) // Amber
        } else {
            iced::Color::from_rgb(0.2, 0.7, 0.4) // Green
        };

        let filled_width = (self.style.width * ratio as f32).max(0.0);
        let empty_width = (self.style.width - filled_width).max(0.0);

        // Create the bar segments
        let filled_bar = container(text(""))
            .width(Length::Fixed(filled_width))
            .height(Length::Fixed(16.0))
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(bar_color)),
                border: iced::Border::default(),
                ..Default::default()
            });

        let empty_bar = container(text(""))
            .width(Length::Fixed(empty_width))
            .height(Length::Fixed(16.0))
            .style(|t: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::colors(t).border_subtle())),
                border: iced::Border::default(),
                ..Default::default()
            });

        let bar = container(row![filled_bar, empty_bar]).style(|t: &Theme| container::Style {
            border: iced::Border {
                color: theme::colors(t).border(),
                width: 1.0,
                radius: 3.0.into(),
            },
            ..Default::default()
        });

        // Format value display
        let value_text = if self.max == 100.0 {
            format!("{:.0}{}", self.value, self.unit)
        } else {
            format!("{:.1}{}", self.value, self.unit)
        };

        let label_text = text(self.label).size(12);
        let value_display = text(value_text).size(12);

        row![label_text, bar, value_display]
            .spacing(10)
            .align_y(Alignment::Center)
            .into()
    }
}
