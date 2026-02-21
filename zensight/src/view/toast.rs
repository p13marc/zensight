//! Toast notification system for user feedback.

use std::time::Instant;

use iced::widget::{column, container, row, text};
use iced::{Alignment, Color, Element, Length, Theme};
use iced_anim::widget::button;

use crate::message::Message;

/// Toast notification severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastSeverity {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastSeverity {
    fn color(&self) -> Color {
        match self {
            ToastSeverity::Info => Color::from_rgb(0.3, 0.5, 0.9),
            ToastSeverity::Success => Color::from_rgb(0.2, 0.7, 0.3),
            ToastSeverity::Warning => Color::from_rgb(0.9, 0.7, 0.0),
            ToastSeverity::Error => Color::from_rgb(0.9, 0.2, 0.2),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ToastSeverity::Info => "Info",
            ToastSeverity::Success => "Success",
            ToastSeverity::Warning => "Warning",
            ToastSeverity::Error => "Error",
        }
    }

    fn duration_secs(&self) -> u64 {
        match self {
            ToastSeverity::Info | ToastSeverity::Success => 5,
            ToastSeverity::Warning => 8,
            ToastSeverity::Error => 10,
        }
    }
}

/// A single toast notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub message: String,
    pub severity: ToastSeverity,
    pub created_at: Instant,
    pub duration_secs: u64,
}

impl Toast {
    /// Whether this toast has expired and should be removed.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= self.duration_secs
    }
}

/// Toast notification state manager.
#[derive(Debug, Default)]
pub struct ToastState {
    toasts: Vec<Toast>,
    next_id: u64,
}

impl ToastState {
    /// Add a new toast notification.
    pub fn push(&mut self, severity: ToastSeverity, message: impl Into<String>) {
        let id = self.next_id;
        self.next_id += 1;
        self.toasts.push(Toast {
            id,
            message: message.into(),
            severity,
            created_at: Instant::now(),
            duration_secs: severity.duration_secs(),
        });

        // Limit to 5 visible toasts
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    /// Remove expired toasts. Returns true if any were removed.
    pub fn cleanup_expired(&mut self) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|t| !t.is_expired());
        self.toasts.len() != before
    }

    /// Dismiss a toast by ID.
    pub fn dismiss(&mut self, id: u64) {
        self.toasts.retain(|t| t.id != id);
    }

    /// Whether there are any active toasts.
    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }
}

/// Render the toast notification overlay.
pub fn toast_overlay<'a>(state: &'a ToastState) -> Element<'a, Message> {
    if state.toasts.is_empty() {
        return column![].into();
    }

    let mut toast_column = column![].spacing(6).width(Length::Fixed(320.0));

    for toast in &state.toasts {
        let severity_color = toast.severity.color();

        let label = text(toast.severity.label())
            .size(11)
            .style(move |_theme: &Theme| text::Style {
                color: Some(severity_color),
            });

        let message = text(&toast.message).size(12);

        let dismiss_btn = button(text("×").size(14))
            .on_press(Message::DismissToast(toast.id))
            .style(iced::widget::button::text)
            .padding([0, 4]);

        let toast_row = row![
            column![label, message].spacing(2).width(Length::Fill),
            dismiss_btn,
        ]
        .spacing(8)
        .align_y(Alignment::Start);

        let toast_container = container(toast_row)
            .padding(10)
            .width(Length::Fill)
            .style(move |theme: &Theme| {
                let bg = match theme {
                    Theme::Dark => Color::from_rgb(0.15, 0.15, 0.18),
                    _ => Color::from_rgb(0.97, 0.97, 0.97),
                };
                container::Style {
                    background: Some(iced::Background::Color(bg)),
                    border: iced::Border {
                        color: severity_color,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..Default::default()
                }
            });

        toast_column = toast_column.push(toast_container);
    }

    toast_column.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toast_state_push_and_dismiss() {
        let mut state = ToastState::default();
        assert!(state.is_empty());

        state.push(ToastSeverity::Info, "Test message");
        assert!(!state.is_empty());
        assert_eq!(state.toasts.len(), 1);
        assert_eq!(state.toasts[0].id, 0);

        state.push(ToastSeverity::Error, "Error message");
        assert_eq!(state.toasts.len(), 2);
        assert_eq!(state.toasts[1].id, 1);

        state.dismiss(0);
        assert_eq!(state.toasts.len(), 1);
        assert_eq!(state.toasts[0].id, 1);
    }

    #[test]
    fn test_toast_state_max_toasts() {
        let mut state = ToastState::default();
        for i in 0..6 {
            state.push(ToastSeverity::Info, format!("Toast {}", i));
        }
        assert_eq!(state.toasts.len(), 5);
        // First toast should have been removed
        assert_eq!(state.toasts[0].id, 1);
    }

    #[test]
    fn test_toast_severity_durations() {
        assert_eq!(ToastSeverity::Info.duration_secs(), 5);
        assert_eq!(ToastSeverity::Success.duration_secs(), 5);
        assert_eq!(ToastSeverity::Warning.duration_secs(), 8);
        assert_eq!(ToastSeverity::Error.duration_secs(), 10);
    }
}
