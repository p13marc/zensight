//! Syslog event specialized view.
//!
//! Displays log events with severity filtering, search, and real-time streaming.
//! Uses Iced 0.14's table widget for structured log display.

use std::collections::HashMap;

use iced::widget::{
    Row, column, container, pick_list, row, rule, scrollable, table, text, text_input,
};
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
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "emerg" | "emergency" => Some(SyslogSeverity::Emergency),
            "alert" => Some(SyslogSeverity::Alert),
            "crit" | "critical" => Some(SyslogSeverity::Critical),
            "err" | "error" => Some(SyslogSeverity::Error),
            "warning" | "warn" => Some(SyslogSeverity::Warning),
            "notice" => Some(SyslogSeverity::Notice),
            "info" | "informational" => Some(SyslogSeverity::Informational),
            "debug" => Some(SyslogSeverity::Debug),
            _ => None,
        }
    }

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
    facility: String,
    #[allow(dead_code)]
    hostname: String,
    app_name: String,
    message: String,
}

/// Syslog filter state for the UI.
#[derive(Debug, Clone, Default)]
pub struct SyslogFilterState {
    /// Whether the filter panel is expanded.
    pub panel_open: bool,
    /// Minimum severity level (None = all).
    pub min_severity: Option<u8>,
    /// Facilities to show (empty = all).
    pub selected_facilities: std::collections::HashSet<String>,
    /// App name filter pattern.
    pub app_filter: String,
    /// Message content filter pattern.
    pub message_filter: String,
    /// Whether filters have been modified (need to apply).
    pub modified: bool,
    /// Bridge filter stats.
    pub stats: Option<crate::message::SyslogFilterStatus>,
}

impl SyslogFilterState {
    /// Check if any filters are active.
    pub fn has_active_filters(&self) -> bool {
        self.min_severity.is_some()
            || !self.selected_facilities.is_empty()
            || !self.app_filter.is_empty()
            || !self.message_filter.is_empty()
    }

    /// Set minimum severity.
    pub fn set_min_severity(&mut self, severity: Option<u8>) {
        self.min_severity = severity;
        self.modified = true;
    }

    /// Toggle a facility.
    pub fn toggle_facility(&mut self, facility: String) {
        if self.selected_facilities.contains(&facility) {
            self.selected_facilities.remove(&facility);
        } else {
            self.selected_facilities.insert(facility);
        }
        self.modified = true;
    }

    /// Set app filter.
    pub fn set_app_filter(&mut self, filter: String) {
        self.app_filter = filter;
        self.modified = true;
    }

    /// Set message filter.
    pub fn set_message_filter(&mut self, filter: String) {
        self.message_filter = filter;
        self.modified = true;
    }

    /// Clear all filters.
    pub fn clear(&mut self) {
        self.min_severity = None;
        self.selected_facilities.clear();
        self.app_filter.clear();
        self.message_filter.clear();
        self.modified = true;
    }

    /// Mark as applied (not modified).
    pub fn mark_applied(&mut self) {
        self.modified = false;
    }
}

/// Severity option for pick list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeverityOption {
    pub value: Option<u8>,
    pub label: &'static str,
}

impl std::fmt::Display for SeverityOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

const SEVERITY_OPTIONS: [SeverityOption; 9] = [
    SeverityOption {
        value: None,
        label: "All Severities",
    },
    SeverityOption {
        value: Some(0),
        label: "Emergency+",
    },
    SeverityOption {
        value: Some(1),
        label: "Alert+",
    },
    SeverityOption {
        value: Some(2),
        label: "Critical+",
    },
    SeverityOption {
        value: Some(3),
        label: "Error+",
    },
    SeverityOption {
        value: Some(4),
        label: "Warning+",
    },
    SeverityOption {
        value: Some(5),
        label: "Notice+",
    },
    SeverityOption {
        value: Some(6),
        label: "Info+",
    },
    SeverityOption {
        value: Some(7),
        label: "Debug (all)",
    },
];

/// Render the syslog event specialized view.
pub fn syslog_event_view<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let header = render_header(state, filter_state);
    let filter_panel = if filter_state.panel_open {
        render_filter_panel(state, filter_state)
    } else {
        column![].into()
    };
    let severity_summary = render_severity_summary(state, filter_state);
    let log_stream = render_log_stream(state, filter_state);

    let content = column![
        header,
        rule::horizontal(1),
        filter_panel,
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
fn render_header<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
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

    // Filter toggle button
    let filter_button = {
        let has_filters = filter_state.has_active_filters();
        let icon = icons::toggle(IconSize::Medium);
        let label = if has_filters {
            "Filters (active)"
        } else {
            "Filters"
        };
        button(
            row![icon, text(label).size(14)]
                .spacing(6)
                .align_y(Alignment::Center),
        )
        .on_press(Message::ToggleSyslogFilterPanel)
        .style(if has_filters {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        })
    };

    row![
        back_button,
        protocol_icon,
        host_name,
        count_text,
        filter_button
    ]
    .spacing(15)
    .align_y(Alignment::Center)
    .into()
}

/// Render the filter panel.
fn render_filter_panel<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let title = row![
        icons::toggle(IconSize::Medium),
        text("Bridge Filters").size(16)
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // Severity picker
    let current_severity = SEVERITY_OPTIONS
        .iter()
        .find(|opt| opt.value == filter_state.min_severity)
        .cloned()
        .unwrap_or(SEVERITY_OPTIONS[0].clone());

    let severity_picker = row![
        text("Min Severity:").size(13),
        pick_list(
            SEVERITY_OPTIONS.as_slice(),
            Some(current_severity),
            |opt: SeverityOption| Message::SetSyslogMinSeverity(opt.value)
        )
        .width(Length::Fixed(150.0))
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Facility checkboxes
    let messages = parse_syslog_messages(state);
    let mut facilities: Vec<String> = messages
        .iter()
        .map(|m| m.facility.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    facilities.sort();

    let facility_label = text("Facilities:").size(13);
    let facility_checkboxes: Element<'_, Message> = if facilities.is_empty() {
        text("(none)")
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into()
    } else {
        let mut row_items: Vec<Element<'_, Message>> = Vec::new();
        for facility in facilities {
            let is_selected = filter_state.selected_facilities.is_empty()
                || filter_state.selected_facilities.contains(&facility);
            let facility_label = facility.clone();
            let facility_msg = facility.clone();
            // Use a button as a toggle instead of checkbox
            let btn = button(text(facility_label).size(12))
                .on_press(Message::ToggleSyslogFacility(facility_msg))
                .style(if is_selected {
                    iced::widget::button::primary
                } else {
                    iced::widget::button::secondary
                });
            row_items.push(btn.into());
        }
        Row::with_children(row_items).spacing(8).into()
    };

    let facility_row = row![facility_label, facility_checkboxes]
        .spacing(10)
        .align_y(Alignment::Center);

    // App filter input
    let app_filter_row = row![
        text("App Pattern:").size(13),
        text_input("e.g., systemd-*", &filter_state.app_filter)
            .on_input(Message::SetSyslogAppFilter)
            .size(13)
            .padding(6)
            .width(Length::Fixed(200.0))
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Message filter input
    let msg_filter_row = row![
        text("Message Pattern:").size(13),
        text_input("e.g., error|failed", &filter_state.message_filter)
            .on_input(Message::SetSyslogMessageFilter)
            .size(13)
            .padding(6)
            .width(Length::Fixed(200.0))
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Action buttons
    let apply_button = button(row![text("Apply to Bridge").size(13)].align_y(Alignment::Center))
        .on_press(Message::ApplySyslogFilters)
        .style(if filter_state.modified {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        });

    let clear_button = button(row![text("Clear").size(13)].align_y(Alignment::Center))
        .on_press(Message::ClearSyslogFilters)
        .style(iced::widget::button::secondary);

    let buttons_row = row![apply_button, clear_button].spacing(10);

    // Stats display
    let stats_row: Element<'_, Message> = if let Some(ref stats) = filter_state.stats {
        let passed_pct = if stats.messages_received > 0 {
            (stats.messages_passed as f64 / stats.messages_received as f64 * 100.0) as u32
        } else {
            100
        };
        text(format!(
            "Bridge stats: {} received, {} passed ({}%), {} filtered",
            stats.messages_received, stats.messages_passed, passed_pct, stats.messages_filtered
        ))
        .size(11)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
    } else {
        text("").into()
    };

    let filter_content = column![
        title,
        severity_picker,
        facility_row,
        app_filter_row,
        msg_filter_row,
        buttons_row,
        stats_row,
    ]
    .spacing(12);

    container(filter_content)
        .padding(15)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render severity distribution summary.
fn render_severity_summary<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let messages = parse_syslog_messages(state);
    let filtered_messages = apply_local_filters(&messages, filter_state);

    // Count by severity
    let mut counts: HashMap<u8, usize> = HashMap::new();
    for msg in &filtered_messages {
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

    // Show total and filtered count
    let total_count = messages.len();
    let filtered_count = filtered_messages.len();
    let count_label = if total_count != filtered_count {
        text(format!(
            "Showing {} of {} messages",
            filtered_count, total_count
        ))
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
    } else {
        text(format!("{} messages", total_count))
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
    };
    severity_items.push(count_label.into());

    container(Row::with_children(severity_items).spacing(20))
        .padding(10)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render the log stream using Iced 0.14's table widget.
fn render_log_stream<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let title = row![icons::log(IconSize::Medium), text("Log Stream").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let messages = parse_syslog_messages(state);
    let filtered_messages = apply_local_filters(&messages, filter_state);

    if filtered_messages.is_empty() {
        let empty_text = if messages.is_empty() {
            "No log messages received yet..."
        } else {
            "No messages match the current filters"
        };
        return column![
            title,
            text(empty_text).size(12).style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
        ]
        .spacing(10)
        .into();
    }

    // Sort by timestamp descending (newest first) and limit to 100
    let mut sorted_messages = filtered_messages;
    sorted_messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sorted_messages.truncate(100);

    // Define table columns with explicit Element type
    let time_column = table::column(
        text("Time").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            let time_text = format_timestamp(msg.timestamp);
            text(time_text)
                .size(10)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                })
                .into()
        },
    );

    let severity_column = table::column(
        text("Severity").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            let severity_color = msg.severity.color();
            text(msg.severity.label())
                .size(10)
                .style(move |_theme: &Theme| text::Style {
                    color: Some(severity_color),
                })
                .into()
        },
    );

    let facility_column = table::column(
        text("Facility").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            text(msg.facility)
                .size(10)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                })
                .into()
        },
    );

    let app_column = table::column(
        text("App").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            text(msg.app_name)
                .size(10)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).primary()),
                })
                .into()
        },
    );

    let message_column = table::column(
        text("Message").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            let message_text = if msg.message.len() > 100 {
                format!("{}...", &msg.message[..97])
            } else {
                msg.message
            };
            text(message_text).size(11).into()
        },
    );

    // Build the table
    let log_table = table(
        [
            time_column,
            severity_column,
            facility_column,
            app_column,
            message_column,
        ],
        sorted_messages,
    )
    .padding(6)
    .padding_y(4);

    let scroll = scrollable(log_table)
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

    for point in state.metrics.values() {
        // Parse severity from metric path (format: facility/severity)
        let parts: Vec<&str> = point.metric.split('/').collect();
        let (facility, severity) = if parts.len() >= 2 {
            let fac = parts[0].to_string();
            let sev = SyslogSeverity::from_str(parts[1]).unwrap_or(SyslogSeverity::Informational);
            (fac, sev)
        } else {
            // Fallback: try labels
            let fac = point
                .labels
                .get("facility")
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let sev = point
                .labels
                .get("severity")
                .and_then(|s| s.parse::<u64>().ok())
                .map(SyslogSeverity::from_value)
                .or_else(|| {
                    point
                        .labels
                        .get("severity")
                        .and_then(|s| SyslogSeverity::from_str(s))
                })
                .unwrap_or(SyslogSeverity::Informational);
            (fac, sev)
        };

        let hostname = point
            .labels
            .get("hostname")
            .cloned()
            .unwrap_or_else(|| state.device_id.source.clone());

        let app_name = point
            .labels
            .get("app")
            .or_else(|| point.labels.get("app_name"))
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

/// Apply local (UI-side) filters to messages.
fn apply_local_filters(
    messages: &[SyslogMessage],
    filter_state: &SyslogFilterState,
) -> Vec<SyslogMessage> {
    messages
        .iter()
        .filter(|msg| {
            // Severity filter
            if let Some(min_sev) = filter_state.min_severity
                && (msg.severity as u8) > min_sev
            {
                return false;
            }

            // Facility filter (if any selected, only show those)
            if !filter_state.selected_facilities.is_empty()
                && !filter_state.selected_facilities.contains(&msg.facility)
            {
                return false;
            }

            // App name filter (simple substring match)
            if !filter_state.app_filter.is_empty() {
                let pattern = filter_state.app_filter.to_lowercase();
                if !msg.app_name.to_lowercase().contains(&pattern) {
                    return false;
                }
            }

            // Message content filter (simple substring match)
            if !filter_state.message_filter.is_empty() {
                let pattern = filter_state.message_filter.to_lowercase();
                if !msg.message.to_lowercase().contains(&pattern) {
                    return false;
                }
            }

            true
        })
        .cloned()
        .collect()
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
    fn test_severity_from_str() {
        assert_eq!(SyslogSeverity::from_str("err"), Some(SyslogSeverity::Error));
        assert_eq!(
            SyslogSeverity::from_str("ERROR"),
            Some(SyslogSeverity::Error)
        );
        assert_eq!(
            SyslogSeverity::from_str("warning"),
            Some(SyslogSeverity::Warning)
        );
        assert_eq!(
            SyslogSeverity::from_str("info"),
            Some(SyslogSeverity::Informational)
        );
    }

    #[test]
    fn test_filter_state_defaults() {
        let state = SyslogFilterState::default();
        assert!(!state.panel_open);
        assert!(!state.has_active_filters());
    }

    #[test]
    fn test_filter_state_modified() {
        let mut state = SyslogFilterState::default();
        assert!(!state.modified);

        state.set_min_severity(Some(4));
        assert!(state.modified);
        assert!(state.has_active_filters());

        state.mark_applied();
        assert!(!state.modified);
    }

    #[test]
    fn test_syslog_view_renders() {
        let device_id = DeviceId::new(Protocol::Syslog, "server01");
        let state = DeviceDetailState::new(device_id);
        let filter_state = SyslogFilterState::default();
        let _view = syslog_event_view(&state, &filter_state);
    }
}
