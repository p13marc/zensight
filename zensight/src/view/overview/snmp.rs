//! SNMP network overview (#533) — fleet-wide aggregation over the typed
//! `InterfaceTable` docs (#529) and their derived rates (#527).
//!
//! Rankings are **rate-based**: top talkers by current in+out throughput
//! (with utilization against link speed), a hotlist of admin-up/oper-down
//! interfaces, and error hotspots by error/discard *rate* — not the
//! lifetime-counter rankings this view used to compute from hand-parsed
//! `if/<index>/<column>` metric strings (a freshly rebooted device with tiny
//! lifetime counters but a saturated uplink now ranks first).

use std::collections::HashMap;

use iced::widget::{Column, column, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::{IfStatus, InterfaceEntry, InterfaceTable};

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState, empty_state};
use crate::view::dashboard::DeviceState;
use crate::view::formatting::format_rate;
use crate::view::theme;
use crate::view::time_range::TimeRange;
use crate::view::tokens::{font, space};

/// Filter state for the fleet trap/event feed (#578).
///
/// The feed is a ring of every producer-published [`EventRecord`] the GUI has
/// seen (live plus the cold-store backfill), so it needs the same narrowing a
/// log feed does: the structured facets an operator knows (device, severity,
/// notification kind, time) plus free text for the ones they don't.
#[derive(Debug, Clone, Default)]
pub struct EventFilterState {
    /// Whether the filter row and full list are shown (collapsed = top 5).
    pub open: bool,
    /// Free text matched case-insensitively across kind, summary and the
    /// structured fields (names and values).
    pub search: String,
    /// Exact device (`EventRecord::source`) filter.
    pub device: Option<String>,
    pub severity: Option<zensight_common::AlertSeverity>,
    /// Exact notification-kind filter, e.g. `trap/link_down`.
    pub kind: Option<String>,
    pub time_range: TimeRange,
    /// `time_range` resolved against the clock when it was picked — the pure
    /// view never reads the clock itself (same contract as the Logs feed).
    pub range_from: Option<i64>,
}

impl EventFilterState {
    /// Resolve a picked range to an absolute lower bound.
    pub fn set_time_range(&mut self, range: TimeRange, now_ms: i64) {
        self.time_range = range;
        self.range_from = range.window_ms().map(|w| now_ms - w);
    }

    /// Whether anything is narrowing the feed.
    pub fn is_active(&self) -> bool {
        !self.search.trim().is_empty()
            || self.device.is_some()
            || self.severity.is_some()
            || self.kind.is_some()
            || self.time_range != TimeRange::All
    }

    /// Clear every facet.
    pub fn clear(&mut self) {
        let open = self.open;
        *self = Self::default();
        self.open = open;
    }

    /// Whether one record passes the active facets.
    pub fn matches(&self, record: &zensight_common::EventRecord) -> bool {
        if let Some(device) = &self.device
            && &record.source != device
        {
            return false;
        }
        if let Some(severity) = self.severity
            && record.severity != severity
        {
            return false;
        }
        if let Some(kind) = &self.kind
            && &record.kind != kind
        {
            return false;
        }
        if let Some(from) = self.range_from
            && record.timestamp < from
        {
            return false;
        }
        let needle = self.search.trim().to_lowercase();
        if !needle.is_empty() {
            let hit = record.kind.to_lowercase().contains(&needle)
                || record.summary.to_lowercase().contains(&needle)
                || record.source.to_lowercase().contains(&needle)
                || record.fields.iter().any(|(k, v)| {
                    k.to_lowercase().contains(&needle) || v.to_lowercase().contains(&needle)
                });
            if !hit {
                return false;
            }
        }
        true
    }
}

/// One interface, flattened across the fleet.
struct FleetIface<'a> {
    device: &'a str,
    entry: &'a InterfaceEntry,
}

impl FleetIface<'_> {
    fn name(&self) -> String {
        self.entry
            .name
            .clone()
            .unwrap_or_else(|| format!("if{}", self.entry.index))
    }

    /// Current in+out throughput in bytes/s.
    fn total_rate(&self) -> f64 {
        self.entry.rates.in_octets_per_sec.unwrap_or(0.0)
            + self.entry.rates.out_octets_per_sec.unwrap_or(0.0)
    }

    /// Combined error+discard rate per second.
    fn error_rate(&self) -> f64 {
        [
            self.entry.rates.in_errors_per_sec,
            self.entry.rates.out_errors_per_sec,
            self.entry.rates.in_discards_per_sec,
            self.entry.rates.out_discards_per_sec,
        ]
        .iter()
        .flatten()
        .sum()
    }

    /// Utilization of the busier direction vs link speed, when knowable.
    fn util_pct(&self) -> Option<f64> {
        let speed = self.entry.speed_bits.filter(|s| *s > 0)? as f64;
        let max_rate = self
            .entry
            .rates
            .in_octets_per_sec
            .unwrap_or(0.0)
            .max(self.entry.rates.out_octets_per_sec.unwrap_or(0.0));
        Some(max_rate * 8.0 / speed * 100.0)
    }

    fn is_up(&self) -> bool {
        self.entry.oper_status.is_some_and(IfStatus::is_up)
    }

    /// Admin-up but oper-down: somebody expects this link to work.
    fn is_unexpectedly_down(&self) -> bool {
        self.entry.admin_status.is_some_and(IfStatus::is_up)
            && self.entry.oper_status.is_some_and(|s| !s.is_up())
    }
}

/// Everything the SNMP fleet overview reads out of the dashboard state.
///
/// Bundled because these five travel together through `overview_section`
/// into `snmp_overview` and nowhere else — passing them individually made
/// every intermediate signature grow with each SNMP feature.
#[derive(Debug, Clone, Copy)]
pub struct SnmpOverviewData<'a> {
    /// Joined interface docs keyed by device (#529).
    pub interfaces: &'a HashMap<String, InterfaceTable>,
    /// Fleet trap/event ring, newest first (#536).
    pub events: &'a std::collections::VecDeque<zensight_common::EventRecord>,
    /// Filter/search state for that feed (#578).
    pub event_filter: &'a EventFilterState,
    /// Subnet-discovery reports keyed by publishing sensor origin (#579).
    pub discovery: &'a HashMap<String, zensight_common::DiscoveryReport>,
    /// Whether the discovery card is expanded (#579).
    pub discovery_open: bool,
}

/// Render the SNMP network overview.
pub fn snmp_overview<'a>(
    devices: &HashMap<&DeviceId, &DeviceState>,
    data: SnmpOverviewData<'a>,
) -> Element<'a, Message> {
    let SnmpOverviewData {
        interfaces,
        events,
        event_filter,
        discovery,
        discovery_open,
    } = data;
    if devices.is_empty() && interfaces.is_empty() && discovery.is_empty() {
        return empty_state("No SNMP devices available", None);
    }

    let fleet: Vec<FleetIface<'a>> = interfaces
        .values()
        .flat_map(|doc| {
            doc.interfaces.iter().map(|entry| FleetIface {
                device: &doc.device,
                entry,
            })
        })
        .collect();

    let total_interfaces = fleet.len();
    let up_count = fleet.iter().filter(|i| i.is_up()).count();
    let down_count = fleet.iter().filter(|i| i.is_unexpectedly_down()).count();
    let erroring = fleet.iter().filter(|i| i.error_rate() > 0.0).count();
    let throughput: f64 = fleet.iter().map(|i| i.total_rate()).sum();

    let summary_row = row![
        render_stat("Devices", devices.len().max(interfaces.len()).to_string()),
        render_stat("Interfaces", total_interfaces.to_string()),
        render_status_stat("UP", up_count, StatusLedState::Active),
        render_status_stat("DOWN", down_count, StatusLedState::Inactive),
        render_stat("Erroring", erroring.to_string()),
        render_stat("Throughput", format_rate(throughput)),
    ]
    .spacing(space::LG)
    .align_y(Alignment::Center);

    let top_talkers = render_top_talkers(&fleet);
    let down_hotlist = render_down_hotlist(&fleet);
    let error_hotspots = render_error_hotspots(&fleet);
    let trap_feed = render_trap_feed(events, event_filter, devices);

    let mut content = column![summary_row].spacing(space::MD).width(Length::Fill);
    if let Some(card) = render_discovery(discovery, discovery_open) {
        content = content.push(card);
    }
    content
        .push(top_talkers)
        .push(down_hotlist)
        .push(error_hotspots)
        .push(trap_feed)
        .into()
}

/// Discovery proposals (#579): unmonitored responders the sensor's opt-in
/// sweep found (#541). A one-line banner, expandable to the per-device list
/// with a copy button for the proposed `devices[]` snippet. Nothing
/// auto-adds — the operator pastes the snippet into the config.
fn render_discovery<'a>(
    discovery: &'a HashMap<String, zensight_common::DiscoveryReport>,
    open: bool,
) -> Option<Element<'a, Message>> {
    // Union across publishing sensors, deduped by address (two pollers can
    // sweep overlapping subnets), newest report first so its metadata wins.
    let mut reports: Vec<_> = discovery.values().collect();
    reports.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
    let mut seen = std::collections::HashSet::new();
    let proposals: Vec<&zensight_common::DiscoveredDevice> = reports
        .iter()
        .flat_map(|r| r.discovered.iter())
        .filter(|d| seen.insert(d.address.as_str()))
        .collect();
    if proposals.is_empty() {
        return None;
    }

    let toggle = iced::widget::button(
        text(format!(
            "{} unmonitored SNMP device{} found {}",
            proposals.len(),
            if proposals.len() == 1 { "" } else { "s" },
            if open { "▾" } else { "▸" },
        ))
        .size(font::CAPTION),
    )
    .on_press(Message::ToggleSnmpDiscovery)
    .style(iced::widget::button::text);

    let mut card = column![toggle].spacing(space::SM);
    if open {
        let rows: Vec<Element<'a, Message>> = proposals
            .iter()
            .map(|d| {
                let identity = d.sys_name.as_deref().unwrap_or("(no sysName)");
                let profiles = if d.matched_profiles.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", d.matched_profiles.join(", "))
                };
                let creds = d
                    .credentials
                    .as_deref()
                    .map(|c| format!(" [creds: {c}]"))
                    .unwrap_or_default();
                row![
                    text(d.address.clone()).size(font::CAPTION),
                    text(format!("{identity}{profiles}{creds}"))
                        .size(font::CAPTION)
                        .style(|t: &Theme| text::Style {
                            color: Some(theme::colors(t).text_muted()),
                        })
                        .width(Length::Fill),
                    iced::widget::button(text("Copy snippet").size(font::CAPTION))
                        .on_press(Message::CopyText(d.suggested.clone()))
                        .style(iced::widget::button::text),
                ]
                .spacing(space::SM)
                .align_y(Alignment::Center)
                .into()
            })
            .collect();
        card = card.push(Column::with_children(rows).spacing(2));
    }
    Some(card.into())
}

/// Fleet trap/event feed (#536, filters + cross-links #578).
///
/// Collapsed it is the five most recent records plus the loudest senders (a
/// trap-storm tell). Opened it grows a filter row — device / severity / kind
/// / time-range facets and a free-text box — and lists everything that
/// passes, with each row linking to the device view and the device's alerts.
fn render_trap_feed<'a>(
    events: &'a std::collections::VecDeque<zensight_common::EventRecord>,
    filter: &'a EventFilterState,
    devices: &HashMap<&DeviceId, &DeviceState>,
) -> Element<'a, Message> {
    if events.is_empty() {
        return text("No trap/event records")
            .size(font::CAPTION)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into();
    }

    // Top trap emitters over the whole ring (helps spot trap storms) — a
    // facet-narrowed view would hide exactly the storm you are looking for.
    let mut per_device: HashMap<&str, usize> = HashMap::new();
    for record in events {
        *per_device.entry(record.source.as_str()).or_default() += 1;
    }
    let mut emitters: Vec<(&str, usize)> = per_device.into_iter().collect();
    emitters.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let emitters = emitters
        .iter()
        .take(3)
        .map(|(device, count)| format!("{device} ({count})"))
        .collect::<Vec<_>>()
        .join(", ");

    let matched: Vec<&zensight_common::EventRecord> =
        events.iter().filter(|e| filter.matches(e)).collect();

    let heading = if filter.is_active() {
        format!(
            "Recent Traps ({} of {}) — top: {emitters}",
            matched.len(),
            events.len()
        )
    } else {
        format!("Recent Traps ({}) — top: {emitters}", events.len())
    };
    let title = iced::widget::button(
        text(format!("{heading} {}", if filter.open { "▾" } else { "▸" }))
            .size(font::CAPTION)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
    )
    .on_press(Message::ToggleSnmpEventFilters)
    .style(iced::widget::button::text);

    let mut feed = column![title].spacing(space::SM);
    if filter.open {
        feed = feed.push(render_event_filters(events, filter));
    }

    // Collapsed: the newest five. Open: everything matching, bounded so a
    // storm cannot render tens of thousands of rows.
    let shown: Vec<&zensight_common::EventRecord> = if filter.open {
        matched.iter().take(EVENT_FEED_MAX_ROWS).copied().collect()
    } else {
        matched.iter().take(5).copied().collect()
    };
    if shown.is_empty() {
        feed = feed.push(
            text("No records match the filter")
                .size(font::CAPTION)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
        );
        return feed.into();
    }

    let rows: Vec<Element<'a, Message>> = shown
        .iter()
        .map(|record| render_event_row(record, filter.open, devices))
        .collect();
    feed = feed.push(Column::with_children(rows).spacing(2));
    if filter.open && matched.len() > shown.len() {
        feed = feed.push(
            text(format!(
                "showing {} of {} matching records",
                shown.len(),
                matched.len()
            ))
            .size(10)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        );
    }
    feed.into()
}

/// Cap on rendered event rows with the feed open (#578) — the count line
/// below the list says when this bites, so a trap storm is visible rather
/// than silently truncated.
const EVENT_FEED_MAX_ROWS: usize = 200;

/// One feed row. Open, it carries the summary and the two cross-links
/// (#578): the device name opens that SNMP device's view, and "alerts" opens
/// the Alerts view scoped to it.
fn render_event_row<'a>(
    record: &'a zensight_common::EventRecord,
    open: bool,
    devices: &HashMap<&DeviceId, &DeviceState>,
) -> Element<'a, Message> {
    let severity = record.severity;
    // The event carries a device *name*; a drill-down needs the full handle
    // (origin included, #474), so resolve it against the fleet. An event from
    // a device the dashboard has never seen simply is not a link.
    let device_id = devices
        .keys()
        .find(|id| id.source == record.source)
        .map(|id| (*id).clone());

    let source: Element<'a, Message> = match device_id {
        Some(id) => iced::widget::button(text(record.source.clone()).size(font::CAPTION))
            .on_press(Message::SelectDevice(id))
            .style(iced::widget::button::text)
            .into(),
        None => text(record.source.clone()).size(font::CAPTION).into(),
    };

    let mut row = row![
        text(crate::view::formatting::format_timestamp(record.timestamp))
            .size(10)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        source,
        text(record.kind.clone())
            .size(font::CAPTION)
            .style(move |t: &Theme| text::Style {
                color: Some(theme::colors(t).alert_severity(severity)),
            }),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    if open {
        row = row.push(
            text(record.summary.clone())
                .size(font::CAPTION)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                })
                .width(Length::Fill),
        );
        // A record that named the alert it raised or cleared links straight to
        // it (#651). Records that drove no alert transition — and every record
        // written before that field existed — keep the device-scoped pivot,
        // which is the honest link when there is no key to follow.
        row = row.push(match &record.alert_key {
            Some(key) => iced::widget::button(text("alert →").size(10))
                .on_press(Message::OpenAlertForKey {
                    source: record.source.clone(),
                    alert_key: key.clone(),
                })
                .style(iced::widget::button::text),
            None => iced::widget::button(text("alerts →").size(10))
                .on_press(Message::OpenAlertsForSource(record.source.clone()))
                .style(iced::widget::button::text),
        });
    }
    row.into()
}

/// One pick-list entry: an optional facet value plus the label shown for it.
/// `Option` has no `Display`, and the "all" entry needs a name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Facet<T> {
    value: Option<T>,
    label: String,
}

impl<T> Facet<T> {
    fn all(label: &str) -> Self {
        Self {
            value: None,
            label: label.to_string(),
        }
    }
}

impl<T> std::fmt::Display for Facet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// The facet row: device / severity / kind pick-lists built from what the
/// ring actually holds, a time-range picker, a search box and Clear.
fn render_event_filters<'a>(
    events: &'a std::collections::VecDeque<zensight_common::EventRecord>,
    filter: &'a EventFilterState,
) -> Element<'a, Message> {
    use iced::widget::{pick_list, text_input};
    use zensight_common::AlertSeverity;

    // Facet vocabularies come from the ring itself: only values that are
    // actually present can be selected, so no filter can empty the feed by
    // construction.
    let mut devices: Vec<String> = events.iter().map(|e| e.source.clone()).collect();
    devices.sort();
    devices.dedup();
    let mut kinds: Vec<String> = events.iter().map(|e| e.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();

    let device_opts: Vec<Facet<String>> = std::iter::once(Facet::all("All devices"))
        .chain(devices.into_iter().map(|d| Facet {
            label: d.clone(),
            value: Some(d),
        }))
        .collect();
    let kind_opts: Vec<Facet<String>> = std::iter::once(Facet::all("All kinds"))
        .chain(kinds.into_iter().map(|k| Facet {
            label: k.clone(),
            value: Some(k),
        }))
        .collect();
    let severity_opts: Vec<Facet<AlertSeverity>> = std::iter::once(Facet::all("All severities"))
        .chain(
            [
                AlertSeverity::Critical,
                AlertSeverity::Warning,
                AlertSeverity::Info,
            ]
            .into_iter()
            .map(|s| Facet {
                label: s.as_str().to_string(),
                value: Some(s),
            }),
        )
        .collect();

    let selected = |opts: &[Facet<String>], current: &Option<String>| -> Option<Facet<String>> {
        opts.iter().find(|f| &f.value == current).cloned()
    };
    let device_sel = selected(&device_opts, &filter.device);
    let kind_sel = selected(&kind_opts, &filter.kind);
    let severity_sel = severity_opts
        .iter()
        .find(|f| f.value == filter.severity)
        .cloned();

    let facets = row![
        pick_list(device_opts, device_sel, |f: Facet<String>| {
            Message::SetSnmpEventDevice(f.value)
        })
        .text_size(font::CAPTION),
        pick_list(severity_opts, severity_sel, |f: Facet<AlertSeverity>| {
            Message::SetSnmpEventSeverity(f.value)
        })
        .text_size(font::CAPTION),
        pick_list(kind_opts, kind_sel, |f: Facet<String>| {
            Message::SetSnmpEventKind(f.value)
        })
        .text_size(font::CAPTION),
        pick_list(
            TimeRange::ALL.to_vec(),
            Some(filter.time_range),
            Message::SetSnmpEventTimeRange
        )
        .text_size(font::CAPTION),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center)
    .wrap();

    let mut search = row![
        text_input("Search traps…", &filter.search)
            .on_input(Message::SetSnmpEventSearch)
            .size(font::CAPTION)
            .width(Length::Fill),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);
    if filter.is_active() {
        search = search.push(
            iced::widget::button(text("Clear").size(font::CAPTION))
                .on_press(Message::ClearSnmpEventFilters)
                .style(iced::widget::button::text),
        );
    }

    column![facets, search].spacing(space::XS).into()
}

/// Render a stat label and value.
fn render_stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        text(value).size(font::EMPHASIS)
    ]
    .spacing(2)
    .into()
}

/// Render a status stat with LED.
fn render_status_stat<'a>(
    label: &'a str,
    count: usize,
    state: StatusLedState,
) -> Element<'a, Message> {
    let led = StatusLed::new(state).with_size(10.0);

    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        row![led.view(), text(count.to_string()).size(font::EMPHASIS)]
            .spacing(space::XS)
            .align_y(Alignment::Center)
    ]
    .spacing(2)
    .into()
}

/// Top talkers by current in+out rate, with utilization.
fn render_top_talkers<'a>(fleet: &[FleetIface<'a>]) -> Element<'a, Message> {
    let title = text("Top Talkers (current rate)")
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        });

    let mut busy: Vec<&FleetIface<'a>> = fleet.iter().filter(|i| i.total_rate() > 0.0).collect();
    busy.sort_by(|a, b| b.total_rate().total_cmp(&a.total_rate()));

    if busy.is_empty() {
        return column![title, empty_state("No traffic observed yet", None)]
            .spacing(space::XS)
            .into();
    }

    let rows: Vec<Element<'a, Message>> = busy
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, iface)| {
            let util: Element<'a, Message> = match iface.util_pct() {
                Some(pct) => text(format!("{pct:.0}%"))
                    .size(10)
                    .style(move |t: &Theme| text::Style {
                        color: Some(if pct > 90.0 {
                            theme::colors(t).danger()
                        } else if pct > 70.0 {
                            theme::colors(t).warning()
                        } else {
                            theme::colors(t).text_muted()
                        }),
                    })
                    .into(),
                None => text("").size(10).into(),
            };
            row![
                text(format!("{}.", i + 1))
                    .size(10)
                    .width(Length::Fixed(20.0)),
                StatusLed::new(if iface.is_up() {
                    StatusLedState::Active
                } else {
                    StatusLedState::Inactive
                })
                .with_size(8.0)
                .view(),
                text(format!("{}/{}", iface.device, iface.name()))
                    .size(font::CAPTION)
                    .width(Length::Fixed(180.0)),
                text(format!(
                    "In: {}",
                    format_rate(iface.entry.rates.in_octets_per_sec.unwrap_or(0.0))
                ))
                .size(10),
                text(format!(
                    "Out: {}",
                    format_rate(iface.entry.rates.out_octets_per_sec.unwrap_or(0.0))
                ))
                .size(10),
                util,
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center)
            .into()
        })
        .collect();

    column![title, Column::with_children(rows).spacing(space::XS)]
        .spacing(space::SM)
        .into()
}

/// Admin-up interfaces that are oper-down, fleet-wide.
fn render_down_hotlist<'a>(fleet: &[FleetIface<'a>]) -> Element<'a, Message> {
    let mut down: Vec<&FleetIface<'a>> =
        fleet.iter().filter(|i| i.is_unexpectedly_down()).collect();

    if down.is_empty() {
        return text("No unexpectedly-down interfaces")
            .size(font::CAPTION)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).success()),
            })
            .into();
    }
    down.sort_by_key(|i| (i.device.to_string(), i.entry.index));

    let title = text(format!("Down Interfaces ({})", down.len()))
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).danger()),
        });

    let rows: Vec<Element<'a, Message>> = down
        .iter()
        .take(5)
        .map(|iface| {
            row![
                StatusLed::new(StatusLedState::Inactive)
                    .with_size(8.0)
                    .view(),
                text(format!("{}/{}", iface.device, iface.name())).size(font::CAPTION),
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center)
            .into()
        })
        .collect();

    column![title, Column::with_children(rows).spacing(2)]
        .spacing(space::SM)
        .into()
}

/// Error hotspots by current error/discard rate.
fn render_error_hotspots<'a>(fleet: &[FleetIface<'a>]) -> Element<'a, Message> {
    let mut erroring: Vec<&FleetIface<'a>> =
        fleet.iter().filter(|i| i.error_rate() > 0.0).collect();

    if erroring.is_empty() {
        return text("No interface errors")
            .size(font::CAPTION)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).success()),
            })
            .into();
    }
    erroring.sort_by(|a, b| b.error_rate().total_cmp(&a.error_rate()));

    let title = text(format!("Error Hotspots ({})", erroring.len()))
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).warning()),
        });

    let rows: Vec<Element<'a, Message>> = erroring
        .iter()
        .take(5)
        .map(|iface| {
            row![
                text(format!("{}/{}", iface.device, iface.name())).size(font::CAPTION),
                text(format!("{:.1} errs/s", iface.error_rate()))
                    .size(font::CAPTION)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).danger()),
                    })
            ]
            .spacing(space::MD)
            .into()
        })
        .collect();

    column![title, Column::with_children(rows).spacing(2)]
        .spacing(space::SM)
        .into()
}
