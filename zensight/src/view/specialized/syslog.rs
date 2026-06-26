//! Syslog event specialized view.
//!
//! Displays log events with severity filtering, search, real-time streaming,
//! and a per-entry structured drill-down (#93).

use std::collections::HashMap;

use iced::widget::{Row, column, container, pick_list, row, scrollable, text, text_input};
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
    /// Process id (`pid` label, from `_PID`/`SYSLOG_PID`), when present (#93).
    pid: Option<String>,
    /// systemd MESSAGE_ID — a stable catalog id for this kind of event,
    /// journald-only (#93).
    msg_id: Option<String>,
    /// journald boot id (`sd.journald.boot_id`) — the boot lens (#93).
    boot_id: Option<String>,
    /// Full journald structured fields (`sd.journald.*`), keyed by field suffix
    /// (e.g. `comm`, `exe`, `uid`, `transport`); empty for network/unix lines.
    /// Powers the per-entry structured drill-down (#93).
    structured: std::collections::BTreeMap<String, String>,
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

    /// The journald boot id, if this entry came from journald (#93).
    pub fn boot_id(&self) -> Option<&str> {
        self.boot_id.as_deref()
    }

    /// A stable content key for this row, used to track the expanded drill-down
    /// across live-tail updates (#93). Not a security boundary — just identity.
    fn row_key(&self) -> String {
        format!("{}|{}|{}", self.timestamp, self.app_name, self.message)
    }
}

/// Short human explanation for the well-known systemd MESSAGE_IDs the logs
/// sensor recognizes (#93), mirroring `zensight-sensor-logs`'s known-event
/// catalog (ids verified against systemd's `catalog/systemd.catalog.in`).
/// Returns `None` for any other id — the drill-down then shows the raw id only.
pub fn message_catalog(msg_id: &str) -> Option<&'static str> {
    match msg_id.trim().to_ascii_lowercase().as_str() {
        "fc2e22bc6ee647b6b90729ab34a250b1" => {
            Some("A process crashed and a coredump was captured.")
        }
        "d9b373ed55a64feb8242e02dbe79a49c" => Some("A systemd unit entered the failed state."),
        "d989611b15e44c9dbf31e3c81256e4ed" => {
            Some("systemd-oomd killed a cgroup under memory pressure.")
        }
        "fe6faa94e7774663a0da52717891d8ef" => Some("The kernel OOM killer terminated a process."),
        _ => None,
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
    /// journald boots to show (empty = all) — the boot lens (#93).
    pub selected_boots: std::collections::HashSet<String>,
    /// App name filter pattern.
    pub app_filter: String,
    /// Message content filter pattern.
    pub message_filter: String,
    /// Whether filters have been modified (need to apply).
    pub modified: bool,
    /// Sensor filter stats.
    pub stats: Option<crate::message::SyslogFilterStatus>,
    /// Live-tail paused (#93). When paused, lines newer than `frozen_at` are
    /// hidden so the stream stays still while the operator reads.
    pub paused: bool,
    /// Upper timestamp bound captured when paused; `None` while following.
    pub frozen_at: Option<i64>,
    /// The expanded log row's content key, for the structured drill-down (#93).
    pub expanded_row: Option<String>,
}

impl SyslogFilterState {
    /// Check if any filters are active.
    pub fn has_active_filters(&self) -> bool {
        self.min_severity.is_some()
            || !self.selected_facilities.is_empty()
            || !self.selected_units.is_empty()
            || !self.selected_boots.is_empty()
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

    /// Toggle a journald boot in the boot filter (#93).
    pub fn toggle_boot(&mut self, boot: String) {
        if self.selected_boots.contains(&boot) {
            self.selected_boots.remove(&boot);
        } else {
            self.selected_boots.insert(boot);
        }
        self.modified = true;
    }

    /// Toggle live-tail follow/pause (#93). Pausing freezes the stream at `now`;
    /// resuming clears the freeze so new lines flow again.
    pub fn toggle_follow(&mut self, now: i64) {
        if self.paused {
            self.resume();
        } else {
            self.paused = true;
            self.frozen_at = Some(now);
        }
    }

    /// Resume live tail — "jump to now" (#93).
    pub fn resume(&mut self) {
        self.paused = false;
        self.frozen_at = None;
    }

    /// Toggle the expanded structured drill-down for a log row (#93).
    pub fn toggle_row(&mut self, key: String) {
        if self.expanded_row.as_deref() == Some(key.as_str()) {
            self.expanded_row = None;
        } else {
            self.expanded_row = Some(key);
        }
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
        self.selected_boots.clear();
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

    // Boot chips (#93): the journald boot lens — built from observed boot ids in
    // the current buffer. Hidden when no journald boots are seen. The id is long
    // hex, so the chip shows a short prefix while filtering on the full id.
    let mut boots: Vec<String> = messages
        .iter()
        .filter_map(|m| m.boot_id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    boots.sort();
    let boot_row: Element<'_, Message> = if boots.is_empty() {
        text("").into()
    } else {
        let mut chips: Vec<Element<'_, Message>> = vec![text("Boots:").size(13).into()];
        for boot in boots.into_iter().take(20) {
            let is_selected = filter_state.selected_boots.contains(&boot);
            let short: String = boot.chars().take(8).collect();
            chips.push(
                button(text(short).size(12))
                    .on_press(Message::ToggleSyslogBoot(boot))
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
        boot_row,
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
// Stream column widths, shared by the header and each row so they line up (#93).
const COL_TIME: f32 = 140.0;
const COL_SEV: f32 = 72.0;
const COL_SRC: f32 = 56.0;
const COL_HOST: f32 = 110.0;
const COL_FAC: f32 = 80.0;
const COL_UNIT: f32 = 150.0;
const COL_APP: f32 = 110.0;

fn muted_cell(value: String, width: f32) -> Element<'static, Message> {
    text(value)
        .size(10)
        .width(Length::Fixed(width))
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
}

fn render_log_stream<'a>(
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    // Header bar: title + live-tail follow/pause + jump-to-now (#93).
    let title = row![icons::log(IconSize::Medium), text("Log Stream").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);
    let follow_btn = button(
        text(if filter_state.paused {
            "⏸ Paused"
        } else {
            "● Live"
        })
        .size(12),
    )
    .on_press(Message::ToggleLogFollow)
    .style(if filter_state.paused {
        iced::widget::button::secondary
    } else {
        iced::widget::button::primary
    });
    let mut header_bar = row![
        title,
        iced::widget::Space::new().width(Length::Fill),
        follow_btn
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if filter_state.paused {
        header_bar = header_bar.push(
            button(text("Jump to now ⤓").size(12))
                .on_press(Message::LogsJumpToNow)
                .style(iced::widget::button::secondary),
        );
    }

    let filtered_messages = apply_local_filters(messages, filter_state);

    if filtered_messages.is_empty() {
        let empty_text = if messages.is_empty() {
            "No log messages received yet..."
        } else {
            "No messages match the current filters"
        };
        return column![
            header_bar,
            text(empty_text).size(12).style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
        ]
        .spacing(10)
        .into();
    }

    // Sort by timestamp descending (newest first) and limit to 100.
    let mut sorted_messages = filtered_messages;
    sorted_messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sorted_messages.truncate(100);

    // Column header row, aligned to the per-row widths.
    let head = |label: &'static str, w: f32| -> Element<'static, Message> {
        text(label).size(11).width(Length::Fixed(w)).into()
    };
    let header_row = row![
        head("Time", COL_TIME),
        head("Severity", COL_SEV),
        head("Src", COL_SRC),
        head("Host", COL_HOST),
        head("Facility", COL_FAC),
        head("Unit", COL_UNIT),
        head("App", COL_APP),
        text("Message").size(11),
    ]
    .spacing(8)
    .padding([0, 6]);

    // One clickable row per entry; clicking toggles the structured drill-down.
    let mut list = column![].spacing(1);
    for msg in sorted_messages {
        let key = msg.row_key();
        let expanded = filter_state.expanded_row.as_deref() == Some(key.as_str());
        let severity_color = msg.severity.color();
        let message_text = if msg.message.chars().count() > 100 {
            let head: String = msg.message.chars().take(97).collect();
            format!("{head}...")
        } else {
            msg.message.clone()
        };
        let cells = row![
            muted_cell(format_timestamp(msg.timestamp), COL_TIME),
            text(msg.severity.label())
                .size(10)
                .width(Length::Fixed(COL_SEV))
                .style(move |_t: &Theme| text::Style {
                    color: Some(severity_color),
                }),
            muted_cell(msg.source_kind.label().to_string(), COL_SRC),
            muted_cell(msg.hostname.clone(), COL_HOST),
            muted_cell(msg.facility.clone(), COL_FAC),
            muted_cell(
                msg.unit.clone().unwrap_or_else(|| "-".to_string()),
                COL_UNIT
            ),
            text(msg.app_name.clone())
                .size(10)
                .width(Length::Fixed(COL_APP))
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).primary()),
                }),
            text(message_text).size(11),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        list = list.push(
            button(cells)
                .on_press(Message::ToggleLogRow(key))
                .padding([3, 6])
                .width(Length::Fill)
                .style(iced::widget::button::text),
        );
        if expanded {
            list = list.push(render_log_detail(&msg));
        }
    }

    let scroll = scrollable(list).width(Length::Fill).height(Length::Fill);

    column![header_bar, header_row, scroll]
        .spacing(8)
        .height(Length::Fill)
        .into()
}

/// The expanded per-entry structured drill-down (#93): full message, parsed
/// essentials (pid / unit / boot / MESSAGE_ID + catalog explanation), and every
/// raw journald `sd.journald.*` field.
fn render_log_detail(msg: &SyslogMessage) -> Element<'static, Message> {
    let line = |label: String, value: String| -> Element<'static, Message> {
        row![
            text(label)
                .size(11)
                .width(Length::Fixed(150.0))
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
            text(value).size(11),
        ]
        .spacing(8)
        .into()
    };

    let mut col = column![line("time".into(), format_timestamp(msg.timestamp))].spacing(3);
    col = col.push(line("severity".into(), msg.severity.label().to_string()));
    col = col.push(line("source".into(), msg.source_kind.label().to_string()));
    col = col.push(line("host".into(), msg.hostname.clone()));
    col = col.push(line("facility".into(), msg.facility.clone()));
    col = col.push(line("app".into(), msg.app_name.clone()));
    if let Some(pid) = &msg.pid {
        col = col.push(line("pid".into(), pid.clone()));
    }
    if let Some(unit) = &msg.unit {
        col = col.push(line("unit".into(), unit.clone()));
    }
    if let Some(boot) = &msg.boot_id {
        col = col.push(line("boot".into(), boot.clone()));
    }
    if let Some(id) = &msg.msg_id {
        col = col.push(line("MESSAGE_ID".into(), id.clone()));
        if let Some(explanation) = message_catalog(id) {
            col = col.push(text(explanation).size(11).style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).primary()),
            }));
        }
    }

    col = col
        .push(text("message").size(11).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }))
        .push(text(msg.message.clone()).size(12));

    if !msg.structured.is_empty() {
        col = col.push(
            text("journald fields")
                .size(11)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
        );
        for (k, v) in &msg.structured {
            col = col.push(line(k.clone(), v.clone()));
        }
    }

    container(card(col)).padding([2, 16]).into()
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

    // Richer journald structure for the drill-down (#93): pid, MESSAGE_ID, the
    // boot id, and every `sd.journald.*` field flattened by the logs sensor.
    let nonempty = |k: &str| point.labels.get(k).filter(|v| !v.is_empty()).cloned();
    let pid = nonempty("pid");
    let msg_id = nonempty("msgid");
    let boot_id = nonempty("sd.journald.boot_id");
    let structured: std::collections::BTreeMap<String, String> = point
        .labels
        .iter()
        .filter_map(|(k, v)| {
            let field = k.strip_prefix("sd.journald.")?;
            (!v.is_empty()).then(|| (field.to_string(), v.clone()))
        })
        .collect();

    SyslogMessage {
        timestamp: point.timestamp,
        severity,
        facility,
        hostname,
        app_name,
        message,
        source_kind,
        unit,
        pid,
        msg_id,
        boot_id,
        structured,
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

            // Boot filter (#93): if any selected, only show those boots.
            if !filter_state.selected_boots.is_empty()
                && !msg
                    .boot_id
                    .as_ref()
                    .is_some_and(|b| filter_state.selected_boots.contains(b))
            {
                return false;
            }

            // Live-tail pause (#93): hide lines newer than the freeze instant.
            if let Some(ceiling) = filter_state.frozen_at
                && msg.timestamp > ceiling
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

    /// #93: a journald point's richer structure (pid, MESSAGE_ID, boot id, all
    /// `sd.journald.*` fields) is lifted onto the row for the drill-down.
    #[test]
    fn structured_fields_extracted_for_drilldown() {
        use std::collections::HashMap;
        use zensight_common::{TelemetryPoint, TelemetryValue};
        let mut labels = HashMap::new();
        labels.insert("source_type".into(), "journald".to_string());
        labels.insert("pid".into(), "4242".to_string());
        labels.insert(
            "msgid".into(),
            "fc2e22bc6ee647b6b90729ab34a250b1".to_string(),
        );
        labels.insert("sd.journald.boot_id".into(), "boot-abc".to_string());
        labels.insert("sd.journald.unit".into(), "nginx.service".to_string());
        labels.insert("sd.journald.comm".into(), "nginx".to_string());
        labels.insert("sd.journald.empty".into(), String::new()); // dropped
        let point = TelemetryPoint {
            timestamp: 9,
            source: "host01".into(),
            protocol: Protocol::Syslog,
            metric: "daemon/crit".into(),
            value: TelemetryValue::Text("segfault".into()),
            labels,
        };
        let m = syslog_message_from_point(&point, "host01");
        assert_eq!(m.pid.as_deref(), Some("4242"));
        assert_eq!(m.boot_id(), Some("boot-abc"));
        assert_eq!(
            m.msg_id.as_deref(),
            Some("fc2e22bc6ee647b6b90729ab34a250b1")
        );
        // Structured map carries every non-empty sd.journald.* field by suffix.
        assert_eq!(m.structured.get("comm").map(String::as_str), Some("nginx"));
        assert_eq!(
            m.structured.get("boot_id").map(String::as_str),
            Some("boot-abc")
        );
        assert!(!m.structured.contains_key("empty"));
        // The MESSAGE_ID resolves to its catalog explanation.
        assert!(message_catalog(m.msg_id.as_deref().unwrap()).is_some());
        assert!(message_catalog("deadbeef").is_none());
    }

    /// #93: the boot lens narrows the stream; the live-tail freeze hides lines
    /// newer than the pause instant; resume clears the freeze.
    #[test]
    fn boot_filter_and_live_tail_pause() {
        use std::collections::HashMap;
        use zensight_common::{TelemetryPoint, TelemetryValue};
        let mk = |ts: i64, boot: &str| {
            let mut labels = HashMap::new();
            labels.insert("source_type".into(), "journald".to_string());
            labels.insert("sd.journald.boot_id".into(), boot.to_string());
            let point = TelemetryPoint {
                timestamp: ts,
                source: "h".into(),
                protocol: Protocol::Syslog,
                metric: "daemon/info".into(),
                value: TelemetryValue::Text("x".into()),
                labels,
            };
            syslog_message_from_point(&point, "h")
        };
        let msgs = vec![mk(10, "bootA"), mk(20, "bootB"), mk(30, "bootA")];

        let mut filter = SyslogFilterState::default();
        filter.toggle_boot("bootA".into());
        assert_eq!(apply_local_filters(&msgs, &filter).len(), 2);
        assert!(filter.has_active_filters());

        // Pause at t=15: only the bootA line at t=10 survives (t=30 is newer).
        filter.resume();
        filter.selected_boots.clear();
        filter.toggle_follow(15);
        assert!(filter.paused);
        let shown = apply_local_filters(&msgs, &filter);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].timestamp, 10);

        // Resume ("jump to now") un-freezes the stream.
        filter.resume();
        assert!(!filter.paused);
        assert_eq!(apply_local_filters(&msgs, &filter).len(), 3);
    }

    #[test]
    fn toggle_row_expands_and_collapses() {
        let mut filter = SyslogFilterState::default();
        filter.toggle_row("k1".into());
        assert_eq!(filter.expanded_row.as_deref(), Some("k1"));
        filter.toggle_row("k2".into());
        assert_eq!(filter.expanded_row.as_deref(), Some("k2"));
        filter.toggle_row("k2".into());
        assert!(filter.expanded_row.is_none());
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
