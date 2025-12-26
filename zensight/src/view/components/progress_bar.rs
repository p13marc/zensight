//! Progress bar widget for disk usage, memory, etc.

use iced::widget::{Column, container, row, text};
use iced::{Alignment, Element, Length, Theme};

use crate::view::theme;

/// Style configuration for a progress bar.
#[derive(Debug, Clone, Copy)]
pub struct ProgressBarStyle {
    /// Warning threshold (0.0 - 1.0).
    pub warning_threshold: f64,
    /// Critical threshold (0.0 - 1.0).
    pub critical_threshold: f64,
    /// Height of the bar.
    pub height: f32,
}

impl Default for ProgressBarStyle {
    fn default() -> Self {
        Self {
            warning_threshold: 0.80,
            critical_threshold: 0.90,
            height: 20.0,
        }
    }
}

/// A progress bar showing used/total with percentage.
pub struct ProgressBar {
    /// Used amount.
    used: f64,
    /// Total amount.
    total: f64,
    /// Label (e.g., mount point "/home").
    label: String,
    /// Unit for display (e.g., "GB", "MB").
    unit: String,
    /// Style configuration.
    style: ProgressBarStyle,
}

impl ProgressBar {
    /// Create a new progress bar.
    pub fn new(used: f64, total: f64, label: impl Into<String>, unit: impl Into<String>) -> Self {
        Self {
            used: used.max(0.0),
            total: total.max(0.0),
            label: label.into(),
            unit: unit.into(),
            style: ProgressBarStyle::default(),
        }
    }

    /// Set custom thresholds.
    pub fn with_thresholds(mut self, warning: f64, critical: f64) -> Self {
        self.style.warning_threshold = warning;
        self.style.critical_threshold = critical;
        self
    }

    /// Set custom height.
    pub fn with_height(mut self, height: f32) -> Self {
        self.style.height = height;
        self
    }

    /// Render the progress bar as an Iced element.
    pub fn view<'a, Message: 'a>(self) -> Element<'a, Message> {
        let ratio = if self.total > 0.0 {
            (self.used / self.total).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let percentage = ratio * 100.0;

        // Determine color based on thresholds
        let bar_color = if ratio >= self.style.critical_threshold {
            iced::Color::from_rgb(0.9, 0.2, 0.2) // Red
        } else if ratio >= self.style.warning_threshold {
            iced::Color::from_rgb(0.9, 0.7, 0.2) // Amber
        } else {
            iced::Color::from_rgb(0.3, 0.6, 0.9) // Blue
        };

        // Label row
        let label_text = text(self.label).size(13);
        let percentage_text = text(format!("{:.0}%", percentage)).size(12);

        let label_row = row![label_text, percentage_text]
            .spacing(10)
            .align_y(Alignment::Center);

        // The bar itself (full width container with filled portion)
        let filled_bar = container(text(""))
            .width(Length::FillPortion((ratio * 100.0) as u16))
            .height(Length::Fixed(self.style.height))
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(bar_color)),
                ..Default::default()
            });

        let empty_bar = container(text(""))
            .width(Length::FillPortion(((1.0 - ratio) * 100.0) as u16))
            .height(Length::Fixed(self.style.height))
            .style(|t: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::colors(t).row_background())),
                ..Default::default()
            });

        let bar = container(row![filled_bar, empty_bar].width(Length::Fill))
            .width(Length::Fill)
            .style(|t: &Theme| container::Style {
                border: iced::Border {
                    color: theme::colors(t).border(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            });

        // Usage text
        let usage_text = text(format!(
            "{:.1}{} / {:.1}{}",
            self.used, self.unit, self.total, self.unit
        ))
        .size(11)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        });

        Column::new()
            .push(label_row)
            .push(bar)
            .push(usage_text)
            .spacing(4)
            .width(Length::Fill)
            .into()
    }
}
