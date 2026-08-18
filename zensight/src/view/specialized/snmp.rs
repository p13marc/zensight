//! SNMP network device specialized view (#530).
//!
//! Built on the typed [`InterfaceTable`] state doc the sensor publishes on
//! `state/snmp/<device>/interfaces` (#529) — no metric-string parsing. The
//! interface table shows **rates** (bytes/s, from the sensor's counter
//! tracker, #527) and utilization against link speed, sortable via the
//! shared [`DataTable`] component, with per-interface drill-down into the
//! history chart and sparklines fed from the raw metric tree.

use iced::widget::{Column as WColumn, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{IfStatus, InterfaceEntry, InterfaceTable, TelemetryValue};

use crate::message::Message;
use crate::view::components::{
    Column as DataColumn, DataTable, Gauge, SortKey, StatusLed, StatusLedState, TableState, card,
    empty_state,
};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_rate;
use crate::view::icons::{self, IconSize};
use crate::view::specialized::metric_sparkline;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// SNMP device detail sub-state (#530): the latest `InterfaceTable` doc off
/// the bus (LWW) plus interface-table UI state.
#[derive(Debug, Default)]
pub struct SnmpDetailState {
    /// The joined interface doc, replaced wholesale on every refresh.
    pub interfaces: Option<InterfaceTable>,
    /// Rendered rows derived from the doc (rebuilt on every doc refresh, so
    /// the `DataTable` can borrow them for the view's lifetime).
    pub rows: Vec<IfaceRow>,
    /// Sort/filter/paging state of the interface table.
    pub table: TableState,
    /// This device's recent trap/event records (#536), newest first.
    pub events: std::collections::VecDeque<zensight_common::EventRecord>,
}

/// Cap on the per-device event ring (#536).
pub const DEVICE_EVENT_RING: usize = 100;

impl SnmpDetailState {
    /// Store a fresh doc (LWW) and rebuild the table rows. `metrics` is the
    /// device's raw metric map, used to pick each interface's drill-down
    /// chart metric.
    pub fn apply_interfaces(
        &mut self,
        table: InterfaceTable,
        metrics: &std::collections::HashMap<String, zensight_common::TelemetryPoint>,
    ) {
        self.rows = table
            .interfaces
            .iter()
            .map(|e| iface_row(metrics, e))
            .collect();
        self.interfaces = Some(table);
    }
}

/// One row of the rendered interface table, pre-joined for the `DataTable`.
#[derive(Debug)]
pub struct IfaceRow {
    name: String,
    alias: Option<String>,
    oper: Option<IfStatus>,
    speed_bits: Option<u64>,
    in_rate: Option<f64>,
    out_rate: Option<f64>,
    util_pct: Option<f64>,
    err_rate: f64,
    /// Raw-tree metric to chart on drill-down, when one exists.
    chart_metric: Option<String>,
}

/// Render the SNMP network device specialized view.
pub fn snmp_device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let system_info = render_system_info(state);
    let interfaces = render_interface_table(state);
    let system_metrics = render_system_metrics(state);

    let content = column![
        header,
        card(system_info),
        card(interfaces),
        card(render_events(state)),
        card(system_metrics),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and device info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(
        row![
            icons::arrow_left(IconSize::Medium),
            text("Back").size(font::BODY)
        ]
        .spacing(space::XS)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let device_name = text(&state.device_id.source).size(font::TITLE);

    let sys_name = get_metric_text_any(state, &["system/name", "system/sysName"])
        .unwrap_or_else(|| "Unknown Device".to_string());
    let sys_name_text = text(sys_name)
        .size(font::BODY)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        });

    // Health status based on sysUpTime presence.
    let status = if uptime_secs(state).is_some() {
        StatusLed::new(StatusLedState::Active).with_label("Healthy")
    } else {
        StatusLed::new(StatusLedState::Warning).with_label("Limited")
    };

    let metric_count = text(format!("{} metrics", state.metrics.len())).size(font::BODY);

    row![
        back_button,
        protocol_icon,
        device_name,
        sys_name_text,
        status.view(),
        metric_count
    ]
    .spacing(space::MD)
    .align_y(Alignment::Center)
    .into()
}

/// Render system information section.
fn render_system_info(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut info_items: Vec<Element<'_, Message>> = Vec::new();

    if let Some(desc) = get_metric_text_any(state, &["system/descr", "system/sysDescr"]) {
        let short_desc = if desc.len() > 60 {
            format!("{}...", &desc[..57])
        } else {
            desc
        };
        info_items.push(
            row![
                text("Description:").size(font::CAPTION),
                text(short_desc).size(font::CAPTION)
            ]
            .spacing(space::SM)
            .into(),
        );
    }

    // sysUpTime — seconds since #527 (converted at the source).
    if let Some(secs) = uptime_secs(state) {
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        let mins = (secs % 3600) / 60;
        let uptime_str = format!("{}d {}h {}m", days, hours, mins);
        // A device that just came (back) up gets flagged: uptime under ten
        // minutes usually means an unplanned reboot worth noticing.
        let rebooted = secs < 600;

        let uptime_text = text(uptime_str)
            .size(font::CAPTION)
            .style(move |t: &Theme| text::Style {
                color: Some(if rebooted {
                    theme::colors(t).warning()
                } else {
                    theme::colors(t).success()
                }),
            });
        let mut r = row![text("Uptime:").size(font::CAPTION), uptime_text].spacing(space::SM);
        if rebooted {
            r = r.push(
                text("rebooted recently")
                    .size(font::CAPTION)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).warning()),
                    }),
            );
        }
        info_items.push(r.into());
    }

    if let Some(contact) = get_metric_text_any(state, &["system/contact", "system/sysContact"])
        && !contact.is_empty()
    {
        info_items.push(
            row![
                text("Contact:").size(font::CAPTION),
                text(contact).size(font::CAPTION)
            ]
            .spacing(space::SM)
            .into(),
        );
    }

    if let Some(location) = get_metric_text_any(state, &["system/location", "system/sysLocation"])
        && !location.is_empty()
    {
        info_items.push(
            row![
                text("Location:").size(font::CAPTION),
                text(location).size(font::CAPTION)
            ]
            .spacing(space::SM)
            .into(),
        );
    }

    if info_items.is_empty() {
        info_items.push(empty_state("Waiting for system information...", None));
    }

    container(WColumn::with_children(info_items).spacing(space::SM))
        .padding(space::SM)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render the sortable interface table from the typed doc (#529).
fn render_interface_table(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::network(IconSize::Medium),
        text("Interfaces").size(font::EMPHASIS)
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    if state.snmp_detail.interfaces.is_none() {
        return column![
            title,
            empty_state(
                "No interface data yet — waiting for the sensor's interface doc",
                None
            )
        ]
        .spacing(space::SM)
        .into();
    }

    let table = DataTable::new(iface_columns(state))
        .searchable(|r: &IfaceRow| format!("{} {}", r.name, r.alias.as_deref().unwrap_or_default()))
        .on_sort(Message::SnmpTableSort)
        .on_filter(Message::SnmpTableFilter)
        .on_more(Message::SnmpTableMore)
        .noun("interfaces")
        .view(&state.snmp_detail.rows, &state.snmp_detail.table);

    column![title, table].spacing(space::SM).into()
}

fn iface_row(
    metrics: &std::collections::HashMap<String, zensight_common::TelemetryPoint>,
    e: &InterfaceEntry,
) -> IfaceRow {
    let in_rate = e.rates.in_octets_per_sec;
    let out_rate = e.rates.out_octets_per_sec;

    // Utilization: the busier direction against link speed (bits vs bits).
    let util_pct = e.speed_bits.filter(|s| *s > 0).and_then(|speed| {
        let max_rate = in_rate.unwrap_or(0.0).max(out_rate.unwrap_or(0.0));
        (in_rate.is_some() || out_rate.is_some()).then(|| (max_rate * 8.0 / speed as f64) * 100.0)
    });

    let err_rate = [
        e.rates.in_errors_per_sec,
        e.rates.out_errors_per_sec,
        e.rates.in_discards_per_sec,
        e.rates.out_discards_per_sec,
    ]
    .iter()
    .flatten()
    .sum();

    IfaceRow {
        name: e.name.clone().unwrap_or_else(|| format!("if{}", e.index)),
        alias: e.alias.clone(),
        oper: e.oper_status,
        speed_bits: e.speed_bits,
        in_rate,
        out_rate,
        util_pct,
        err_rate,
        chart_metric: iface_chart_metric(metrics, e.index),
    }
}

/// The best raw-tree metric to chart for interface `index`: a derived octet
/// rate when one exists, else any octet counter — tolerant of every naming
/// scheme (profiles `if/1/in_octets`, legacy `if/1/ifInOctets`, SMI
/// `if_in_octets/1`).
fn iface_chart_metric(
    metrics: &std::collections::HashMap<String, zensight_common::TelemetryPoint>,
    index: u32,
) -> Option<String> {
    let infix = format!("/{index}/");
    let suffix = format!("/{index}");
    let candidates: Vec<&String> = metrics
        .keys()
        .filter(|k| k.contains(&infix) || k.ends_with(&suffix))
        .filter(|k| k.to_ascii_lowercase().contains("octets"))
        .collect();

    candidates
        .iter()
        .find(|k| k.ends_with(".rate") && k.to_ascii_lowercase().contains("in"))
        .or_else(|| candidates.iter().find(|k| k.ends_with(".rate")))
        .or_else(|| candidates.first())
        .map(|k| (**k).clone())
}

fn led_state(status: Option<IfStatus>) -> StatusLedState {
    match status {
        Some(IfStatus::Up) => StatusLedState::Active,
        Some(IfStatus::Down | IfStatus::NotPresent | IfStatus::LowerLayerDown) => {
            StatusLedState::Inactive
        }
        Some(IfStatus::Testing | IfStatus::Dormant) => StatusLedState::Warning,
        Some(IfStatus::Unknown | IfStatus::Other(_)) | None => StatusLedState::Unknown,
    }
}

fn sort_rank(status: Option<IfStatus>) -> f64 {
    match led_state(status) {
        StatusLedState::Inactive => 0.0,
        StatusLedState::Warning => 1.0,
        StatusLedState::Unknown => 2.0,
        StatusLedState::Active => 3.0,
    }
}

/// "1.0 Gb/s" style link speed.
fn format_speed(bits: u64) -> String {
    let b = bits as f64;
    if b >= 1e9 {
        format!("{:.1} Gb/s", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} Mb/s", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kb/s", b / 1e3)
    } else {
        format!("{bits} b/s")
    }
}

fn iface_columns<'a>(state: &'a DeviceDetailState) -> Vec<DataColumn<'a, IfaceRow, Message>> {
    fn dim(t: &Theme) -> text::Style {
        text::Style {
            color: Some(theme::colors(t).text_muted()),
        }
    }

    vec![
        DataColumn::fixed("status", 70.0, |r: &IfaceRow| {
            StatusLed::new(led_state(r.oper)).with_state_text().view()
        })
        .sortable(|r: &IfaceRow| SortKey::Num(sort_rank(r.oper))),
        DataColumn::fill("name", 3, |r: &IfaceRow| {
            let label: Element<'_, Message> = match &r.alias {
                Some(alias) => column![
                    text(r.name.clone()).size(font::CAPTION),
                    text(alias.clone()).size(font::CAPTION).style(|t: &Theme| {
                        text::Style {
                            color: Some(theme::colors(t).text_dimmed()),
                        }
                    })
                ]
                .into(),
                None => text(r.name.clone()).size(font::CAPTION).into(),
            };
            match &r.chart_metric {
                // Drill-down: open the history chart for this interface.
                Some(metric) => button(label)
                    .on_press(Message::SelectMetricForChart(metric.clone()))
                    .style(iced::widget::button::text)
                    .padding(0)
                    .into(),
                None => label,
            }
        })
        .sortable(|r: &IfaceRow| SortKey::Text(r.name.clone())),
        DataColumn::fixed("speed", 80.0, move |r: &IfaceRow| {
            text(r.speed_bits.map(format_speed).unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .style(dim)
                .into()
        })
        .sortable(|r: &IfaceRow| SortKey::Num(r.speed_bits.unwrap_or(0) as f64)),
        DataColumn::fixed("in", 90.0, |r: &IfaceRow| {
            text(r.in_rate.map(format_rate).unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &IfaceRow| SortKey::Num(r.in_rate.unwrap_or(-1.0))),
        DataColumn::fixed("out", 90.0, |r: &IfaceRow| {
            text(r.out_rate.map(format_rate).unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &IfaceRow| SortKey::Num(r.out_rate.unwrap_or(-1.0))),
        DataColumn::fixed("util", 70.0, |r: &IfaceRow| {
            let Some(pct) = r.util_pct else {
                return text("-").size(font::CAPTION).into();
            };
            text(format!("{pct:.0}%"))
                .size(font::CAPTION)
                .style(move |t: &Theme| text::Style {
                    color: Some(if pct > 90.0 {
                        theme::colors(t).danger()
                    } else if pct > 70.0 {
                        theme::colors(t).warning()
                    } else {
                        theme::colors(t).text()
                    }),
                })
                .into()
        })
        .sortable(|r: &IfaceRow| SortKey::Num(r.util_pct.unwrap_or(-1.0))),
        DataColumn::fixed("errs/s", 70.0, |r: &IfaceRow| {
            if r.err_rate > 0.0 {
                text(format!("{:.1}", r.err_rate))
                    .size(font::CAPTION)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).danger()),
                    })
                    .into()
            } else {
                text("-").size(font::CAPTION).style(dim).into()
            }
        })
        .sortable(|r: &IfaceRow| SortKey::Num(r.err_rate)),
        DataColumn::fixed("trend", 90.0, move |r: &IfaceRow| match &r.chart_metric {
            Some(metric) => metric_sparkline(state, metric),
            None => text("").size(font::CAPTION).into(),
        }),
    ]
}

/// Render the trap/event feed for this device (#536): reverse-chronological
/// translated records off the events plane.
fn render_events(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::chart(IconSize::Medium),
        text("Events").size(font::EMPHASIS)
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    let events = &state.snmp_detail.events;
    if events.is_empty() {
        return column![title, empty_state("No trap/event records yet", None)]
            .spacing(space::SM)
            .into();
    }

    let rows: Vec<Element<'_, Message>> = events
        .iter()
        .take(20)
        .map(|record| {
            let severity = record.severity;
            let when = crate::view::formatting::format_timestamp(record.timestamp);
            let mut fields: Vec<String> = record
                .fields
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "trap_oid" | "snmp_version" | "confirmed"))
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            fields.sort();
            let detail = fields.join("  ");
            let mut row = row![
                text(when).size(font::CAPTION).style(|t: &Theme| {
                    text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    }
                }),
                text(record.kind.clone())
                    .size(font::CAPTION)
                    .style(move |t: &Theme| text::Style {
                        color: Some(theme::colors(t).alert_severity(severity)),
                    }),
                text(detail).size(font::CAPTION).style(|t: &Theme| {
                    text::Style {
                        color: Some(theme::colors(t).text_dimmed()),
                    }
                }),
            ]
            .spacing(space::MD)
            .align_y(Alignment::Center);
            // Only the exact link here (#651): a device-scoped pivot is
            // redundant inside that device's own view.
            if let Some(key) = &record.alert_key {
                row = row.push(
                    iced::widget::button(text("alert →").size(10))
                        .on_press(Message::OpenAlertForKey {
                            source: record.source.clone(),
                            alert_key: key.clone(),
                        })
                        .style(iced::widget::button::text),
                );
            }
            row.into()
        })
        .collect();

    column![title, WColumn::with_children(rows).spacing(space::XS)]
        .spacing(space::SM)
        .into()
}

/// Render system metrics section (CPU, storage, temperatures) with
/// sparklines where history exists.
fn render_system_metrics(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![
        icons::chart(IconSize::Medium),
        text("System Metrics").size(font::EMPHASIS)
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    let mut metrics_content = WColumn::new().spacing(space::SM);
    let mut has_metrics = false;

    // Processor load: profile names (`cpu/<i>/load`) or legacy
    // (`host/hrProcessorLoad`).
    let mut cpu_metrics: Vec<&String> = state
        .metrics
        .keys()
        .filter(|k| {
            (k.starts_with("cpu/") && k.ends_with("/load")) || k.contains("hrProcessorLoad")
        })
        .collect();
    cpu_metrics.sort();
    for name in cpu_metrics {
        if let Some(load) = get_metric_value(state, name) {
            let gauge = Gauge::percentage(load, name.clone()).with_width(200.0);
            metrics_content = metrics_content.push(
                row![gauge.view(), metric_sparkline(state, name)]
                    .spacing(space::MD)
                    .align_y(Alignment::Center),
            );
            has_metrics = true;
        }
    }

    // Storage: profile names `storage/<i>/{used,size}` (or legacy hrStorage*).
    let mut storage_indexes: Vec<String> = state
        .metrics
        .keys()
        .filter_map(|k| {
            k.strip_prefix("storage/")
                .and_then(|rest| rest.strip_suffix("/used"))
                .map(str::to_string)
        })
        .collect();
    storage_indexes.sort();
    for idx in storage_indexes {
        let used = get_metric_value(state, &format!("storage/{idx}/used"));
        let size = get_metric_value(state, &format!("storage/{idx}/size"));
        if let (Some(used), Some(size)) = (used, size)
            && size > 0.0
        {
            let descr = get_metric_text_any(state, &[&format!("storage/{idx}/descr")])
                .unwrap_or_else(|| format!("storage {idx}"));
            let gauge = Gauge::percentage((used / size) * 100.0, descr).with_width(200.0);
            metrics_content = metrics_content.push(gauge.view());
            has_metrics = true;
        }
    }
    if let (Some(used), Some(total)) = (
        get_metric_value(state, "host/hrStorageUsed"),
        get_metric_value(state, "host/hrStorageSize"),
    ) && total > 0.0
    {
        let gauge = Gauge::percentage((used / total) * 100.0, "Memory").with_width(200.0);
        metrics_content = metrics_content.push(gauge.view());
        has_metrics = true;
    }

    // Temperature sensors.
    let mut temp_metrics: Vec<(&String, f64)> = state
        .metrics
        .iter()
        .filter(|(k, _)| k.contains("temp") || k.contains("Temperature"))
        .filter_map(|(k, p)| match &p.value {
            TelemetryValue::Gauge(v) => Some((k, *v)),
            _ => None,
        })
        .collect();
    temp_metrics.sort_by(|a, b| a.0.cmp(b.0));
    for (name, temp) in temp_metrics {
        let short_name = name.split('/').next_back().unwrap_or(name);
        metrics_content = metrics_content.push(
            row![
                text(format!("{}:", short_name)).size(font::CAPTION),
                text(format!("{:.1}°C", temp)).size(font::CAPTION),
                metric_sparkline(state, name)
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center),
        );
        has_metrics = true;
    }

    if !has_metrics {
        metrics_content = metrics_content.push(empty_state("No system metrics available", None));
    }

    column![title, metrics_content].spacing(space::SM).into()
}

// Helper functions

fn uptime_secs(state: &DeviceDetailState) -> Option<u64> {
    ["system/uptime", "system/sysUpTime"]
        .iter()
        .find_map(|m| get_metric_value(state, m))
        .map(|v| v as u64)
}

fn get_metric_value(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Counter(v) => Some(*v as f64),
            TelemetryValue::Gauge(v) => Some(*v),
            _ => None,
        })
}

fn get_metric_text_any(state: &DeviceDetailState, metrics: &[&str]) -> Option<String> {
    metrics.iter().find_map(|metric| {
        state
            .metrics
            .get(*metric)
            .and_then(|point| match &point.value {
                TelemetryValue::Text(s) => Some(s.clone()),
                _ => None,
            })
    })
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
    fn led_state_covers_rfc2863() {
        assert_eq!(led_state(Some(IfStatus::Up)), StatusLedState::Active);
        assert_eq!(led_state(Some(IfStatus::Down)), StatusLedState::Inactive);
        assert_eq!(led_state(Some(IfStatus::Dormant)), StatusLedState::Warning);
        assert_eq!(led_state(Some(IfStatus::Other(9))), StatusLedState::Unknown);
        assert_eq!(led_state(None), StatusLedState::Unknown);
    }

    #[test]
    fn speed_formatting() {
        assert_eq!(format_speed(1_000_000_000), "1.0 Gb/s");
        assert_eq!(format_speed(100_000_000), "100 Mb/s");
        assert_eq!(format_speed(64_000), "64 kb/s");
    }

    #[test]
    fn test_snmp_view_renders() {
        let device_id = DeviceId::fixture(Protocol::Snmp, "router01");
        let state = DeviceDetailState::new(device_id);
        let _view = snmp_device_view(&state);
    }
}
