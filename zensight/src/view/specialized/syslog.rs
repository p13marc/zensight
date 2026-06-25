//! Syslog event specialized view.
//!
//! Displays log events with severity filtering, search, and real-time streaming.
//! Uses Iced 0.14's table widget for structured log display.

use std::collections::HashMap;

use iced::widget::{Row, column, container, pick_list, row, scrollable, table, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{TelemetryPoint, TelemetryValue};

use crate::message::Message;
use crate::view::components::card;
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_timestamp;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::tokens::space;

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

/// Parsed syslog message. Built from a `TelemetryPoint` via
/// [`syslog_message_from_point`]; the app keeps a rolling buffer of these for
/// the top-level [`logs_view`].
#[derive(Debug, Clone)]
pub struct SyslogMessage {
    timestamp: i64,
    severity: SyslogSeverity,
    facility: String,
    hostname: String,
    app_name: String,
    message: String,
    /// Ingestion provenance (#64): journald vs network vs unix socket.
    source_kind: LogSource,
    /// systemd unit (`_SYSTEMD_UNIT`), journald-only — the per-unit lens.
    unit: Option<String>,
}

impl SyslogMessage {
    /// The originating host (used to filter the buffer per device).
    pub fn host(&self) -> &str {
        &self.hostname
    }

    /// The systemd unit, if this entry came from journald with one.
    pub fn unit(&self) -> Option<&str> {
        self.unit.as_deref()
    }
}

/// Where a log entry was ingested from — drives the per-row provenance badge
/// (#64). journald entries carry far richer structure than network syslog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Journald,
    Unix,
    Network,
}

impl LogSource {
    /// Short badge label.
    pub fn label(self) -> &'static str {
        match self {
            LogSource::Journald => "journald",
            LogSource::Unix => "unix",
            LogSource::Network => "net",
        }
    }

    /// Classify from the point's `source_type` label (journald / unix / addr).
    fn from_source_type(s: &str) -> Self {
        match s {
            "journald" => LogSource::Journald,
            "unix" => LogSource::Unix,
            _ => LogSource::Network,
        }
    }
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
    /// systemd units to show (empty = all) — the journald unit lens (#64).
    pub selected_units: std::collections::HashSet<String>,
    /// App name filter pattern.
    pub app_filter: String,
    /// Message content filter pattern.
    pub message_filter: String,
    /// Whether filters have been modified (need to apply).
    pub modified: bool,
    /// Sensor filter stats.
    pub stats: Option<crate::message::SyslogFilterStatus>,
}

impl SyslogFilterState {
    /// Check if any filters are active.
    pub fn has_active_filters(&self) -> bool {
        self.min_severity.is_some()
            || !self.selected_facilities.is_empty()
            || !self.selected_units.is_empty()
            || !self.app_filter.is_empty()
            || !self.message_filter.is_empty()
    }

    /// Toggle a systemd unit in the unit filter (#64).
    pub fn toggle_unit(&mut self, unit: String) {
        if self.selected_units.contains(&unit) {
            self.selected_units.remove(&unit);
        } else {
            self.selected_units.insert(unit);
        }
        self.modified = true;
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
        self.selected_units.clear();
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

/// Render the syslog event specialized view for one host.
///
/// `host_logs` is the app's rolling log buffer already filtered to this device's
/// host — the full recent stream, so drilling into a syslog sensor shows its
/// history (not just the latest line per facility/severity). Falls back to the
/// latest-per-metric snapshot if the buffer has nothing for this host yet.
pub fn syslog_event_view<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
    host_logs: &[SyslogMessage],
) -> Element<'a, Message> {
    let fallback;
    let messages: &[SyslogMessage] = if host_logs.is_empty() {
        fallback = parse_syslog_messages(state);
        &fallback
    } else {
        host_logs
    };

    let mut content = column![render_header(state, filter_state, messages.len())]
        .spacing(space::MD)
        .padding(space::LG);
    if filter_state.panel_open {
        content = content.push(card(render_filter_panel(messages, filter_state)));
    }
    content = content.push(card(render_severity_summary(messages, filter_state)));
    // Derived rollups (#63/#64): rendered when the logs sensor publishes them.
    if state.metrics.keys().any(|k| k.starts_with("logs/")) {
        content = content.push(card(render_logs_rollup(state)));
    }
    content = content.push(card(render_log_stream(messages, filter_state)));

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Derived log-rollup panel (#64): consumes the sensor's `logs/*` metrics (#63)
/// — error/warning totals, units-in-failure, per-severity volume, the noisiest
/// units, and journald throughput — mirroring the netring RED card pattern.
fn render_logs_rollup(state: &DeviceDetailState) -> Element<'_, Message> {
    let num = |m: &str| -> String {
        match state.metrics.get(m).map(|p| &p.value) {
            Some(TelemetryValue::Counter(c)) => c.to_string(),
            Some(TelemetryValue::Gauge(g)) => format!("{g:.0}"),
            _ => "-".into(),
        }
    };
    let muted = |t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    };
    let line = |label: &str, value: String| -> Element<'_, Message> {
        row![
            text(label.to_string()).size(12).width(Length::Fixed(220.0)),
            text(value).size(12),
        ]
        .spacing(8)
        .into()
    };

    let mut col = column![
        row![icons::log(IconSize::Medium), text("Log Rollups").size(16)]
            .spacing(8)
            .align_y(Alignment::Center),
        line("errors (total)", num("logs/errors_total")),
        line("warnings (total)", num("logs/warnings_total")),
        line("units in failure", num("logs/units_in_failure")),
    ]
    .spacing(6);

    // journald throughput, when present.
    if state.metrics.contains_key("logs/journald/read_total") {
        col = col
            .push(text("journald throughput").size(12).style(muted))
            .push(line("  read (total)", num("logs/journald/read_total")))
            .push(line(
                "  published (total)",
                num("logs/journald/published_total"),
            ))
            .push(line(
                "  dropped (total)",
                num("logs/journald/dropped_total"),
            ));
    }

    // Top noisiest units by message count (from the per-unit series).
    let mut units: Vec<(String, u64)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            let unit = m
                .strip_prefix("logs/by_unit/")?
                .strip_suffix("/messages_total")?;
            let n = match &p.value {
                TelemetryValue::Counter(c) => *c,
                TelemetryValue::Gauge(g) => *g as u64,
                _ => return None,
            };
            Some((unit.to_string(), n))
        })
        .collect();
    if !units.is_empty() {
        units.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        col = col.push(text("by unit (top)").size(12).style(muted));
        for (unit, n) in units.into_iter().take(10) {
            col = col.push(line(&format!("  {unit}"), n.to_string()));
        }
    }

    col.into()
}

/// Top-level **Logs** view: a unified, filterable feed of recent log lines from
/// every syslog/journald source (fed by the app's rolling buffer), independent
/// of any single device. This is the discoverable home for logs.
pub fn logs_view<'a>(
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    // Header: title + count + filter toggle (no per-device back button).
    let has_filters = filter_state.has_active_filters();
    let filter_button = button(
        row![
            icons::toggle(IconSize::Medium),
            text(if has_filters {
                "Filters (active)"
            } else {
                "Filters"
            })
            .size(14)
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleSyslogFilterPanel)
    .style(if has_filters {
        iced::widget::button::primary
    } else {
        iced::widget::button::secondary
    });

    let header = row![
        icons::log(IconSize::Large),
        text("Logs").size(24),
        text(format!("{} buffered", messages.len()))
            .size(13)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        filter_button,
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    let mut content = column![header].spacing(space::MD).padding(space::LG);
    if filter_state.panel_open {
        content = content.push(card(render_filter_panel(messages, filter_state)));
    }
    content = content.push(card(render_severity_summary(messages, filter_state)));
    content = content.push(card(render_log_stream(messages, filter_state)));

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and host info.
fn render_header<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
    message_count: usize,
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
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let title = row![
        icons::toggle(IconSize::Medium),
        text("Sensor Filters").size(16)
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

    // Unit chips (#64): the journald per-unit lens — built from observed units
    // in the current buffer. Hidden entirely when no journald units are seen.
    let mut units: Vec<String> = messages
        .iter()
        .filter_map(|m| m.unit.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    units.sort();
    let unit_row: Element<'_, Message> = if units.is_empty() {
        text("").into()
    } else {
        let mut chips: Vec<Element<'_, Message>> = vec![text("Units:").size(13).into()];
        for unit in units.into_iter().take(50) {
            let is_selected = filter_state.selected_units.contains(&unit);
            let label = unit.clone();
            chips.push(
                button(text(label).size(12))
                    .on_press(Message::ToggleSyslogUnit(unit))
                    .style(if is_selected {
                        iced::widget::button::primary
                    } else {
                        iced::widget::button::secondary
                    })
                    .into(),
            );
        }
        Row::with_children(chips)
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
    };

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
    let apply_button = button(row![text("Apply to Sensor").size(13)].align_y(Alignment::Center))
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
            "Sensor stats: {} received, {} passed ({}%), {} filtered",
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
        unit_row,
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
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let filtered_messages = apply_local_filters(messages, filter_state);

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
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    let title = row![icons::log(IconSize::Medium), text("Log Stream").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let filtered_messages = apply_local_filters(messages, filter_state);

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

    let host_column = table::column(
        text("Host").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            text(msg.hostname)
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

    // Provenance badge (#64): journald / unix / net, so operators see where a
    // line came from at a glance.
    let source_column = table::column(
        text("Src").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            text(msg.source_kind.label())
                .size(10)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                })
                .into()
        },
    );

    // systemd unit (#64): empty for non-journald lines.
    let unit_column = table::column(
        text("Unit").size(11),
        |msg: SyslogMessage| -> Element<'_, Message> {
            text(msg.unit.clone().unwrap_or_else(|| "-".to_string()))
                .size(10)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
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
            source_column,
            host_column,
            facility_column,
            unit_column,
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
    state
        .metrics
        .values()
        .map(|p| syslog_message_from_point(p, &state.device_id.source))
        .collect()
}

/// Build a [`SyslogMessage`] from a syslog `TelemetryPoint`. `source_fallback`
/// is the host to use when the point carries no `hostname` label (the telemetry
/// `source`). Used both by the device view and the app's rolling logs buffer.
pub fn syslog_message_from_point(point: &TelemetryPoint, source_fallback: &str) -> SyslogMessage {
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
        .unwrap_or_else(|| source_fallback.to_string());

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

    // Provenance + journald unit (#64), from the labels the logs sensor sets.
    let source_kind = point
        .labels
        .get("source_type")
        .map(|s| LogSource::from_source_type(s))
        .unwrap_or(LogSource::Network);
    let unit = point
        .labels
        .get("sd.journald.unit")
        .filter(|u| !u.is_empty())
        .cloned();

    SyslogMessage {
        timestamp: point.timestamp,
        severity,
        facility,
        hostname,
        app_name,
        message,
        source_kind,
        unit,
    }
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

            // Unit filter (#64): if any selected, only show those units.
            if !filter_state.selected_units.is_empty()
                && !msg
                    .unit
                    .as_ref()
                    .is_some_and(|u| filter_state.selected_units.contains(u))
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

    /// #64: a journald point yields a `Journald` source and a unit; the unit
    /// filter then narrows the stream to the selected unit.
    #[test]
    fn unit_and_source_extracted_and_filtered() {
        use std::collections::HashMap;
        use zensight_common::{TelemetryPoint, TelemetryValue};

        let mk = |unit: &str, src: &str| {
            let mut labels = HashMap::new();
            labels.insert("source_type".to_string(), src.to_string());
            labels.insert("sd.journald.unit".to_string(), unit.to_string());
            let point = TelemetryPoint {
                timestamp: 1,
                source: "host01".into(),
                protocol: Protocol::Syslog,
                metric: "daemon/info".into(),
                value: TelemetryValue::Text("hi".into()),
                labels,
            };
            syslog_message_from_point(&point, "host01")
        };

        let nginx = mk("nginx.service", "journald");
        assert_eq!(nginx.source_kind, LogSource::Journald);
        assert_eq!(nginx.unit(), Some("nginx.service"));

        let msgs = vec![nginx, mk("cron.service", "journald")];
        let mut filter = SyslogFilterState::default();
        filter.toggle_unit("nginx.service".into());
        let shown = apply_local_filters(&msgs, &filter);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].unit(), Some("nginx.service"));
        assert!(filter.has_active_filters());
    }

    #[test]
    fn network_point_has_no_unit() {
        use std::collections::HashMap;
        use zensight_common::{TelemetryPoint, TelemetryValue};
        let point = TelemetryPoint {
            timestamp: 1,
            source: "10.0.0.9".into(),
            protocol: Protocol::Syslog,
            metric: "daemon/info".into(),
            value: TelemetryValue::Text("hi".into()),
            labels: HashMap::new(),
        };
        let m = syslog_message_from_point(&point, "10.0.0.9");
        assert_eq!(m.source_kind, LogSource::Network);
        assert_eq!(m.unit(), None);
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
        let _view = syslog_event_view(&state, &filter_state, &[]);
    }
}
