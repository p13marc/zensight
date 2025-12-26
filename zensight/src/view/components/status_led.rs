//! Status LED widget for boolean indicators.

use iced::widget::{container, row, text};
use iced::{Alignment, Element, Length, Theme};

/// State of a status LED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLedState {
    /// Active/Up/True - green.
    Active,
    /// Inactive/Down/False - red.
    Inactive,
    /// Warning/Degraded - amber.
    Warning,
    /// Unknown/Indeterminate - gray.
    Unknown,
}

impl StatusLedState {
    /// Get the color for this state.
    fn color(&self) -> iced::Color {
        match self {
            StatusLedState::Active => iced::Color::from_rgb(0.2, 0.8, 0.3), // Green
            StatusLedState::Inactive => iced::Color::from_rgb(0.9, 0.2, 0.2), // Red
            StatusLedState::Warning => iced::Color::from_rgb(0.9, 0.7, 0.2), // Amber
            StatusLedState::Unknown => iced::Color::from_rgb(0.5, 0.5, 0.5), // Gray
        }
    }

    /// Get a text description for this state.
    pub fn label(&self) -> &'static str {
        match self {
            StatusLedState::Active => "UP",
            StatusLedState::Inactive => "DOWN",
            StatusLedState::Warning => "WARN",
            StatusLedState::Unknown => "???",
        }
    }
}

impl From<bool> for StatusLedState {
    fn from(value: bool) -> Self {
        if value {
            StatusLedState::Active
        } else {
            StatusLedState::Inactive
        }
    }
}

/// A status LED indicator widget.
pub struct StatusLed {
    /// Current state.
    state: StatusLedState,
    /// Optional label text.
    label: Option<String>,
    /// Size of the LED (diameter).
    size: f32,
    /// Whether to show the state text.
    show_state_text: bool,
}

impl StatusLed {
    /// Create a new status LED.
    pub fn new(state: StatusLedState) -> Self {
        Self {
            state,
            label: None,
            size: 12.0,
            show_state_text: false,
        }
    }

    /// Create from a boolean value.
    pub fn from_bool(value: bool) -> Self {
        Self::new(StatusLedState::from(value))
    }

    /// Add a label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the size.
    pub fn with_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// Show the state text (UP/DOWN/etc).
    pub fn with_state_text(mut self) -> Self {
        self.show_state_text = true;
        self
    }

    /// Render the status LED as an Iced element.
    pub fn view<'a, Message: 'a>(self) -> Element<'a, Message> {
        let color = self.state.color();

        let led = container(text(""))
            .width(Length::Fixed(self.size))
            .height(Length::Fixed(self.size))
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(color)),
                border: iced::Border {
                    color: iced::Color::from_rgb(0.3, 0.3, 0.3),
                    width: 1.0,
                    radius: (self.size / 2.0).into(),
                },
                ..Default::default()
            });

        let mut content = row![led].spacing(8).align_y(Alignment::Center);

        if let Some(label) = self.label {
            content = content.push(text(label).size(12));
        }

        if self.show_state_text {
            let state_text = text(self.state.label())
                .size(10)
                .style(move |_theme: &Theme| text::Style { color: Some(color) });
            content = content.push(state_text);
        }

        content.into()
    }
}
