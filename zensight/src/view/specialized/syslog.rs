//! Syslog event specialized view.
//!
//! Displays log events with severity filtering, search, and real-time streaming.

use std::collections::HashMap;

use iced::widget::{Column, Row, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_timestamp;
use crate::view::icons::{self, IconSize};
use crate::view::theme;

/// Syslog severity levels (RFC 5424).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyslogSeverity {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Informational = 6,
    Debug = 7,
}

impl SyslogSeverity {
    fn from_value(val: u64) -> Self {
        match val {
            0 => SyslogSeverity::Emergency,
            1 => SyslogSeverity::Alert,
            2 => SyslogSeverity::Critical,
            3 => SyslogSeverity::Error,
            4 => SyslogSeverity::Warning,
            5 => SyslogSeverity::Notice,
            6 => SyslogSeverity::Informational,
            7 => SyslogSeverity::Debug,
            _ => SyslogSeverity::Debug,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            SyslogSeverity::Emergency => "EMERG",
            SyslogSeverity::Alert => "ALERT",
            SyslogSeverity::Critical => "CRIT",
            SyslogSeverity::Error => "ERR",
            SyslogSeverity::Warning => "WARN",
            SyslogSeverity::Notice => "NOTICE",
            SyslogSeverity::Informational => "INFO",
            SyslogSeverity::Debug => "DEBUG",
        }
    }

    fn color(&self) -> iced::Color {
        match self {
            SyslogSeverity::Emergency | SyslogSeverity::Alert => {
                iced::Color::from_rgb(0.95, 0.2, 0.2) // Bright red
            }
            SyslogSeverity::Critical | SyslogSeverity::Error => {
                iced::Color::from_rgb(0.9, 0.4, 0.3) // Red-orange
            }
            SyslogSeverity::Warning => {
                iced::Color::from_rgb(0.9, 0.7, 0.2) // Amber
            }
            SyslogSeverity::Notice => {
                iced::Color::from_rgb(0.4, 0.7, 0.9) // Blue
            }
            SyslogSeverity::Informational => {
                iced::Color::from_rgb(0.5, 0.8, 0.5) // Green
            }
            SyslogSeverity::Debug => {
                iced::Color::from_rgb(0.6, 0.6, 0.6) // Gray
            }
        }
    }
}

/// Parsed syslog message.
#[derive(Debug, Clone)]
struct SyslogMessage {
    timestamp: i64,
    severity: SyslogSeverity,
    #[allow(dead_code)]
    facility: String,
    #[allow(dead_code)]
    hostname: String,
    app_name: String,
    message: String,
}

/// Render the syslog event specialized view.
pub fn syslog_event_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let severity_summary = render_severity_summary(state);
    let log_stream = render_log_stream(state);

    let content = column![
        header,
        rule::horizontal(1),
        severity_summary,
        rule::horizontal(1),
        log_stream,
    ]
    .spacing(15)
    .padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and host info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let host_name = text(&state.device_id.source).size(24);

    let message_count = state.metrics.len();
    let count_text = text(format!("{} messages", message_count)).size(14);

    row![back_button, protocol_icon, host_name, count_text]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render severity distribution summary.
fn render_severity_summary(state: &DeviceDetailState) -> Element<'_, Message> {
    let messages = parse_syslog_messages(state);

    // Count by severity
    let mut counts: HashMap<u8, usize> = HashMap::new();
    for msg in &messages {
        *counts.entry(msg.severity as u8).or_insert(0) += 1;
    }

    let severities = [
        SyslogSeverity::Emergency,
        SyslogSeverity::Alert,
        SyslogSeverity::Critical,
        SyslogSeverity::Error,
        SyslogSeverity::Warning,
        SyslogSeverity::Notice,
        SyslogSeverity::Informational,
        SyslogSeverity::Debug,
    ];

    let mut severity_items: Vec<Element<'_, Message>> = Vec::new();

    for sev in severities {
        let count = counts.get(&(sev as u8)).copied().unwrap_or(0);
        if count > 0 || sev as u8 <= SyslogSeverity::Warning as u8 {
            let color = sev.color();
            let label = text(format!("{}: {}", sev.label(), count))
                .size(12)
                .style(move |_theme: &Theme| text::Style { color: Some(color) });
            severity_items.push(label.into());
        }
    }

    container(Row::with_children(severity_items).spacing(20))
        .padding(10)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render the log stream.
fn render_log_stream(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::log(IconSize::Medium), text("Log Stream").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let messages = parse_syslog_messages(state);

    if messages.is_empty() {
        return column![
            title,
            text("No log messages received yet...")
                .size(12)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                })
        ]
        .spacing(10)
        .into();
    }

    // Sort by timestamp descending (newest first)
    let mut sorted_messages = messages;
    sorted_messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let mut log_rows = Column::new().spacing(4);

    for msg in sorted_messages.into_iter().take(100) {
        // Limit to 100 most recent
        let severity_color = msg.severity.color();

        let time_text = format_timestamp(msg.timestamp);
        let time = text(time_text).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        });

        let severity = text(msg.severity.label())
            .size(10)
            .style(move |_theme: &Theme| text::Style {
                color: Some(severity_color),
            });

        let app = text(msg.app_name).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).primary()),
        });

        let message_text = if msg.message.len() > 80 {
            format!("{}...", &msg.message[..77])
        } else {
            msg.message
        };
        let message = text(message_text).size(11);

        let row_content = row![time, severity, app, message]
            .spacing(10)
            .align_y(Alignment::Center);

        let is_critical = matches!(
            msg.severity,
            SyslogSeverity::Emergency | SyslogSeverity::Alert | SyslogSeverity::Critical
        );
        let is_error = matches!(msg.severity, SyslogSeverity::Error);
        let is_warning = matches!(msg.severity, SyslogSeverity::Warning);

        let row_container =
            container(row_content)
                .padding(6)
                .width(Length::Fill)
                .style(move |t: &Theme| container::Style {
                    background: Some(iced::Background::Color(
                        theme::colors(t).syslog_row_background(is_critical, is_error, is_warning),
                    )),
                    border: iced::Border {
                        color: theme::colors(t).border_subtle(),
                        width: 1.0,
                        radius: 2.0.into(),
                    },
                    ..Default::default()
                });

        log_rows = log_rows.push(row_container);
    }

    let scroll = scrollable(log_rows)
        .width(Length::Fill)
        .height(Length::Fill);

    column![title, scroll]
        .spacing(10)
        .height(Length::Fill)
        .into()
}

/// Parse syslog messages from metrics.
fn parse_syslog_messages(state: &DeviceDetailState) -> Vec<SyslogMessage> {
    let mut messages = Vec::new();

    for (key, point) in &state.metrics {
        // Expect format: message/<id>/text or similar
        if !key.starts_with("message/") {
            continue;
        }

        let severity = point
            .labels
            .get("severity")
            .and_then(|s| s.parse::<u64>().ok())
            .map(SyslogSeverity::from_value)
            .unwrap_or(SyslogSeverity::Informational);

        let facility = point
            .labels
            .get("facility")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        let hostname = point
            .labels
            .get("hostname")
            .cloned()
            .unwrap_or_else(|| state.device_id.source.clone());

        let app_name = point
            .labels
            .get("app_name")
            .or_else(|| point.labels.get("program"))
            .cloned()
            .unwrap_or_else(|| "-".to_string());

        let message = match &point.value {
            TelemetryValue::Text(s) => s.clone(),
            _ => format!("{:?}", point.value),
        };

        messages.push(SyslogMessage {
            timestamp: point.timestamp,
            severity,
            facility,
            hostname,
            app_name,
            message,
        });
    }

    messages
}

fn section_style(t: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(theme::colors(t).card_background())),
        border: iced::Border {
            color: theme::colors(t).border(),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::Protocol;

    #[test]
    fn test_severity_ordering() {
        assert!(SyslogSeverity::Emergency < SyslogSeverity::Alert);
        assert!(SyslogSeverity::Error < SyslogSeverity::Warning);
    }

    #[test]
    fn test_syslog_view_renders() {
        let device_id = DeviceId::new(Protocol::Syslog, "server01");
        let state = DeviceDetailState::new(device_id);
        let _view = syslog_event_view(&state);
    }
}
