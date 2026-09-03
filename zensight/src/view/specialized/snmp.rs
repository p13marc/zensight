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

use zensight_common::outlet::{OutletCapability, OutletStatus};
use zensight_common::{IfStatus, InterfaceEntry, InterfaceTable, TelemetryValue};

use crate::message::Message;
use crate::view::components::{
    Column as DataColumn, DataTable, Gauge, SortKey, StatusLed, StatusLedState, TableState, card,
    empty_state,
};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_rate;
use crate::view::icons::{self, IconSize};
use crate::view::specialized::fetch::Fetch;
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

    // ── Gated PDU outlet control (#956) ─────────────────────────────────
    /// What the sensor says it will permit. Probed on drill-in, and the whole
    /// panel is rendered from it rather than optimistically: on a default
    /// deployment — which is every deployment until someone decides otherwise
    /// — there is nothing to click, and learning that from an error toast a
    /// second later is not an answer.
    pub outlet_capability: Fetch<OutletCapability>,
    /// The outlet armed for confirmation, and what the operator has typed so
    /// far. Typing the outlet's own name is the confirmation: a `[confirm]`
    /// button one slip away from a live one is not a confirmation, and this
    /// action cuts power.
    pub pending_outlet: Option<String>,
    pub outlet_confirm_text: String,
    /// The outlet whose cycle is in flight — no re-arming until it resolves.
    pub outlet_inflight: Option<String>,
    /// The most recent outcome, for the panel's footer.
    pub outlet_last: Option<OutletStatus>,
}

/// What the panel may offer for one outlet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutletGate {
    /// The probe has not answered yet. Controls render **disabled rather than
    /// hidden**: hiding would silently strip working controls from a sensor
    /// that does have control on, which is the worse failure of the two.
    Unknown,
    /// The sensor answered "outlet control is off here". The whole panel is
    /// dropped in this case — the button is *absent*, not greyed, because
    /// there is nothing here to enable and a disabled control invites a
    /// support question.
    Disabled,
    /// Control is on, but this outlet is outside `allow_outlets`.
    NotAllowed,
    /// Control is on and this outlet is in scope.
    Allowed,
    /// A cycle on this outlet is in flight.
    Busy,
}

/// The gated-write key for one device's outlet control, **origin-scoped**.
///
/// There is deliberately no fleet fallback. A wildcard origin here would
/// cycle the matching outlet on every host serving the sensor, which on a
/// fleet is a datacentre going dark — the sharper case of the same rule
/// `systemd action/set` follows (RFC 05 G2, and `docs/KEYSPACE.md`).
pub fn outlet_action_key(origin: &zenkey::RemoteOrigin) -> String {
    zensight_common::origin_rpc_key(origin, "snmp", "action/set")
}

/// The outlet-control probe key. Answered whether control is on or off, so
/// "off" is an answer and not a silence (#648).
pub fn outlet_capability_key(origin: &zenkey::RemoteOrigin) -> String {
    zensight_common::origin_rpc_key(origin, "snmp", "action/capability")
}

impl SnmpDetailState {
    /// What this sensor will permit for `outlet` on `device`, from its own
    /// advertised gate.
    ///
    /// Shares [`OutletCapability::permits`] — and therefore
    /// `zensight_common::action::allows` — with the sensor's own gate, so the
    /// button and the decision cannot disagree about what a glob means.
    pub fn outlet_gate(&self, device: &str, outlet: &str) -> OutletGate {
        if self.outlet_inflight.as_deref() == Some(outlet) {
            return OutletGate::Busy;
        }
        let Some(cap) = self.outlet_capability.ready() else {
            return OutletGate::Unknown;
        };
        if !cap.enabled {
            return OutletGate::Disabled;
        }
        if cap.permits(&format!("{device}/{outlet}")) {
            OutletGate::Allowed
        } else {
            OutletGate::NotAllowed
        }
    }

    /// Whether the typed confirmation matches the armed outlet.
    ///
    /// Exact, and trimmed only of surrounding whitespace: the point of typing
    /// the name is that it cannot be produced by a slip.
    pub fn outlet_confirmation_matches(&self) -> bool {
        self.pending_outlet
            .as_deref()
            .is_some_and(|o| self.outlet_confirm_text.trim() == o)
    }

    /// Outlet ids this device has published, with their state — from the raw
    /// metric tree (`pdu/outlet/{index}/state`, #955).
    ///
    /// Read off the metrics rather than a state document because there is no
    /// outlet document: #955 registered the outlet columns as telemetry
    /// families, and the panel needs only the ids and their last known state.
    ///
    /// **The raw value is vendor-specific and cannot be read without knowing
    /// which vendor answered**: APC spells off(1)/on(2), Eaton and Raritan
    /// off(0)/on(1), so `1` means opposite things on the two. The applied
    /// profile is already on the bus as the `system/profile` text metric, so
    /// the mapping is looked up rather than guessed — and an unrecognised
    /// profile yields `None`, which renders as "—". A wrong on/off in a panel
    /// beside a power button is worse than no on/off at all.
    pub fn outlets(
        metrics: &std::collections::HashMap<String, zensight_common::TelemetryPoint>,
    ) -> Vec<(String, Option<bool>)> {
        let profile = metrics.get("system/profile").and_then(|p| match &p.value {
            TelemetryValue::Text(s) => Some(s.as_str()),
            _ => None,
        });
        let read_state = outlet_state_reader(profile);

        let mut out: Vec<(String, Option<bool>)> = metrics
            .iter()
            .filter_map(|(name, point)| {
                let rest = name.strip_prefix("pdu/outlet/")?;
                let (index, leaf) = rest.rsplit_once('/')?;
                (leaf == "state").then(|| {
                    let raw = match &point.value {
                        TelemetryValue::Gauge(v) => Some(*v as i64),
                        TelemetryValue::Counter(v) => i64::try_from(*v).ok(),
                        _ => None,
                    };
                    (index.to_string(), raw.and_then(&read_state))
                })
            })
            .collect();
        out.sort_by_key(|(id, _)| natural_outlet_order(id));
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }
}

/// How to read a raw outlet state, given the applied profile set.
///
/// Anything that is neither the vendor's on nor its off value is a
/// **transition** — Eaton's `pendingOn`, Raritan's `cycling` — and reads as
/// unknown rather than as off, for the same reason the sensor's own rules
/// treat it that way (#955).
fn outlet_state_reader(profile: Option<&str>) -> Box<dyn Fn(i64) -> Option<bool>> {
    let applied = profile.unwrap_or_default();
    if applied.contains("pdu-apc") {
        Box::new(|n| match n {
            2 => Some(true),
            1 => Some(false),
            _ => None,
        })
    } else if applied.contains("pdu-eaton") || applied.contains("pdu-raritan") {
        Box::new(|n| match n {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        })
    } else {
        // No profile, or one this build does not know: the raw integer means
        // nothing without a vendor, so say nothing.
        Box::new(|_| None)
    }
}

/// Sort outlet ids the way a rack is numbered: `2` before `10`, and an
/// Eaton's `1.2` before `1.10`. Lexicographic order would put outlet 10
/// between 1 and 2, which reads as a PDU that cannot count.
fn natural_outlet_order(id: &str) -> Vec<u64> {
    id.split('.')
        .map(|p| p.parse().unwrap_or(u64::MAX))
        .collect()
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

    let mut content = column![
        header,
        card(system_info),
        card(interfaces),
        card(render_events(state)),
        card(system_metrics),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    // The outlet panel is ABSENT, not disabled, on a device that publishes no
    // outlets or a sensor that says control is off — the issue's own rule
    // (#956). A greyed power button invites a support question; nothing at all
    // is the honest rendering of "this deployment does not do that".
    if let Some(outlets) = render_outlets(state) {
        content = content.push(card(outlets));
    }

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The gated outlet-control panel (#956), or nothing.
///
/// Three conditions, and each removes the panel entirely rather than greying
/// it: the device publishes no outlets, the sensor says control is off, or the
/// probe has not answered and the device is not a PDU anyway. What is left is
/// a panel that appears only where an operator can actually act.
fn render_outlets(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    let d = &state.snmp_detail;
    let outlets = SnmpDetailState::outlets(&state.metrics);
    if outlets.is_empty() {
        return None;
    }
    // "Off here" is an answer (#648) — but it is an answer that belongs in a
    // sentence, not in a row of dead buttons.
    let disabled_reason = match &d.outlet_capability {
        Fetch::Ready(cap) if !cap.enabled => Some(
            cap.reason
                .clone()
                .unwrap_or_else(|| "outlet control is disabled on this sensor".to_string()),
        ),
        _ => None,
    };

    let device = state.device_id.source.as_str();
    let mut rows = WColumn::new().spacing(space::XS);
    for (outlet, on) in &outlets {
        rows = rows.push(outlet_row(d, device, outlet.clone(), *on));
    }

    let mut panel = column![
        row![
            text("Outlets").size(font::BODY),
            text(format!("({})", outlets.len()))
                .size(font::CAPTION)
                .style(dim),
        ]
        .spacing(space::XS)
        .align_y(Alignment::Center),
    ]
    .spacing(space::SM);

    if let Some(reason) = disabled_reason {
        // The whole point of the capability probe: the operator learns why
        // there is nothing to press, here, rather than from an error toast a
        // second and a half after clicking.
        panel = panel.push(text(reason).size(font::CAPTION).style(dim));
    }
    panel = panel.push(rows);

    if let Some(last) = &d.outlet_last {
        let detail = last
            .error
            .clone()
            .or_else(|| last.reason.clone())
            .unwrap_or_else(|| match last.reboot_duration_secs {
                // The PDU's own off-time, not ours.
                Some(secs) => format!("accepted; the PDU's reboot duration is {secs}s"),
                None => "accepted".to_string(),
            });
        panel = panel.push(
            text(format!("last: {} — {detail}", last.outlet))
                .size(font::CAPTION)
                .style(dim),
        );
    }

    Some(panel.into())
}

fn outlet_row<'a>(
    d: &'a SnmpDetailState,
    device: &str,
    // Owned: the outlet ids are derived from the metric map inside
    // `render_outlets`, so a borrow would not outlive that local.
    outlet: String,
    on: Option<bool>,
) -> Element<'a, Message> {
    let led = match on {
        Some(true) => StatusLedState::Active,
        Some(false) => StatusLedState::Inactive,
        // A transition, or a vendor this build cannot read the raw value for.
        // Neither is "off", and showing it as off beside a power button is the
        // one mistake this panel must not make.
        None => StatusLedState::Unknown,
    };
    let state_text = match on {
        Some(true) => "on",
        Some(false) => "off",
        None => "—",
    };

    let control: Element<'a, Message> = if d.pending_outlet.as_deref() == Some(outlet.as_str()) {
        // Armed: the confirmation is typing the outlet's own name. A
        // `[confirm]` button one slip away from a live one is not a
        // confirmation, and this cuts power to whatever is plugged in.
        let matches = d.outlet_confirmation_matches();
        row![
            text(format!("type \"{outlet}\" to cycle:")).size(font::CAPTION),
            iced::widget::text_input("", &d.outlet_confirm_text)
                .on_input(Message::SnmpOutletConfirmTextChanged)
                .size(font::CAPTION)
                .width(Length::Fixed(90.0)),
            tiny_button(
                "cycle".into(),
                matches.then_some(Message::SnmpOutletConfirm)
            ),
            tiny_button("cancel".into(), Some(Message::SnmpOutletCancel)),
        ]
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .into()
    } else {
        match d.outlet_gate(device, &outlet) {
            OutletGate::Busy => text("cycling…").size(font::CAPTION).style(dim).into(),
            OutletGate::Allowed => {
                tiny_button("cycle".into(), Some(Message::SnmpOutletArm(outlet.clone())))
            }
            // Outside the allowlist: say so rather than offering nothing,
            // because "this outlet, deliberately not" is different from "this
            // deployment, not at all".
            OutletGate::NotAllowed => text("not in allowlist")
                .size(font::CAPTION)
                .style(dim)
                .into(),
            // Disabled and Unknown render no control. Disabled has its reason
            // in the panel header; Unknown is the gap before the probe answers.
            OutletGate::Disabled | OutletGate::Unknown => text("").size(font::CAPTION).into(),
        }
    };

    row![
        StatusLed::new(led).view(),
        text(outlet).size(font::CAPTION).width(Length::Fixed(70.0)),
        text(state_text)
            .size(font::CAPTION)
            .style(dim)
            .width(Length::Fixed(40.0)),
        control,
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center)
    .into()
}

/// Dimmed caption text, the shared secondary style.
fn dim(theme: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

/// A small labelled button, live only when it has a message.
fn tiny_button<'a>(label: String, on_press: Option<Message>) -> Element<'a, Message> {
    let b = button(text(label).size(font::CAPTION)).padding(space::XS);
    match on_press {
        Some(m) => b.on_press(m).into(),
        None => b.into(),
    }
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

#[cfg(test)]
mod outlet_tests {
    //! Fixtures build state stepwise, which reads more clearly here than a
    //! struct literal naming every field.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use std::collections::HashMap;
    use zensight_common::outlet::{OutletCapability, OutletVerb};
    use zensight_common::{Protocol, TelemetryPoint};

    fn cap(enabled: bool, allow: &[&str]) -> OutletCapability {
        OutletCapability {
            enabled,
            allow_outlets: allow.iter().map(|s| s.to_string()).collect(),
            verbs: OutletVerb::all(),
            reason: None,
        }
    }

    fn point(metric: &str, value: TelemetryValue) -> (String, TelemetryPoint) {
        (
            metric.to_string(),
            TelemetryPoint::new("pdu-a", Protocol::Snmp, metric, value),
        )
    }

    /// The panel renders from the sensor's advertised gate, and shares its
    /// matcher — so the button and the decision cannot disagree about a glob.
    #[test]
    fn the_gate_mirrors_what_the_sensor_advertises() {
        let mut d = SnmpDetailState::default();
        assert_eq!(d.outlet_gate("pdu-a", "3"), OutletGate::Unknown);

        d.outlet_capability = Fetch::Ready(cap(false, &[]));
        assert_eq!(d.outlet_gate("pdu-a", "3"), OutletGate::Disabled);

        d.outlet_capability = Fetch::Ready(cap(true, &[]));
        assert_eq!(
            d.outlet_gate("pdu-a", "3"),
            OutletGate::NotAllowed,
            "an empty allowlist permits nothing even with the switch on"
        );

        d.outlet_capability = Fetch::Ready(cap(true, &["pdu-a/*"]));
        assert_eq!(d.outlet_gate("pdu-a", "3"), OutletGate::Allowed);
        assert_eq!(
            d.outlet_gate("pdu-b", "3"),
            OutletGate::NotAllowed,
            "a different PDU is not covered"
        );

        d.outlet_inflight = Some("3".to_string());
        assert_eq!(d.outlet_gate("pdu-a", "3"), OutletGate::Busy);
    }

    /// **Typing the name is the confirmation.** A `[confirm]` button one slip
    /// away from a live one is not a confirmation, and this cuts power.
    #[test]
    fn the_confirmation_must_match_the_outlet_exactly() {
        let mut d = SnmpDetailState::default();
        d.pending_outlet = Some("3".to_string());

        for typed in ["", "4", "33", "outlet 3", "  "] {
            d.outlet_confirm_text = typed.to_string();
            assert!(
                !d.outlet_confirmation_matches(),
                "{typed:?} must not arm the button"
            );
        }
        d.outlet_confirm_text = "3".to_string();
        assert!(d.outlet_confirmation_matches());
        // Surrounding whitespace is forgiven; nothing else is.
        d.outlet_confirm_text = " 3 ".to_string();
        assert!(d.outlet_confirmation_matches());

        // Nothing armed: nothing matches, whatever is typed.
        d.pending_outlet = None;
        assert!(!d.outlet_confirmation_matches());
    }

    /// The raw outlet state is vendor-specific — APC off(1)/on(2), Eaton and
    /// Raritan off(0)/on(1) — so `1` means opposite things. The applied
    /// profile decides, and an unknown one yields no state at all: a wrong
    /// on/off beside a power button is worse than none.
    #[test]
    fn outlet_state_is_read_through_the_applied_profile_or_not_at_all() {
        let mut metrics: HashMap<String, TelemetryPoint> = HashMap::new();
        metrics.extend([
            point("pdu/outlet/1/state", TelemetryValue::Gauge(2.0)),
            point("pdu/outlet/2/state", TelemetryValue::Gauge(1.0)),
        ]);

        // No profile: the integers mean nothing.
        let outlets = SnmpDetailState::outlets(&metrics);
        assert_eq!(outlets.len(), 2);
        assert!(outlets.iter().all(|(_, on)| on.is_none()));

        // APC: 2 is on, 1 is off.
        metrics.extend([point(
            "system/profile",
            TelemetryValue::Text("generic-device,pdu-apc".into()),
        )]);
        let outlets = SnmpDetailState::outlets(&metrics);
        assert_eq!(outlets[0], ("1".to_string(), Some(true)));
        assert_eq!(outlets[1], ("2".to_string(), Some(false)));

        // Eaton: the SAME integers mean the opposite.
        metrics.insert(
            "system/profile".to_string(),
            TelemetryPoint::new(
                "pdu-a",
                Protocol::Snmp,
                "system/profile",
                TelemetryValue::Text("generic-device,pdu-eaton".into()),
            ),
        );
        let outlets = SnmpDetailState::outlets(&metrics);
        assert_eq!(outlets[1], ("2".to_string(), Some(true)), "eaton on(1)");
        assert_eq!(
            outlets[0],
            ("1".to_string(), None),
            "2 is not a state Eaton spells — a transition, not off"
        );
    }

    /// Outlets sort the way a rack is numbered. Lexicographic order puts 10
    /// between 1 and 2, which reads as a PDU that cannot count — and on an
    /// Eaton the ids are `unit.outlet` pairs, so it has to be per-component.
    #[test]
    fn outlets_sort_numerically_including_the_two_part_eaton_ids() {
        let mut metrics: HashMap<String, TelemetryPoint> = HashMap::new();
        for id in ["10", "2", "1"] {
            metrics.extend([point(
                &format!("pdu/outlet/{id}/state"),
                TelemetryValue::Gauge(2.0),
            )]);
        }
        let ids: Vec<String> = SnmpDetailState::outlets(&metrics)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec!["1", "2", "10"]);

        assert!(natural_outlet_order("1.2") < natural_outlet_order("1.10"));
        assert!(natural_outlet_order("2") > natural_outlet_order("1.10"));
    }

    /// A device with no outlets has no panel — the button is **absent**, not
    /// disabled (#956). Ditto a non-PDU, which is every other SNMP device.
    #[test]
    fn a_device_with_no_outlets_has_no_panel() {
        assert!(SnmpDetailState::outlets(&HashMap::new()).is_empty());

        let mut metrics: HashMap<String, TelemetryPoint> = HashMap::new();
        metrics.extend([point("if/1/in_octets", TelemetryValue::Counter(10))]);
        assert!(SnmpDetailState::outlets(&metrics).is_empty());
    }

    /// The write key is origin-scoped, and there is deliberately no fleet
    /// spelling: a wildcard here cycles the matching outlet on every host
    /// serving the sensor, which on a fleet is a datacentre going dark.
    #[test]
    fn the_action_keys_are_origin_scoped() {
        let origin = zenkey::RemoteOrigin::parse("h-0123456789ab").expect("valid origin");
        let set = outlet_action_key(&origin);
        let cap_key = outlet_capability_key(&origin);
        assert!(set.contains("h-0123456789ab"), "{set}");
        assert!(set.ends_with("@rpc/snmp/action/set"), "{set}");
        assert!(
            cap_key.ends_with("@rpc/snmp/action/capability"),
            "{cap_key}"
        );
        for key in [&set, &cap_key] {
            assert!(!key.contains('*'), "no wildcard origin is spellable: {key}");
        }
    }
}
