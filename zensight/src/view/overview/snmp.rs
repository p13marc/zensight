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
use crate::view::tokens::{font, space};

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

/// Render the SNMP network overview.
pub fn snmp_overview<'a>(
    devices: &HashMap<&DeviceId, &DeviceState>,
    interfaces: &'a HashMap<String, InterfaceTable>,
) -> Element<'a, Message> {
    if devices.is_empty() && interfaces.is_empty() {
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

    column![summary_row, top_talkers, down_hotlist, error_hotspots]
        .spacing(space::MD)
        .width(Length::Fill)
        .into()
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
