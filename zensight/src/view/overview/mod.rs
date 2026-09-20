//! Protocol-specific overview sections for the dashboard.
//!
//! These sections aggregate metrics across all devices of each protocol type,
//! providing at-a-glance insights before diving into individual devices.

pub mod containers;
pub mod gnmi;
pub mod modbus;
pub mod netflow;
pub mod netlink;
pub mod netring;
pub mod probe;
pub mod pve;
pub mod snmp;
pub mod sysinfo;
pub mod syslog;

use std::collections::HashMap;

use iced::widget::{Row, column, container, row, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::Protocol;

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::tokens::font;

/// State for the overview section.
#[derive(Debug, Clone)]
pub struct OverviewState {
    /// Which producer's overview is currently selected (None = collapsed).
    /// A name since #1256: a producer without a bespoke overview gets the
    /// generic table.
    pub selected_producer: Option<String>,
    /// Whether the overview section is expanded.
    pub expanded: bool,
}

impl Default for OverviewState {
    fn default() -> Self {
        Self {
            selected_producer: None,
            expanded: true,
        }
    }
}

impl OverviewState {
    /// Select a producer for overview.
    pub fn select_producer(&mut self, producer: String) {
        if self.selected_producer.as_deref() == Some(producer.as_str()) {
            // Toggle off if already selected
            self.selected_producer = None;
        } else {
            self.selected_producer = Some(producer);
            self.expanded = true;
        }
    }

    /// Toggle the expanded state.
    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }
}

/// Render the overview section.
pub fn overview_section<'a>(
    state: &'a OverviewState,
    devices: &'a HashMap<DeviceId, DeviceState>,
    snmp: snmp::SnmpOverviewData<'a>,
    firing_by_protocol: &'a HashMap<String, usize>,
) -> Element<'a, Message> {
    // Count devices by producer
    let protocol_counts = count_devices_by_producer(devices);

    // Only show producers that have devices
    if !protocol_counts.values().any(|count| *count > 0) {
        return column![].into();
    }

    // Header with expand/collapse toggle
    let toggle_icon = if state.expanded {
        icons::arrow_down(IconSize::Small)
    } else {
        icons::arrow_right(IconSize::Small)
    };

    let header_btn = button(
        row![toggle_icon, text("Protocol Overviews").size(font::BODY)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleOverviewExpanded)
    .style(iced::widget::button::text);

    if !state.expanded {
        return container(header_btn).width(Length::Fill).into();
    }

    // Protocol tabs
    let tabs = render_protocol_tabs(state, &protocol_counts);

    // Selected protocol content
    let content: Element<'a, Message> = if let Some(producer) = state.selected_producer.as_deref() {
        let protocol_devices: HashMap<&DeviceId, &DeviceState> = devices
            .iter()
            .filter(|(id, _)| id.producer == producer)
            .collect();

        // Firing-alert headline tile (#582), same for every protocol tab:
        // count of this protocol's firing external alerts, clicking through
        // to the Alerts view pre-filtered to it.
        // An alert names its producer as the closed enum, so a producer
        // outside it has no firing count to show — and no Alerts filter to
        // open (#1256).
        let firing = firing_by_protocol.get(producer).copied().unwrap_or(0);
        let alert_tile: Element<'a, Message> =
            if let (true, Ok(protocol)) = (firing > 0, producer.parse::<Protocol>()) {
                button(
                    text(format!(
                        "{firing} firing alert{} →",
                        if firing == 1 { "" } else { "s" }
                    ))
                    .size(font::CAPTION)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).status_error()),
                    }),
                )
                .on_press(Message::OpenAlertsForProtocol(protocol))
                .style(iced::widget::button::text)
                .into()
            } else {
                text("No firing alerts")
                    .size(font::CAPTION)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    })
                    .into()
            };

        // A producer the GUI was compiled with may have a bespoke overview;
        // everything else — an enum member without one, or a producer outside
        // the enum entirely (#1256) — gets the generic device table.
        let body = match producer.parse::<Protocol>().ok() {
            Some(Protocol::Snmp) => snmp::snmp_overview(&protocol_devices, snmp),
            Some(Protocol::Sysinfo) => sysinfo::sysinfo_overview(&protocol_devices),
            Some(Protocol::Logs) => syslog::syslog_overview(&protocol_devices),
            Some(Protocol::Netflow) => netflow::netflow_overview(&protocol_devices),
            Some(Protocol::Modbus) => modbus::modbus_overview(&protocol_devices),
            Some(Protocol::Gnmi) => gnmi::gnmi_overview(&protocol_devices),
            Some(Protocol::Netlink) => netlink::netlink_overview(&protocol_devices),
            Some(Protocol::Netring) => netring::netring_overview(&protocol_devices),
            // #818: one device per guest, plus the hypervisor itself.
            Some(Protocol::Pve) => pve::pve_overview(&protocol_devices),
            // #819: one device per container.
            Some(Protocol::Container) => containers::container_overview(&protocol_devices),
            // #820: one device per configured target.
            Some(Protocol::Probe) => probe::probe_overview(&protocol_devices),
            _ => generic_overview(&protocol_devices, generic_label(producer)),
        };
        column![alert_tile, body].spacing(8).into()
    } else {
        text("Select a protocol tab to view aggregated metrics")
            .size(font::CAPTION)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into()
    };

    let content_container =
        container(content)
            .padding(15)
            .width(Length::Fill)
            .style(|t: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::colors(t).card_background())),
                border: iced::Border {
                    color: theme::colors(t).border(),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            });

    column![header_btn, tabs, content_container]
        .spacing(8)
        .width(Length::Fill)
        .into()
}

/// The order the tabs prefer to appear in — an **ordering hint, not a gate**.
///
/// #1128: this used to be the whole list, and `render_protocol_tabs` iterated
/// it. It was frozen at the nine protocols that existed when it was written, so
/// every protocol added since — systemd, parallax, hostspec, pve, bmc,
/// container, probe, historian — could never get a tab, and its arm in
/// [`render_protocol_overview`] was **unreachable code**. `generic_overview(…,
/// "guests")` had sat there for the pve sensor's whole life, compiling, tested
/// by nothing, rendered never.
///
/// The fix is structural rather than "add the missing eight": tabs are now
/// built from the protocols actually present, and this array only says which
/// come first. A protocol nobody added here still gets a tab — after the listed
/// ones, in name order — so the next sensor cannot be silently invisible.
const TAB_ORDER: [&str; 9] = [
    "sysinfo", "snmp", "logs", "netflow", "modbus", "gnmi", "netlink", "netring", "opcua",
];

/// The producers to show tabs for: everything with at least one device,
/// [`TAB_ORDER`] first and the rest after it in name order.
fn tab_producers(counts: &HashMap<String, usize>) -> Vec<String> {
    let mut present: Vec<String> = counts
        .iter()
        .filter(|&(_, &n)| n > 0)
        .map(|(p, _)| p.clone())
        .collect();
    present.sort_by_key(|p| {
        (
            TAB_ORDER.iter().position(|q| q == p).unwrap_or(usize::MAX),
            producer_short_name(p),
        )
    });
    present
}

/// Render the producer tabs.
fn render_protocol_tabs<'a>(
    state: &'a OverviewState,
    counts: &HashMap<String, usize>,
) -> Element<'a, Message> {
    let tabs: Vec<Element<'a, Message>> = tab_producers(counts)
        .into_iter()
        .map(|producer| {
            let count = counts.get(&producer).copied().unwrap_or(0);
            let is_selected = state.selected_producer.as_deref() == Some(producer.as_str());

            let icon = icons::for_producer(&producer, IconSize::Small);
            let label =
                text(format!("{} ({})", producer_short_name(&producer), count)).size(font::CAPTION);

            let btn = button(row![icon, label].spacing(6).align_y(Alignment::Center))
                .on_press(Message::SelectOverviewProducer(producer))
                .padding([6, 12])
                .style(if is_selected {
                    iced::widget::button::primary
                } else {
                    iced::widget::button::secondary
                });

            btn.into()
        })
        .collect();

    Row::with_children(tabs).spacing(8).wrap().into()
}

/// A minimal count-and-health overview for protocols without richer aggregates
/// (e.g. OPC-UA, whose specialized telemetry isn't modelled yet).
fn generic_overview<'a>(
    devices: &HashMap<&DeviceId, &DeviceState>,
    noun: &'a str,
) -> Element<'a, Message> {
    if devices.is_empty() {
        return text(format!("No {noun} available"))
            .size(font::CAPTION)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into();
    }
    let healthy = devices.values().filter(|d| d.is_healthy).count();
    let metrics: usize = devices.values().map(|d| d.metric_count).sum();
    row![
        column![
            text("Devices").size(font::MICRO).style(muted),
            text(devices.len().to_string()).size(font::EMPHASIS)
        ]
        .spacing(2),
        column![
            text("Online").size(font::MICRO).style(muted),
            text(healthy.to_string()).size(font::EMPHASIS)
        ]
        .spacing(2),
        column![
            text("Metrics").size(font::MICRO).style(muted),
            text(metrics.to_string()).size(font::EMPHASIS)
        ]
        .spacing(2),
    ]
    .spacing(25)
    .align_y(Alignment::Center)
    .into()
}

fn muted(t: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(t).text_muted()),
    }
}

/// Count devices by producer.
fn count_devices_by_producer(devices: &HashMap<DeviceId, DeviceState>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for device_id in devices.keys() {
        *counts.entry(device_id.producer.clone()).or_insert(0) += 1;
    }
    counts
}

/// The tab label for a producer: a curated short name for the enum members,
/// the producer name itself for everyone else (#1256).
fn producer_short_name(producer: &str) -> String {
    match producer.parse::<Protocol>() {
        Ok(Protocol::Snmp) => "SNMP",
        Ok(Protocol::Sysinfo) => "Sysinfo",
        Ok(Protocol::Logs) => "Logs",
        Ok(Protocol::Netflow) => "NetFlow",
        Ok(Protocol::Modbus) => "Modbus",
        Ok(Protocol::Gnmi) => "gNMI",
        Ok(Protocol::Opcua) => "OPC-UA",
        Ok(Protocol::Netlink) => "Netlink",
        Ok(Protocol::Netring) => "Netring",
        Ok(Protocol::Systemd) => "systemd",
        Ok(Protocol::Parallax) => "Parallax",
        Ok(Protocol::Hostspec) => "hostspec",
        Ok(Protocol::Pve) => "PVE",
        Ok(Protocol::Bmc) => "BMC",
        Ok(Protocol::Container) => "Containers",
        Ok(Protocol::Probe) => "Probes",
        Ok(Protocol::Historian) => "History",
        Err(()) => return producer.to_string(),
    }
    .to_string()
}

/// What the generic table calls the rows of a producer without a bespoke
/// overview — the enum members' labels as they were, and the producer's own
/// name for one outside the enum.
fn generic_label(producer: &str) -> &str {
    match producer.parse::<Protocol>() {
        Ok(Protocol::Opcua) => "OPC-UA nodes",
        Ok(Protocol::Systemd) => "systemd units",
        Ok(Protocol::Parallax) => "video streams",
        Ok(Protocol::Hostspec) => "host assertions",
        // #953: one device per managed chassis.
        Ok(Protocol::Bmc) => "chassis",
        // #898: one device per running instance; the history it holds is
        // read through `@rpc/historian/range` from the charts that need it.
        Ok(Protocol::Historian) => "historians",
        _ => producer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overview_state_default() {
        let state = OverviewState::default();
        assert!(state.selected_producer.is_none());
        assert!(state.expanded);
    }

    #[test]
    fn test_select_protocol_toggle() {
        let mut state = OverviewState::default();

        state.select_producer("snmp".to_string());
        assert_eq!(state.selected_producer, Some("snmp".to_string()));

        // Selecting same protocol toggles off
        state.select_producer("snmp".to_string());
        assert_eq!(state.selected_producer, None);
    }

    #[test]
    fn test_toggle_expanded() {
        let mut state = OverviewState::default();
        assert!(state.expanded);

        state.toggle_expanded();
        assert!(!state.expanded);

        state.toggle_expanded();
        assert!(state.expanded);
    }
    /// #1128: the tab list was a frozen array of nine, and
    /// `render_protocol_overview` had a match arm for every protocol — so
    /// eight of those arms were **unreachable code**. `Protocol::Pve =>
    /// generic_overview(…, "guests")` compiled, was never rendered, and nobody
    /// noticed for the pve sensor's whole life.
    ///
    /// This is the property that makes that unrepeatable: a tab appears for
    /// any protocol with devices, whether or not anyone remembered to list it.
    #[test]
    fn a_protocol_nobody_listed_still_gets_a_tab() {
        let mut counts = HashMap::new();
        counts.insert("sysinfo".to_string(), 3);
        counts.insert("pve".to_string(), 2);
        counts.insert("container".to_string(), 7);
        counts.insert("bmc".to_string(), 1);

        let tabs = tab_producers(&counts);
        for p in ["pve", "container", "bmc"] {
            assert!(
                tabs.iter().any(|t| t == p),
                "{p} has devices and is not in TAB_ORDER — it must still get a tab"
            );
        }
        assert_eq!(tabs.len(), 4);
    }

    /// The listed ones keep their curated order, and come first.
    #[test]
    fn tab_order_is_honoured_and_unlisted_protocols_follow_it() {
        let mut counts = HashMap::new();
        counts.insert("netring".to_string(), 1);
        counts.insert("sysinfo".to_string(), 1);
        counts.insert("pve".to_string(), 1);
        counts.insert("bmc".to_string(), 1);

        let tabs = tab_producers(&counts);
        assert_eq!(tabs[0], "sysinfo", "TAB_ORDER[0] leads");
        assert_eq!(tabs[1], "netring", "then the later listed one");
        // The unlisted two follow, in name order — deterministic, so the tab
        // strip does not reshuffle itself between polls.
        assert_eq!(&tabs[2..], &["bmc", "pve"]);
    }

    /// A producer this GUI was not compiled with gets a tab like any other
    /// (#1256), labelled by its own name, after the listed ones.
    #[test]
    fn a_producer_outside_the_enum_gets_a_tab() {
        let mut counts = HashMap::new();
        counts.insert("sysinfo".to_string(), 1);
        counts.insert("fake-sensor".to_string(), 1);
        let tabs = tab_producers(&counts);
        assert_eq!(tabs, vec!["sysinfo".to_string(), "fake-sensor".to_string()]);
        assert_eq!(producer_short_name("fake-sensor"), "fake-sensor");
        assert_eq!(producer_short_name("pve"), "PVE");
        assert_eq!(generic_label("fake-sensor"), "fake-sensor");
        assert_eq!(generic_label("bmc"), "chassis");
    }

    /// A protocol with no devices gets no tab, listed or not.
    #[test]
    fn a_protocol_with_no_devices_gets_no_tab() {
        let mut counts = HashMap::new();
        counts.insert("sysinfo".to_string(), 1);
        counts.insert("snmp".to_string(), 0);
        assert_eq!(tab_producers(&counts), vec!["sysinfo".to_string()]);
    }
}
