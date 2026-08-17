//! Syslog event specialized view.
//!
//! Displays log events with severity filtering, search, real-time streaming,
//! and a per-entry structured drill-down (#93).

use std::collections::HashMap;

use iced::widget::{Row, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{ArtifactKind, LogBundleFormat, Protocol, TelemetryPoint, TelemetryValue};

use zensight_common::registry::logs::Subject as LogsSubject;

use crate::message::Message;
use crate::view::components::{Sparkline, card};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_timestamp;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::time_range::TimeRange;
use crate::view::tokens::space;

/// Syslog severity — the one canonical model (#557), re-exported under the name
/// this view has always used. `from_slug`/`from_value`/`label` are its methods;
/// the badge color is [`theme::severity_color`] (a shared helper, since a
/// `Color` can't live in the wire crate).
pub use zensight_common::LogSeverity as SyslogSeverity;

/// Parsed syslog message. Built from a `TelemetryPoint` via
/// [`syslog_message_from_point`]; the app keeps a rolling buffer of these for
/// the top-level [`logs_view`].
#[derive(Debug, Clone)]
pub struct SyslogMessage {
    /// Time-sortable per-line event uid (`<13-ts_ms><12-seq>`, #104) — unique per
    /// record. The buffer-merge dedup + sort tie-break key (#556); empty for
    /// legacy points without a `log.record.uid` label.
    uid: String,
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

    /// The journald invocation id (`_SYSTEMD_INVOCATION_ID`), if present —
    /// the durable per-run identity joining `UnitDetail.invocation_id` (#313).
    pub fn invocation_id(&self) -> Option<&str> {
        self.structured.get("invocation_id").map(String::as_str)
    }

    /// Event time (Unix epoch ms) — for ordering the merged buffer (#107, C9).
    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// The unique, time-sortable record uid (#556). Empty for legacy points that
    /// carried no `log.record.uid` label.
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// The message text — used to de-duplicate cold-store search-back against the
    /// live buffer (#107, C9).
    pub fn message(&self) -> &str {
        &self.message
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
    /// One unit *run* to show (journald invocation id) — the per-run lens
    /// pivoted from the systemd unit drill-down (#313). `None` = all runs.
    pub invocation_id: Option<String>,
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
    /// Whether the "Log statistics" block (severity summary + rollups) is
    /// expanded (#350). Collapsed by default so the log stream is visible
    /// without scrolling; not persisted (per-session).
    pub stats_open: bool,
    /// Whether the by-unit rollup list shows every unit (vs. top-3) (#350).
    pub stats_all_units: bool,
    /// When the feed was opened by pivoting from an alert (#558), the source
    /// rule name — shown as a "filtered from alert <rule>" breadcrumb with a
    /// one-click clear.
    pub alert_pivot: Option<String>,
    /// Selected relative time window (#554). Displayed by the picker; resolved to
    /// [`Self::range_from`] against `now` when applied.
    pub time_range: TimeRange,
    /// Absolute lower time bound (epoch ms) resolved from [`Self::time_range`],
    /// pushed to the events query (`from=`) and the filtered export. `None` = all.
    pub range_from: Option<i64>,
    /// Bundle format the Export button requests (#602). JSONL keeps the
    /// records machine-readable; text is what you paste into a ticket.
    pub export_format: LogBundleFormat,
}

impl SyslogFilterState {
    /// Check if any filters are active.
    pub fn has_active_filters(&self) -> bool {
        self.min_severity.is_some()
            || !self.selected_facilities.is_empty()
            || !self.selected_units.is_empty()
            || !self.selected_boots.is_empty()
            || self.invocation_id.is_some()
            || !self.app_filter.is_empty()
            || !self.message_filter.is_empty()
            || self.time_range != TimeRange::All
    }

    /// Select a relative time window and resolve its absolute lower bound against
    /// `now_ms` (epoch ms). `TimeRange::All` clears the bound.
    pub fn set_time_range(&mut self, range: TimeRange, now_ms: i64) {
        self.time_range = range;
        self.range_from = range.window_ms().map(|w| now_ms - w);
        self.modified = true;
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
        self.invocation_id = None;
        self.app_filter.clear();
        self.message_filter.clear();
        self.alert_pivot = None;
        self.time_range = TimeRange::All;
        self.range_from = None;
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
        content = content.push(card(render_filter_panel(messages, filter_state, None)));
    }
    // Log statistics (#350): severity summary + derived rollups behind ONE
    // collapsible header (default closed) so the log stream is on screen
    // without scrolling. Collapsing loses nothing — expanding shows it all.
    let caret = button(
        text(if filter_state.stats_open {
            "▾"
        } else {
            "▸"
        })
        .size(12),
    )
    .on_press(Message::ToggleLogStatsPanel)
    .padding([2, 8])
    .style(iced::widget::button::text);
    let mut stats = column![crate::view::components::section_header(
        "Log statistics",
        Some(caret.into()),
    )]
    .spacing(space::SM);
    if filter_state.stats_open {
        stats = stats.push(render_severity_summary(messages, filter_state));
        // Derived rollups (#63/#64): rendered when the sensor publishes them.
        //
        // The gate used to be `starts_with("logs/")`, which worked only because
        // the metric names redundantly repeated the producer name (#470). What
        // it was really asking is "is this a registered logs subject?", so it
        // now asks that directly — the registry's parse direction, which is the
        // thing it exists for (RFC 08 §1, #475). The unregistered legacy
        // `<facility>/<severity>` line shape correctly fails it.
        if state
            .metrics
            .keys()
            .any(|k| LogsSubject::parse_metric(k).is_some())
        {
            stats = stats.push(render_logs_rollup(state, filter_state));
        }
    }
    content = content.push(card(stats));
    content = content.push(card(render_log_stream(messages, filter_state)));

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Derived log-rollup panel (#64, compacted in #350): consumes the sensor's
/// `logs/*` metrics (#63) as a KPI tile row (errors / warnings / units-in-
/// failure / journald throughput) instead of one-per-line label rows, plus a
/// top-3 noisiest-units list with a "show all" affordance.
fn render_logs_rollup<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    use crate::view::components::metric_tile;

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

    // KPI tiles in a single wrap-row (the netring/netlink tile pattern).
    let mut tiles = vec![
        metric_tile("errors (total)", num("errors_total")),
        metric_tile("warnings (total)", num("warnings_total")),
        metric_tile("units in failure", num("units_in_failure")),
    ];
    if state.metrics.contains_key("journald/read_total") {
        tiles.push(metric_tile("journald read", num("journald/read_total")));
        tiles.push(metric_tile(
            "journald published",
            num("journald/published_total"),
        ));
        tiles.push(metric_tile(
            "journald dropped",
            num("journald/dropped_total"),
        ));
    }
    let mut col = column![].spacing(space::SM);
    let mut tile_iter = tiles.into_iter().peekable();
    while tile_iter.peek().is_some() {
        let mut tile_row = row![].spacing(space::SM);
        for tile in tile_iter.by_ref().take(4) {
            tile_row = tile_row.push(container(tile).width(Length::FillPortion(1)));
        }
        col = col.push(tile_row);
    }

    // Top noisiest units by message count: top-3 by default, expandable (#350).
    let mut units: Vec<(String, u64)> = state
        .metrics
        .iter()
        .filter_map(|(m, p)| {
            // `by_unit/{unit}/messages_total` (#475).
            let Some(LogsSubject::ByUnitMessagesTotal { unit }) = LogsSubject::parse_metric(m)
            else {
                return None;
            };
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
        let total = units.len();
        let shown = if filter_state.stats_all_units {
            total
        } else {
            3.min(total)
        };
        col = col.push(text("by unit (top)").size(12).style(muted));
        for (unit, n) in units.into_iter().take(shown) {
            col = col.push(
                row![
                    text(format!("  {unit}"))
                        .size(12)
                        .width(Length::Fixed(220.0)),
                    text(n.to_string()).size(12),
                ]
                .spacing(8),
            );
        }
        if total > 3 {
            let label = if filter_state.stats_all_units {
                "Show top 3".to_string()
            } else {
                format!("Show all {total}")
            };
            col = col.push(
                button(text(label).size(12))
                    .on_press(Message::ToggleLogStatsAllUnits)
                    .padding([2, 8])
                    .style(iced::widget::button::text),
            );
        }
    }

    col.into()
}

/// Top-level **Logs** view: a unified, filterable feed of recent log lines from
/// every syslog/journald source (fed by the app's rolling buffer), independent
/// of any single device. This is the discoverable home for logs.
/// Availability of the filtered log-bundle export (#555) for the Logs feed.
/// `Some` only when the logs sensor advertises a `logbundle` artifact kind; the
/// button is disabled while any artifact transfer is already in flight (`busy`).
#[derive(Debug, Clone, Copy)]
pub struct LogExport {
    /// Advertised max line count for one bundle (0 = sensor default).
    pub max_lines: u64,
    /// An artifact transfer is already running — disable the button.
    pub busy: bool,
}

/// Map the active Logs filter onto a `LogBundle` artifact request, so the export
/// carries the same selectors that scope the on-screen feed (#553 parity). Filter
/// dimensions the bundle can't express (facility / boot / unit-run, or more than
/// one selected unit) are dropped here and surfaced by [`log_export_caveats`].
pub fn log_bundle_kind_from_filter(filter_state: &SyslogFilterState) -> ArtifactKind {
    let non_empty = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    // A single selected unit maps cleanly; multiple units can't be expressed in
    // one bundle request, so fall back to no unit filter (caveat surfaced).
    let unit = if filter_state.selected_units.len() == 1 {
        filter_state.selected_units.iter().next().cloned()
    } else {
        None
    };
    ArtifactKind::LogBundle {
        // The selected time-range lower bound (#554), resolved to epoch ms.
        from: filter_state.range_from,
        // The live-tail pause upper bound, when frozen, is the visible window's end.
        to: filter_state.frozen_at,
        pattern: non_empty(&filter_state.message_filter),
        severity_min: filter_state.min_severity.map(|n| n.to_string()),
        unit,
        app: non_empty(&filter_state.app_filter),
        source: None,
        format: filter_state.export_format,
    }
}

/// Active filter dimensions the log bundle can't carry, for an honest "the export
/// is broader than the view" caption. `None` when the export faithfully matches
/// the on-screen filter.
pub fn log_export_caveats(filter_state: &SyslogFilterState) -> Option<String> {
    let mut dropped = Vec::new();
    if !filter_state.selected_facilities.is_empty() {
        dropped.push("facility");
    }
    if !filter_state.selected_boots.is_empty() {
        dropped.push("boot");
    }
    if filter_state.invocation_id.is_some() {
        dropped.push("unit-run");
    }
    if filter_state.selected_units.len() > 1 {
        dropped.push("multiple units");
    }
    (!dropped.is_empty()).then(|| {
        format!(
            "export ignores {} (not expressible in a bundle)",
            dropped.join(", ")
        )
    })
}

pub fn logs_view<'a>(
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
    export: Option<LogExport>,
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
    // Unit-run lens banner (#313): the Logs view was pivoted to one invocation
    // from the systemd unit drill-down — say so, with a way out.
    if let Some(inv) = &filter_state.invocation_id {
        let short: String = inv.chars().take(12).collect();
        content = content.push(card(
            row![
                text(format!("Showing one unit run · invocation {short}…")).size(12),
                button(text("Clear run filter").size(11))
                    .padding([3, 9])
                    .style(iced::widget::button::secondary)
                    .on_press(Message::ClearLogsInvocationFilter),
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center),
        ));
    }
    // Alert-pivot breadcrumb (#558): the feed was opened pre-filtered from an
    // alert — name the source rule, with a one-click clear.
    if let Some(rule) = &filter_state.alert_pivot {
        content = content.push(card(
            row![
                text(format!("Filtered from alert · {rule}")).size(12),
                button(text("Clear").size(11))
                    .padding([3, 9])
                    .style(iced::widget::button::secondary)
                    .on_press(Message::ClearLogsAlertPivot),
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center),
        ));
    }
    if filter_state.panel_open {
        content = content.push(card(render_filter_panel(messages, filter_state, export)));
    }
    content = content.push(card(render_severity_summary(messages, filter_state)));
    content = content.push(card(render_log_stream(messages, filter_state)));

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the logs-facet toolbar: message count + filter toggle. Navigation
/// chrome (Back / protocol icon / host name) is owned by the shared device nav
/// bar that wraps every device view — this used to duplicate it, stacking two
/// Back buttons on the drill-down (#350).
fn render_header<'a>(
    _state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
    message_count: usize,
) -> Element<'a, Message> {
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

    row![count_text, filter_button]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render the filter panel.
fn render_filter_panel<'a>(
    messages: &[SyslogMessage],
    filter_state: &'a SyslogFilterState,
    export: Option<LogExport>,
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

    // Time-range picker (#554): a relative window resolved to a `from=` bound on
    // apply, narrowing both the events query (server-side history depth) and the
    // filtered export.
    let time_range_picker = row![
        text("Time range:").size(13),
        pick_list(
            TimeRange::ALL.as_slice(),
            Some(filter_state.time_range),
            Message::SetLogTimeRange,
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

    // Message filter input (#554): regex, with a subtle hint when the pattern
    // isn't valid regex (we fall back to a substring match rather than error).
    let mut msg_filter_row = row![
        text("Message Pattern:").size(13),
        text_input("e.g., error|failed", &filter_state.message_filter)
            .on_input(Message::SetSyslogMessageFilter)
            .size(13)
            .padding(6)
            .width(Length::Fixed(200.0))
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if message_filter_is_substring_fallback(&filter_state.message_filter) {
        msg_filter_row =
            msg_filter_row.push(text("invalid regex — matching as text").size(11).style(
                |t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                },
            ));
    }

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

    let mut buttons_row = row![apply_button, clear_button].spacing(10);
    // Export the currently-filtered feed as a log bundle (#555): the active
    // selectors ride along as a `LogBundle` request (see `log_bundle_kind_from_filter`),
    // reusing the same @rpc/@blob download path as the per-sensor whole-store button.
    if let Some(exp) = export {
        let label = if exp.max_lines > 0 {
            format!("Export filtered logs (≤{} lines)", exp.max_lines)
        } else {
            "Export filtered logs".to_string()
        };
        let mut export_button = button(row![text(label).size(13)].align_y(Alignment::Center))
            .style(iced::widget::button::secondary);
        if !exp.busy {
            export_button = export_button.on_press(Message::StartArtifact {
                producer: Protocol::Logs.as_str().to_string(),
                kind: log_bundle_kind_from_filter(filter_state),
                target_source: None,
            });
        }
        // Format choice (#602) sits next to the button that uses it: JSONL for
        // a machine, text for a ticket.
        let format_toggle = button(
            text(match filter_state.export_format {
                LogBundleFormat::Jsonl => "as JSONL",
                LogBundleFormat::Text => "as text",
            })
            .size(12),
        )
        .on_press(Message::ToggleLogExportFormat)
        .style(iced::widget::button::text);
        buttons_row = buttons_row.push(export_button).push(format_toggle);
    }

    // Honest caption when the active filter has dimensions the bundle can't carry.
    let export_note: Element<'_, Message> = match export.and(log_export_caveats(filter_state)) {
        Some(note) => text(note)
            .size(11)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into(),
        None => text("").into(),
    };

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
        time_range_picker,
        facility_row,
        unit_row,
        boot_row,
        app_filter_row,
        msg_filter_row,
        buttons_row,
        export_note,
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
/// Number of buckets in the log-rate trend sparkline.
const RATE_BUCKETS: usize = 30;
/// Trend window for the log-rate sparkline (10 minutes).
const RATE_WINDOW_MS: i64 = 10 * 60 * 1000;

/// Derive a message-rate trend (#126): bucket `messages` by timestamp into
/// `buckets` equal slices over the most-recent `window_ms` window (ending at the
/// latest message), returning oldest-first per-bucket counts ready for a
/// [`Sparkline`]. Gives logs a trend without entering the store — derived live
/// from the in-memory buffer. Empty when there are no messages or `buckets == 0`.
fn log_rate_series(messages: &[SyslogMessage], buckets: usize, window_ms: i64) -> Vec<f64> {
    if buckets == 0 || window_ms <= 0 {
        return Vec::new();
    }
    let Some(latest) = messages.iter().map(|m| m.timestamp).max() else {
        return Vec::new();
    };
    let start = latest - window_ms;
    let span = window_ms as f64 / buckets as f64;
    let mut series = vec![0.0_f64; buckets];
    for msg in messages {
        if msg.timestamp <= start || msg.timestamp > latest {
            continue;
        }
        // Offset within (start, latest]; clamp the last bucket so latest lands in
        // bucket `buckets - 1` rather than overflowing.
        let idx = (((msg.timestamp - start) as f64 / span).ceil() as usize)
            .saturating_sub(1)
            .min(buckets - 1);
        series[idx] += 1.0;
    }
    series
}

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
            let color = theme::severity_color(sev);
            let label = text(format!("{}: {}", sev.label(), count))
                .size(12)
                .style(move |_theme: &Theme| text::Style { color: Some(color) });
            severity_items.push(label.into());
        }
    }

    // Show total and filtered count
    let total_count = messages.len();
    let filtered_count = filtered_messages.len();
    // These counts are derived from the *local* recent-lines buffer, not the
    // sensor's lifetime rollup counters below (#557 stats honesty) — label them
    // so the two denominators aren't confused.
    let count_label = if total_count != filtered_count {
        text(format!(
            "Showing {filtered_count} of {total_count} (local buffer)"
        ))
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
    } else {
        text(format!("{total_count} messages (local buffer)"))
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
    };
    severity_items.push(count_label.into());

    // Message-rate trend (#126): a sparkline of volume over the recent window,
    // derived live from the filtered buffer so logs get a trend, not just counts.
    let rate = log_rate_series(&filtered_messages, RATE_BUCKETS, RATE_WINDOW_MS);
    let nonzero = rate.iter().filter(|&&v| v > 0.0).count();
    if nonzero >= 2 {
        let per_min: f64 = rate.iter().sum::<f64>() / (RATE_WINDOW_MS as f64 / 60_000.0);
        let trend = row![
            text("rate (10m)").size(12).style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
            Sparkline::new(rate).with_size(120.0, 20.0).view(),
            text(format!("{per_min:.0}/min")).size(12),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        severity_items.push(trend.into());
    }

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
const COL_HOST: f32 = 96.0;
const COL_FAC: f32 = 80.0;
const COL_UNIT: f32 = 120.0;
const COL_APP: f32 = 100.0;

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
    sorted_messages.sort_by_key(|b| std::cmp::Reverse(b.timestamp));
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
        text("Message").size(11).width(Length::Fill),
    ]
    .spacing(8)
    .padding([0, 6]);

    // One clickable row per entry; clicking toggles the structured drill-down.
    let mut list = column![].spacing(1);
    for msg in sorted_messages {
        let key = msg.row_key();
        let expanded = filter_state.expanded_row.as_deref() == Some(key.as_str());
        let severity_color = theme::severity_color(msg.severity);
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
            text(message_text).size(11).width(Length::Fill),
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
        // Identity pivot (#313): a journald line resolves to its unit *run* —
        // clicking opens the systemd unit drill-down for this host.
        let chip = button(text(unit.clone()).size(11))
            .padding([2, 8])
            .style(iced::widget::button::secondary)
            .on_press(Message::PivotToUnit {
                host: msg.hostname.clone(),
                unit: unit.clone(),
            });
        col = col.push(
            row![
                text("unit")
                    .size(11)
                    .width(Length::Fixed(150.0))
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    }),
                chip,
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }
    if let Some(inv) = msg.invocation_id() {
        col = col.push(line("invocation".into(), inv.to_string()));
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
    // Per-line event keys (#104) carry facility/severity in labels, not the metric
    // (which is now `events/<uid>`). Prefer labels; fall back to the old
    // `facility/severity` metric path for any legacy points still in the buffer.
    let (facility, severity) = {
        let fac_label = point.labels.get("facility").cloned();
        let sev_label = point
            .labels
            .get("severity")
            .and_then(|s| s.parse::<u64>().ok())
            .map(SyslogSeverity::from_value)
            .or_else(|| {
                point
                    .labels
                    .get("severity")
                    .and_then(|s| SyslogSeverity::from_slug(s))
            });
        match (fac_label, sev_label) {
            (Some(fac), Some(sev)) => (fac, sev),
            (fac_opt, sev_opt) => {
                // Legacy fallback: parse from `facility/severity` metric path.
                let parts: Vec<&str> = point.metric.split('/').collect();
                let fac = fac_opt.unwrap_or_else(|| {
                    if parts.len() >= 2 {
                        parts[0].to_string()
                    } else {
                        "unknown".to_string()
                    }
                });
                let sev = sev_opt
                    .or_else(|| {
                        (parts.len() >= 2)
                            .then(|| SyslogSeverity::from_slug(parts[1]))
                            .flatten()
                    })
                    .unwrap_or(SyslogSeverity::Informational);
                (fac, sev)
            }
        }
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

    // The per-line uid (#104/#556) — carried as the `log.record.uid` label.
    let uid = point
        .labels
        .get("log.record.uid")
        .cloned()
        .unwrap_or_default();

    SyslogMessage {
        uid,
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
    // Compile the message-content matcher once (#554), not per row.
    let msg_matcher = MessageMatcher::compile(&filter_state.message_filter);
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

            // Unit-run filter (#313): only lines from this invocation.
            if let Some(inv) = &filter_state.invocation_id
                && msg.invocation_id() != Some(inv.as_str())
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

            // Message content filter (#554): regex (case-insensitive), falling
            // back to substring on an invalid pattern. Compiled once below.
            if !msg_matcher.matches(&msg.message) {
                return false;
            }

            true
        })
        .cloned()
        .collect()
}

/// The compiled message-content filter (#554): a case-insensitive regex, or a
/// substring fallback when the pattern is empty or not valid regex — so the
/// placeholder's promise of `error|failed` regex is real, and a half-typed
/// pattern still filters usefully instead of erroring.
enum MessageMatcher {
    All,
    Regex(Box<regex::Regex>),
    Substring(String),
}

impl MessageMatcher {
    fn compile(pat: &str) -> Self {
        if pat.is_empty() {
            return Self::All;
        }
        match regex::RegexBuilder::new(pat)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
        {
            Ok(re) => Self::Regex(Box::new(re)),
            Err(_) => Self::Substring(pat.to_lowercase()),
        }
    }
    fn matches(&self, msg: &str) -> bool {
        match self {
            Self::All => true,
            Self::Regex(re) => re.is_match(msg),
            Self::Substring(p) => msg.to_lowercase().contains(p),
        }
    }
}

/// Whether the message filter is a valid regex (empty counts as valid) — drives
/// the "filtering as text" hint under the filter input (#554).
pub fn message_filter_is_regex(pat: &str) -> bool {
    !pat.is_empty() && regex::Regex::new(pat).is_ok()
}

/// True when `pat` is non-empty and *not* valid regex (falling back to
/// substring) — the case the hint calls out.
pub fn message_filter_is_substring_fallback(pat: &str) -> bool {
    !pat.is_empty() && regex::Regex::new(pat).is_err()
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

    /// #555: multiple selected units can't be expressed in one bundle request,
    /// so the mapping drops the unit filter and the caveat is surfaced.
    #[test]
    fn export_multi_unit_falls_back_and_is_caveated() {
        let mut f = SyslogFilterState::default();
        f.selected_units.insert("a.service".to_string());
        f.selected_units.insert("b.service".to_string());
        f.selected_facilities.insert("daemon".to_string());

        match log_bundle_kind_from_filter(&f) {
            ArtifactKind::LogBundle { unit, .. } => assert_eq!(unit, None),
            other => panic!("expected LogBundle, got {other:?}"),
        }
        let note = log_export_caveats(&f).expect("caveats present");
        assert!(note.contains("facility"), "note: {note}");
        assert!(note.contains("multiple units"), "note: {note}");
    }

    /// #554: a relative time range resolves to an absolute `from` bound against
    /// `now`, feeds the bundle, and `All` clears it.
    #[test]
    fn time_range_resolves_from_bound() {
        let now = 1_700_000_000_000;
        let mut f = SyslogFilterState::default();
        assert_eq!(f.range_from, None);
        assert!(!f.has_active_filters());

        f.set_time_range(TimeRange::LastHour, now);
        assert_eq!(f.range_from, Some(now - 3_600_000));
        assert!(f.has_active_filters());
        match log_bundle_kind_from_filter(&f) {
            ArtifactKind::LogBundle { from, .. } => assert_eq!(from, Some(now - 3_600_000)),
            other => panic!("expected LogBundle, got {other:?}"),
        }

        f.set_time_range(TimeRange::All, now);
        assert_eq!(f.range_from, None);
    }

    /// #555: a single selected unit maps cleanly and a plain filter has no caveat.
    #[test]
    fn export_single_unit_maps_and_no_caveat() {
        let mut f = SyslogFilterState::default();
        f.selected_units.insert("nginx.service".to_string());
        f.app_filter = "  nginx  ".to_string(); // trimmed
        match log_bundle_kind_from_filter(&f) {
            ArtifactKind::LogBundle { unit, app, .. } => {
                assert_eq!(unit.as_deref(), Some("nginx.service"));
                assert_eq!(app.as_deref(), Some("nginx"));
            }
            other => panic!("expected LogBundle, got {other:?}"),
        }
        assert_eq!(log_export_caveats(&f), None);
    }

    /// #554: the message filter is a case-insensitive regex, falling back to a
    /// substring match (never an error) on an invalid pattern.
    #[test]
    fn message_matcher_regex_and_substring_fallback() {
        // Empty → matches everything.
        assert!(MessageMatcher::compile("").matches("anything"));
        // Valid regex, case-insensitive.
        let m = MessageMatcher::compile("error|failed");
        assert!(m.matches("connection FAILED"));
        assert!(m.matches("disk Error"));
        assert!(!m.matches("all good"));
        // Invalid regex → substring fallback (matches the literal text), flagged.
        assert!(message_filter_is_substring_fallback("err[or"));
        let m = MessageMatcher::compile("err[or");
        assert!(m.matches("an err[or occurred"));
        assert!(!m.matches("fine"));
        // A valid regex is not flagged as a fallback.
        assert!(!message_filter_is_substring_fallback("error|failed"));
    }

    /// Build a bare [`SyslogMessage`] at `ts` (ms) for rate-series tests.
    fn msg_at(ts: i64) -> SyslogMessage {
        SyslogMessage {
            uid: format!("{ts:013}{:012}", 0),
            timestamp: ts,
            severity: SyslogSeverity::Informational,
            facility: "daemon".into(),
            hostname: "host01".into(),
            app_name: "app".into(),
            message: "m".into(),
            source_kind: LogSource::Network,
            unit: None,
            pid: None,
            msg_id: None,
            boot_id: None,
            structured: std::collections::BTreeMap::new(),
        }
    }

    /// #313: the unit-run lens keeps only lines from the exact invocation.
    #[test]
    fn invocation_filter_keeps_only_that_run() {
        let with_inv = |inv: Option<&str>| {
            let mut m = msg_at(1000);
            m.unit = Some("redis.service".into());
            if let Some(inv) = inv {
                m.structured
                    .insert("invocation_id".to_string(), inv.to_string());
            }
            m
        };
        let msgs = vec![
            with_inv(Some("run-a")),
            with_inv(Some("run-b")),
            with_inv(None), // pre-restart / network line with no invocation
        ];
        let mut f = SyslogFilterState {
            invocation_id: Some("run-a".into()),
            ..Default::default()
        };
        let kept = apply_local_filters(&msgs, &f);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].invocation_id(), Some("run-a"));
        assert!(f.has_active_filters());
        // clear() also drops the run lens.
        f.clear();
        assert!(f.invocation_id.is_none());
    }

    /// #126: empty input and degenerate params yield an empty series.
    #[test]
    fn log_rate_series_empty() {
        assert!(log_rate_series(&[], 30, RATE_WINDOW_MS).is_empty());
        assert!(log_rate_series(&[msg_at(1000)], 0, RATE_WINDOW_MS).is_empty());
        assert!(log_rate_series(&[msg_at(1000)], 30, 0).is_empty());
    }

    /// #126: every message counted once, total preserved, latest lands in the
    /// final bucket (window ends at the latest timestamp, inclusive).
    #[test]
    fn log_rate_series_buckets_and_total() {
        // 4 messages on the bucket boundaries of a 4-bucket / 4000ms window
        // ending at ts=4000 (buckets are (0,1000], (1000,2000], …, (3000,4000]).
        let msgs = vec![msg_at(1000), msg_at(2000), msg_at(3000), msg_at(4000)];
        let series = log_rate_series(&msgs, 4, 4000);
        assert_eq!(series.len(), 4);
        assert_eq!(series.iter().sum::<f64>(), 4.0);
        // latest (ts=4000) is in the last bucket.
        assert_eq!(series[3], 1.0);
        // one message per slice → all buckets equal.
        assert_eq!(series, vec![1.0, 1.0, 1.0, 1.0]);
    }

    /// #126: messages clustered near the latest land in the final bucket; older
    /// messages outside the window (ts <= latest - window) are dropped.
    #[test]
    fn log_rate_series_clusters_and_trims() {
        // window = 10_000ms ending at latest=10_000 → start=0; ts=-50 is dropped,
        // the two recent points both fall in the final (10th) bucket.
        let series = log_rate_series(&[msg_at(-50), msg_at(9_900), msg_at(10_000)], 10, 10_000);
        assert_eq!(series.len(), 10);
        assert_eq!(series.iter().sum::<f64>(), 2.0);
        assert_eq!(series[9], 2.0);
        assert_eq!(series[0], 0.0);
    }

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
                protocol: Protocol::Logs,
                metric: "daemon/info".into(),
                value: TelemetryValue::Text("hi".into()),
                labels,
                unit: None,
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
            protocol: Protocol::Logs,
            metric: "daemon/crit".into(),
            value: TelemetryValue::Text("segfault".into()),
            labels,
            unit: None,
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

    /// #104: per-line event points key as `events/<uid>` and carry facility/
    /// severity in labels — `syslog_message_from_point` must read them from the
    /// labels, not parse the (now non-semantic) `events/<uid>` metric path.
    #[test]
    fn per_line_event_reads_facility_severity_from_labels() {
        use std::collections::HashMap;
        use zensight_common::{TelemetryPoint, TelemetryValue};
        let mut labels = HashMap::new();
        labels.insert("facility".into(), "auth".to_string());
        labels.insert("severity".into(), "err".to_string());
        labels.insert("severity_number".into(), "17".to_string());
        labels.insert("severity_text".into(), "ERROR".to_string());
        labels.insert(
            "log.record.uid".into(),
            "0000000000009000000000042".to_string(),
        );
        let point = TelemetryPoint {
            timestamp: 9,
            source: "host01".into(),
            protocol: Protocol::Logs,
            metric: "events/0000000000009000000000042".into(),
            value: TelemetryValue::Text("login failed".into()),
            labels,
            unit: None,
        };
        let m = syslog_message_from_point(&point, "host01");
        assert_eq!(m.facility, "auth");
        assert_eq!(m.severity, SyslogSeverity::Error);
        assert_eq!(m.message, "login failed");
    }

    /// #104: legacy points still keyed `<facility>/<severity>` (no labels) keep
    /// parsing from the metric path so in-flight buffers don't regress.
    #[test]
    fn legacy_facility_severity_metric_still_parses() {
        use std::collections::HashMap;
        use zensight_common::{TelemetryPoint, TelemetryValue};
        let point = TelemetryPoint {
            timestamp: 1,
            source: "host01".into(),
            protocol: Protocol::Logs,
            metric: "kern/warning".into(),
            value: TelemetryValue::Text("low mem".into()),
            labels: HashMap::new(),
            unit: None,
        };
        let m = syslog_message_from_point(&point, "host01");
        assert_eq!(m.facility, "kern");
        assert_eq!(m.severity, SyslogSeverity::Warning);
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
                protocol: Protocol::Logs,
                metric: "daemon/info".into(),
                value: TelemetryValue::Text("x".into()),
                labels,
                unit: None,
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
            protocol: Protocol::Logs,
            metric: "daemon/info".into(),
            value: TelemetryValue::Text("hi".into()),
            labels: HashMap::new(),
            unit: None,
        };
        let m = syslog_message_from_point(&point, "10.0.0.9");
        assert_eq!(m.source_kind, LogSource::Network);
        assert_eq!(m.unit(), None);
    }

    #[test]
    fn test_severity_from_str() {
        assert_eq!(
            SyslogSeverity::from_slug("err"),
            Some(SyslogSeverity::Error)
        );
        assert_eq!(
            SyslogSeverity::from_slug("ERROR"),
            Some(SyslogSeverity::Error)
        );
        assert_eq!(
            SyslogSeverity::from_slug("warning"),
            Some(SyslogSeverity::Warning)
        );
        assert_eq!(
            SyslogSeverity::from_slug("info"),
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
        let device_id = DeviceId::fixture(Protocol::Logs, "server01");
        let state = DeviceDetailState::new(device_id);
        let filter_state = SyslogFilterState::default();
        let _view = syslog_event_view(&state, &filter_state, &[]);
    }
}
