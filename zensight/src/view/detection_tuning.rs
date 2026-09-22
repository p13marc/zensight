//! Netring detection-tuning panel (#121).
//!
//! Surfaces the netring sensor's runtime detector config (fetched from
//! `@rpc/netring/detectors`) and lets an operator mute/unmute a
//! detector, adjust its threshold, and edit the allowlist — pushed back via
//! `@rpc/netring/detectors/set` and applied without a sensor
//! restart. Rendered inside the Security view (the NDR home).

use iced::widget::{Row, column, container, pick_list, row, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use crate::call::{Armed, CallSurface, Confirmation, Request};
use crate::message::Message;
use crate::view::components::card;
use crate::view::security::SecurityState;
use crate::view::theme;
use crate::view::tokens::font;

/// The three status procedures the panel reads (#121, #225, #328), each at
/// the chosen host — a fleet fan-in's first reply is one host's config,
/// and editing it back fleet-wide was the bug #1114 named.
pub const STATUS_PROCEDURES: [&str; 3] = ["detectors", "capture_filter", "threat_intel"];

/// How long a tuning write waits: the sensor applies it without a restart.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One status read at `host`, on the Security surface.
pub fn status_request(host: &zenkey::RemoteOrigin, procedure: &str) -> Request {
    Request::new(procedure, "")
        .of("netring")
        .on(CallSurface::Security)
        .at(host.clone())
}

/// The three status reads at `host`.
pub fn status_requests(host: &zenkey::RemoteOrigin) -> Vec<Request> {
    STATUS_PROCEDURES
        .iter()
        .map(|p| status_request(host, p))
        .collect()
}

/// What Refresh sends: the three reads, as one message.
pub fn refresh(host: &zenkey::RemoteOrigin) -> Message {
    Message::Batch(
        status_requests(host)
            .into_iter()
            .map(Message::Call)
            .collect(),
    )
}

/// Arm a tuning write to `host` (#1261): `<topic>/set` with the request
/// the registry declares, confirmed by a second click on the pane's armed
/// bar, sent to that host and no other.
pub fn tuning_write(
    host: &zenkey::RemoteOrigin,
    topic: &str,
    request: serde_json::Value,
    label: impl Into<String>,
) -> Message {
    Message::Arm(Armed {
        surface: CallSurface::Security,
        producer: Some("netring".to_string()),
        origin: Some(host.clone()),
        procedure: format!("{topic}/set"),
        request,
        label: label.into(),
        confirmation: Confirmation::Click,
        typed: String::new(),
        timeout: WRITE_TIMEOUT,
    })
}

/// A caption button that is offered only when it has somewhere to write.
fn btn<'a>(
    label: &str,
    style: fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let b = button(text(label.to_string()).size(font::CAPTION)).style(style);
    match on_press {
        Some(m) => b.on_press(m).into(),
        None => b.into(),
    }
}

/// The tunable detectors, in display order: (config key, label, has-threshold).
/// Mirrors `zensight_sensor_netring::command::detector_names`.
const DETECTORS: &[(&str, &str, bool)] = &[
    ("port_scan", "Port scan (TRW)", false),
    ("beaconing", "Beaconing (CV)", true),
    ("rita_beacon", "Beaconing (RITA)", true),
    ("connection_flood", "Connection flood", true),
    ("dga", "DGA scoring", true),
    ("dns_tunnel", "DNS tunnel", false),
    ("nod", "Newly-observed domain", false),
];

/// The threshold field name in `AnomalyConfig` for a detector, if it has one.
fn threshold_field(detector: &str) -> Option<&'static str> {
    match detector {
        "beaconing" => Some("beacon_threshold"),
        "rita_beacon" => Some("rita_beacon_threshold"),
        "connection_flood" => Some("flood_threshold"),
        "dga" => Some("dga_threshold"),
        _ => None,
    }
}

/// One detector's editable row.
#[derive(Debug, Clone)]
pub struct DetectorRow {
    pub name: String,
    pub label: String,
    pub enabled: bool,
    /// The current threshold, or `None` for detectors without one.
    pub threshold: Option<f64>,
    /// The threshold text field (editable, applied on demand).
    pub threshold_input: String,
}

/// The netring sensor's live capture-focus filter state (#225/#228), parsed from
/// `@rpc/netring/capture_filter`.
#[derive(Debug, Clone, Default)]
pub struct CaptureFilterView {
    /// Whether the reloadable packet-tier subscription is wired up.
    pub enabled: bool,
    /// How many reloadable filters the sensor registered (0 ⇒ not reloadable).
    pub reloadable: u64,
    /// The currently-applied filter expression.
    pub current: String,
    /// The configured base filter, restored by `clear`.
    pub base: String,
    /// The last validation error, if the most recent set was rejected.
    pub last_error: Option<String>,
}

/// The netring sensor's live threat-intel (IOC / YARA) reload state (#328),
/// parsed from `@rpc/netring/threat_intel`.
#[derive(Debug, Clone, Default)]
pub struct ThreatIntelView {
    /// IOC reload is armed (monitor built with `ioc(..)`).
    pub ioc_armed: bool,
    /// Live IOC indicator count after the last apply.
    pub ioc_total: u64,
    /// The configured indicator files re-read by "Reload files".
    pub ioc_files: Vec<String>,
    /// YARA reload is armed (built `--features yara`).
    pub yara_armed: bool,
    /// Outcome of the last reload attempt (`ok:` / `error:`), if any.
    pub last_reload: Option<String>,
}

/// Frontend state for the detection-tuning panel.
#[derive(Debug, Default, Clone)]
pub struct DetectionTuningState {
    /// Whether a status reply has been parsed yet.
    pub loaded: bool,
    pub detectors: Vec<DetectorRow>,
    pub allowlist: Vec<String>,
    /// The new-allowlist-entry input.
    pub new_entry: String,
    pub status_note: Option<String>,
    /// Capture-focus BPF expression input (not yet applied) (#225/#228).
    pub packet_filter_input: String,
    /// The sensor's live capture-filter status, once fetched.
    pub capture_filter: Option<CaptureFilterView>,
    /// Paste box for IOC indicators (one per line; IP or domain inferred) (#328).
    pub threat_ioc_input: String,
    /// Paste box for YARA rules source (#328).
    pub threat_yara_input: String,
    /// The sensor's live threat-intel status, once fetched.
    pub threat_intel: Option<ThreatIntelView>,
    /// Schema verdicts for the three status replies (#791) — set beside the
    /// bodies they judge, at the receive sites in `app.rs`.
    pub detectors_verdict: Option<zensight_common::schema::Verdict>,
    pub capture_filter_verdict: Option<zensight_common::schema::Verdict>,
    pub threat_intel_verdict: Option<zensight_common::schema::Verdict>,
}

impl DetectionTuningState {
    /// Forget what was read from a host (#1261) — on a host change, so the
    /// panel never shows one host's config under another's name. The
    /// operator's inputs survive.
    pub fn forget_status(&mut self) {
        self.loaded = false;
        self.detectors.clear();
        self.allowlist.clear();
        self.status_note = None;
        self.capture_filter = None;
        self.threat_intel = None;
        self.detectors_verdict = None;
        self.capture_filter_verdict = None;
        self.threat_intel_verdict = None;
    }

    /// The current enabled state for a detector, if known.
    pub fn is_enabled(&self, detector: &str) -> Option<bool> {
        self.detectors
            .iter()
            .find(|d| d.name == detector)
            .map(|d| d.enabled)
    }

    /// Parse the sensor's `AnomalyConfig` JSON status reply into rows.
    pub fn apply_status(&mut self, json: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            self.status_note = Some("Could not parse detector status".into());
            return;
        };
        self.detectors = DETECTORS
            .iter()
            .map(|(name, label, _)| {
                let enabled = value.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
                let threshold = threshold_field(name)
                    .and_then(|f| value.get(f))
                    .and_then(|v| v.as_f64());
                DetectorRow {
                    name: (*name).to_string(),
                    label: (*label).to_string(),
                    enabled,
                    threshold,
                    threshold_input: threshold.map(fmt_threshold).unwrap_or_default(),
                }
            })
            .collect();
        self.allowlist = value
            .get("allowlist")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.loaded = true;
        self.status_note = None;
    }

    /// Parse the sensor's `CaptureFilterStatus` JSON into the capture-focus view.
    /// Leaves the input field alone (the operator may be mid-edit).
    pub fn apply_capture_filter_status(&mut self, json: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            return;
        };
        let str_field = |k: &str| {
            value
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        self.capture_filter = Some(CaptureFilterView {
            enabled: value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            reloadable: value
                .get("reloadable")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            current: str_field("current"),
            base: str_field("base"),
            last_error: value
                .get("last_error")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }

    /// Parse the sensor's `ThreatIntelStatus` JSON into the threat-intel view.
    /// Leaves the paste boxes alone (the operator may be mid-edit).
    pub fn apply_threat_intel_status(&mut self, json: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            return;
        };
        let bool_field = |k: &str| value.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        self.threat_intel = Some(ThreatIntelView {
            ioc_armed: bool_field("ioc_armed"),
            ioc_total: value.get("ioc_total").and_then(|v| v.as_u64()).unwrap_or(0),
            ioc_files: value
                .get("ioc_files")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            yara_armed: bool_field("yara_armed"),
            last_reload: value
                .get("last_reload")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }
}

/// Split a pasted IOC block into (IPs, domains): one indicator per line, `#`
/// comments and blanks skipped, an IP-parseable line → IP else domain (mirrors
/// the sensor's indicator-file inference).
pub fn split_ioc_paste(text: &str) -> (Vec<String>, Vec<String>) {
    let mut ips = Vec::new();
    let mut domains = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.parse::<std::net::IpAddr>().is_ok() {
            ips.push(l.to_string());
        } else {
            domains.push(l.to_string());
        }
    }
    (ips, domains)
}

/// Format a threshold without trailing noise (e.g. `0.8`, `100`).
fn fmt_threshold(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

/// Render the detection-tuning panel.
pub fn detection_tuning_panel<'a>(
    state: &'a DetectionTuningState,
    sec: &'a SecurityState,
) -> Element<'a, Message> {
    let muted = |t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    };
    let host = sec.host_origin();
    // Every write control is offered only with a host to write to.
    let arm = |topic: &str, request: serde_json::Value, label: String| -> Option<Message> {
        host.as_ref()
            .map(|h| tuning_write(h, topic, request, label))
    };

    // The host chooser (#1261): one host's sensor, chosen or alone.
    let host_pick = pick_list(
        sec.hosts.clone(),
        sec.host.clone(),
        Message::SetSecurityHost,
    )
    .placeholder("pick a netring host")
    .text_size(font::CAPTION);
    let refresh = btn(
        "Refresh",
        iced::widget::button::secondary,
        host.as_ref().map(refresh),
    );
    let mut header = row![text("Detection Tuning (netring)").size(font::EMPHASIS)];
    if let Some(v) = &state.detectors_verdict {
        header = header.push(crate::view::components::verdict::verdict_badge(v));
    }
    let header = header
        .push(iced::widget::Space::new().width(Length::Fill))
        .push(host_pick)
        .push(refresh)
        .align_y(Alignment::Center)
        .spacing(8);
    let mut top = column![header].spacing(8);
    if let Some(bar) = armed_bar(sec) {
        top = top.push(bar);
    }
    if host.is_none() {
        top = top.push(
            text("No host chosen — netring tuning belongs to one host's sensor; pick the host above.")
                .size(font::CAPTION)
                .style(muted),
        );
    }

    if !state.loaded {
        let note = state
            .status_note
            .clone()
            .unwrap_or_else(|| "Open with a live netring sensor, then Refresh.".to_string());
        return column![
            card(top.push(text(note).size(font::CAPTION).style(muted))),
            capture_focus_card(state, &arm),
            threat_intel_card(state, &arm),
        ]
        .spacing(12)
        .into();
    }

    // Per-detector rows: mute/unmute + optional threshold edit.
    let mut detectors = column![].spacing(6);
    for d in &state.detectors {
        let enabled = !d.enabled;
        let toggle = btn(
            if d.enabled { "On" } else { "Off" },
            if d.enabled {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            },
            arm(
                "detectors",
                serde_json::json!({ "type": "set_enabled", "detector": d.name, "enabled": enabled }),
                format!("{} {}", if enabled { "enable" } else { "mute" }, d.label),
            ),
        );
        let mut r = row![
            toggle,
            text(d.label.clone())
                .size(font::BODY)
                .width(Length::Fixed(190.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if d.threshold.is_some() {
            let name = d.name.clone();
            r = r.push(text("threshold").size(font::DENSE).style(muted));
            r = r.push(
                text_input("", &d.threshold_input)
                    .on_input(move |v| Message::SetNetringThresholdInput {
                        detector: name.clone(),
                        value: v,
                    })
                    .size(font::CAPTION)
                    .padding(4)
                    .width(Length::Fixed(80.0)),
            );
            // Offered only for a number: a threshold that does not parse
            // has nothing to send.
            let value = d.threshold_input.trim().parse::<f64>().ok();
            r = r.push(btn(
                "Apply",
                iced::widget::button::secondary,
                value.and_then(|v| {
                    arm(
                        "detectors",
                        serde_json::json!({ "type": "set_threshold", "detector": d.name, "value": v }),
                        format!("{} threshold = {v}", d.label),
                    )
                }),
            ));
        }
        detectors = detectors.push(r);
    }

    // Allowlist editor: chips with remove + an add field.
    let mut chips: Vec<Element<'_, Message>> =
        vec![text("Allowlist:").size(font::BODY).style(muted).into()];
    if state.allowlist.is_empty() {
        chips.push(text("(none)").size(font::CAPTION).style(muted).into());
    }
    for entry in &state.allowlist {
        chips.push(btn(
            &format!("{entry}  ✕"),
            iced::widget::button::secondary,
            arm(
                "detectors",
                serde_json::json!({ "type": "remove_allowlist", "entry": entry }),
                format!("remove {entry} from the allowlist"),
            ),
        ));
    }
    let allowlist_row = Row::with_children(chips)
        .spacing(6)
        .align_y(Alignment::Center);
    let entry = state.new_entry.trim().to_string();
    let add = (!entry.is_empty())
        .then(|| {
            arm(
                "detectors",
                serde_json::json!({ "type": "add_allowlist", "entry": entry }),
                format!("allowlist {entry}"),
            )
        })
        .flatten();
    let input = text_input("host or SLD to allowlist", &state.new_entry)
        .on_input(Message::SetNetringAllowlistInput)
        .size(font::CAPTION)
        .padding(5)
        .width(Length::Fixed(220.0));
    let input = match add.clone() {
        Some(m) => input.on_submit(m),
        None => input,
    };
    let add_row = row![input, btn("Add", iced::widget::button::primary, add)]
        .spacing(8)
        .align_y(Alignment::Center);

    column![
        container(
            top.push(detectors)
                .push(allowlist_row)
                .push(add_row)
                .push(
                    text("Tuning applies without a sensor restart. Enabling a detector that was off at startup needs a restart.")
                        .size(font::MICRO)
                        .style(muted),
                )
                .spacing(10),
        ),
        capture_focus_card(state, &arm),
        threat_intel_card(state, &arm),
    ]
    .spacing(12)
    .into()
}

/// The pane's armed write, when there is one (#1261): what will be sent,
/// to which host, and the confirm/cancel pair — one bar for every control,
/// since one write is armed at a time.
fn armed_bar(sec: &SecurityState) -> Option<Element<'_, Message>> {
    let host = sec
        .host
        .as_ref()
        .map(|h| h.label.clone())
        .unwrap_or_default();
    if let Some(armed) = &sec.writes.armed {
        return Some(
            row![
                text(format!("{} on {host}?", armed.label)).size(font::CAPTION),
                btn(
                    "confirm",
                    iced::widget::button::primary,
                    Some(Message::Confirm)
                ),
                btn(
                    "cancel",
                    iced::widget::button::secondary,
                    Some(Message::Disarm)
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    sec.writes.inflight.as_ref().map(|a| {
        text(format!("{} on {host}…", a.label))
            .size(font::CAPTION)
            .into()
    })
}

/// Capture-focus card (#225/#228): a live BPF box that hot-swaps the netring
/// sensor's reloadable packet-tier filter via `@rpc/netring/capture_filter/set`, with a
/// readout of the currently-applied filter (and any validation error) from
/// `@rpc/netring/capture_filter`. Narrows capture attention during an incident
/// without restarting capture.
fn capture_focus_card<'a>(
    state: &'a DetectionTuningState,
    arm: &dyn Fn(&str, serde_json::Value, String) -> Option<Message>,
) -> Element<'a, Message> {
    let muted = |t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    };
    let danger = |t: &Theme| text::Style {
        color: Some(theme::colors(t).danger()),
    };

    let mut header = row![text("Capture Focus (netring)").size(font::EMPHASIS)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(v) = &state.capture_filter_verdict {
        header = header.push(crate::view::components::verdict::verdict_badge(v));
    }
    let expr = state.packet_filter_input.trim().to_string();
    let apply = (!expr.is_empty())
        .then(|| {
            arm(
                "capture_filter",
                serde_json::json!({ "type": "set_packet_filter", "expr": expr }),
                format!("capture filter → {expr}"),
            )
        })
        .flatten();
    let input = text_input(
        "BPF expr, e.g. host 10.0.0.5 and port 443",
        &state.packet_filter_input,
    )
    .on_input(Message::SetPacketFilterInput)
    .size(font::CAPTION)
    .padding(5)
    .width(Length::Fixed(320.0));
    let input = match apply.clone() {
        Some(m) => input.on_submit(m),
        None => input,
    };
    let input_row = row![
        input,
        btn("Apply", iced::widget::button::primary, apply),
        btn(
            "Clear",
            iced::widget::button::secondary,
            arm(
                "capture_filter",
                serde_json::json!({ "type": "clear_packet_filter" }),
                "clear the capture filter".to_string(),
            ),
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut body = column![
        header,
        input_row,
        text("Grammar: tcp|udp|icmp, [src|dst] port N, [src|dst] host IP, [src|dst] net CIDR, combined with and/or/!/parens.")
            .size(font::MICRO)
            .style(muted),
    ]
    .spacing(8);

    match &state.capture_filter {
        None => {
            body = body.push(
                text("Refresh to load the live capture filter.")
                    .size(font::CAPTION)
                    .style(muted),
            );
        }
        Some(cf) if !cf.enabled || cf.reloadable == 0 => {
            body = body.push(
                text("Capture-focus is disabled on this sensor (set capture_focus.enabled). Live capture only.")
                    .size(font::CAPTION)
                    .style(muted),
            );
        }
        Some(cf) => {
            body = body
                .push(text(format!("current: {}", cf.current)).size(font::CAPTION))
                .push(
                    text(format!("base: {}", cf.base))
                        .size(font::DENSE)
                        .style(muted),
                );
            if let Some(err) = &cf.last_error {
                body = body.push(
                    text(format!("✕ rejected: {err}"))
                        .size(font::CAPTION)
                        .style(danger),
                );
            }
        }
    }

    card(body)
}

/// Threat-intel (IOC / YARA) hot-reload card (#328): paste indicators or YARA
/// rules and swap them into the live netring matchers via
/// `@rpc/netring/threat_intel/set`, with an armed/loaded readout and the last-reload
/// outcome from `@rpc/netring/threat_intel`. No capture restart.
fn threat_intel_card<'a>(
    state: &'a DetectionTuningState,
    arm: &dyn Fn(&str, serde_json::Value, String) -> Option<Message>,
) -> Element<'a, Message> {
    let muted = |t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    };
    let danger = |t: &Theme| text::Style {
        color: Some(theme::colors(t).danger()),
    };

    let (ips, domains) = split_ioc_paste(&state.threat_ioc_input);
    let n = ips.len() + domains.len();
    let apply_ioc = (n > 0)
        .then(|| {
            arm(
                "threat_intel",
                serde_json::json!({
                    "type": "set_ioc", "ips": ips, "domains": domains, "ja4": [], "ja3": [],
                }),
                format!("push {n} IOC indicators"),
            )
        })
        .flatten();
    let ioc_row = row![
        text_input("IOCs, one per line (IP or domain)", &state.threat_ioc_input)
            .on_input(Message::SetThreatIocInput)
            .size(font::CAPTION)
            .padding(5)
            .width(Length::Fixed(320.0)),
        btn("Apply IOCs", iced::widget::button::primary, apply_ioc),
        btn(
            "Reload files",
            iced::widget::button::secondary,
            arm(
                "threat_intel",
                serde_json::json!({ "type": "reload_ioc_files" }),
                "reload the indicator files".to_string(),
            ),
        ),
        btn(
            "Clear",
            iced::widget::button::secondary,
            arm(
                "threat_intel",
                serde_json::json!({ "type": "clear_ioc" }),
                "clear the IOC indicators".to_string(),
            ),
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let rules = state.threat_yara_input.trim().to_string();
    let apply_yara = (!rules.is_empty())
        .then(|| {
            arm(
                "threat_intel",
                serde_json::json!({ "type": "set_yara", "rules": rules }),
                "apply the YARA rules".to_string(),
            )
        })
        .flatten();
    let yara_row = row![
        text_input("YARA rules source", &state.threat_yara_input)
            .on_input(Message::SetThreatYaraInput)
            .size(font::CAPTION)
            .padding(5)
            .width(Length::Fixed(320.0)),
        btn("Apply YARA", iced::widget::button::primary, apply_yara),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut ti_header = row![text("Threat Intel (netring)").size(font::EMPHASIS)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(v) = &state.threat_intel_verdict {
        ti_header = ti_header.push(crate::view::components::verdict::verdict_badge(v));
    }
    let mut body = column![ti_header, ioc_row, yara_row].spacing(8);

    match &state.threat_intel {
        None => {
            body = body.push(
                text("Refresh to load the live threat-intel status.")
                    .size(font::CAPTION)
                    .style(muted),
            );
        }
        Some(ti) => {
            let ioc_line = if ti.ioc_armed {
                format!("IOC: armed — {} live indicators", ti.ioc_total)
            } else {
                "IOC: not armed (set threat.reload=true or provide startup indicators)".to_string()
            };
            body = body.push(text(ioc_line).size(font::CAPTION));
            if !ti.ioc_files.is_empty() {
                body = body.push(
                    text(format!("files: {}", ti.ioc_files.join(", ")))
                        .size(font::DENSE)
                        .style(muted),
                );
            }
            let yara_line = if ti.yara_armed {
                "YARA: armed"
            } else {
                "YARA: not armed (build --features yara + threat.reload/threat.yara.file)"
            };
            body = body.push(text(yara_line).size(font::CAPTION).style(muted));
            if let Some(last) = &ti.last_reload {
                let is_err = last.starts_with("error");
                let line = text(format!("last: {last}")).size(font::DENSE);
                body = body.push(if is_err { line.style(danger) } else { line });
            }
        }
    }

    card(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_into_rows() {
        let json = r#"{
            "port_scan": true,
            "beaconing": true, "beacon_threshold": 0.8,
            "rita_beacon": false, "rita_beacon_threshold": 0.9,
            "connection_flood": true, "flood_threshold": 100,
            "dga": false, "dga_threshold": -8.0,
            "dns_tunnel": true, "nod": false,
            "allowlist": ["telemetry.host", "cdn.example"]
        }"#;
        let mut state = DetectionTuningState::default();
        state.apply_status(json);
        assert!(state.loaded);
        assert_eq!(state.detectors.len(), DETECTORS.len());
        assert_eq!(state.is_enabled("port_scan"), Some(true));
        assert_eq!(state.is_enabled("rita_beacon"), Some(false));
        let beacon = state
            .detectors
            .iter()
            .find(|d| d.name == "beaconing")
            .unwrap();
        assert_eq!(beacon.threshold, Some(0.8));
        assert_eq!(beacon.threshold_input, "0.8");
        let flood = state
            .detectors
            .iter()
            .find(|d| d.name == "connection_flood")
            .unwrap();
        assert_eq!(flood.threshold_input, "100");
        // Detectors without a threshold carry none.
        let nod = state.detectors.iter().find(|d| d.name == "nod").unwrap();
        assert!(nod.threshold.is_none());
        assert_eq!(state.allowlist, vec!["telemetry.host", "cdn.example"]);
    }

    #[test]
    fn bad_json_sets_note_not_panic() {
        let mut state = DetectionTuningState::default();
        state.apply_status("not json");
        assert!(!state.loaded);
        assert!(state.status_note.is_some());
    }

    #[test]
    fn parses_capture_filter_status() {
        let mut state = DetectionTuningState::default();
        state.apply_capture_filter_status(
            r#"{"enabled":true,"reloadable":1,"current":"host 10.0.0.5","base":"tcp or udp or icmp","last_error":"unexpected token foo"}"#,
        );
        let cf = state.capture_filter.expect("parsed");
        assert!(cf.enabled);
        assert_eq!(cf.reloadable, 1);
        assert_eq!(cf.current, "host 10.0.0.5");
        assert_eq!(cf.base, "tcp or udp or icmp");
        assert_eq!(cf.last_error.as_deref(), Some("unexpected token foo"));
    }

    #[test]
    fn capture_filter_status_no_error_is_none() {
        let mut state = DetectionTuningState::default();
        state.apply_capture_filter_status(
            r#"{"enabled":true,"reloadable":1,"current":"tcp","base":"tcp"}"#,
        );
        assert!(state.capture_filter.unwrap().last_error.is_none());
    }

    #[test]
    fn bad_capture_filter_json_leaves_state() {
        let mut state = DetectionTuningState::default();
        state.apply_capture_filter_status("not json");
        assert!(state.capture_filter.is_none());
    }

    #[test]
    fn parses_threat_intel_status() {
        let mut state = DetectionTuningState::default();
        state.apply_threat_intel_status(
            r#"{"ioc_armed":true,"ioc_total":3,"ioc_files":["/etc/iocs.txt"],"yara_armed":false,"last_reload":"error: yara compile failed: bad"}"#,
        );
        let ti = state.threat_intel.expect("parsed");
        assert!(ti.ioc_armed);
        assert_eq!(ti.ioc_total, 3);
        assert_eq!(ti.ioc_files, vec!["/etc/iocs.txt"]);
        assert!(!ti.yara_armed);
        assert!(ti.last_reload.as_deref().unwrap().starts_with("error"));
    }

    #[test]
    fn bad_threat_intel_json_leaves_state() {
        let mut state = DetectionTuningState::default();
        state.apply_threat_intel_status("not json");
        assert!(state.threat_intel.is_none());
    }

    #[test]
    fn split_ioc_paste_infers_ip_vs_domain() {
        let (ips, domains) = split_ioc_paste(
            "198.51.100.7\n# a comment\nmalware.test\n\n  2001:db8::1  \nevil.example\n",
        );
        assert_eq!(ips, vec!["198.51.100.7", "2001:db8::1"]);
        assert_eq!(domains, vec!["malware.test", "evil.example"]);
    }
}
