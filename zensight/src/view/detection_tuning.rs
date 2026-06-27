//! Netring detection-tuning panel (#121).
//!
//! Surfaces the netring sensor's runtime detector config (fetched from
//! `zensight/netring/@/status/detectors`) and lets an operator mute/unmute a
//! detector, adjust its threshold, and edit the allowlist — pushed back over the
//! command channel (`@/commands/detectors`) and applied without a sensor
//! restart. Rendered inside the Security view (the NDR home).

use iced::widget::{Row, column, container, row, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use crate::message::Message;
use crate::view::components::card;
use crate::view::theme;

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
}

impl DetectionTuningState {
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
pub fn detection_tuning_panel(state: &DetectionTuningState) -> Element<'_, Message> {
    let muted = |t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    };

    let refresh = button(text("Refresh").size(12))
        .on_press(Message::RefreshDetectorConfig)
        .style(iced::widget::button::secondary);
    let header = row![
        text("Detection Tuning (netring)").size(16),
        iced::widget::Space::new().width(Length::Fill),
        refresh,
    ]
    .align_y(Alignment::Center)
    .spacing(8);

    if !state.loaded {
        let note = state
            .status_note
            .clone()
            .unwrap_or_else(|| "Open with a live netring sensor, then Refresh.".to_string());
        return card(column![header, text(note).size(12).style(muted)].spacing(8));
    }

    // Per-detector rows: mute/unmute + optional threshold edit.
    let mut detectors = column![].spacing(6);
    for d in &state.detectors {
        let toggle = button(text(if d.enabled { "On" } else { "Off" }).size(12))
            .on_press(Message::ToggleNetringDetector(d.name.clone()))
            .style(if d.enabled {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            });
        let mut r = row![
            toggle,
            text(d.label.clone()).size(13).width(Length::Fixed(190.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if d.threshold.is_some() {
            let name = d.name.clone();
            r = r.push(text("threshold").size(11).style(muted));
            r = r.push(
                text_input("", &d.threshold_input)
                    .on_input(move |v| Message::SetNetringThresholdInput {
                        detector: name.clone(),
                        value: v,
                    })
                    .size(12)
                    .padding(4)
                    .width(Length::Fixed(80.0)),
            );
            r = r.push(
                button(text("Apply").size(12))
                    .on_press(Message::ApplyNetringThreshold(d.name.clone()))
                    .style(iced::widget::button::secondary),
            );
        }
        detectors = detectors.push(r);
    }

    // Allowlist editor: chips with remove + an add field.
    let mut chips: Vec<Element<'_, Message>> =
        vec![text("Allowlist:").size(13).style(muted).into()];
    if state.allowlist.is_empty() {
        chips.push(text("(none)").size(12).style(muted).into());
    }
    for entry in &state.allowlist {
        chips.push(
            button(text(format!("{entry}  ✕")).size(12))
                .on_press(Message::RemoveNetringAllowlist(entry.clone()))
                .style(iced::widget::button::secondary)
                .into(),
        );
    }
    let allowlist_row = Row::with_children(chips)
        .spacing(6)
        .align_y(Alignment::Center);
    let add_row = row![
        text_input("host or SLD to allowlist", &state.new_entry)
            .on_input(Message::SetNetringAllowlistInput)
            .on_submit(Message::AddNetringAllowlist)
            .size(12)
            .padding(5)
            .width(Length::Fixed(220.0)),
        button(text("Add").size(12))
            .on_press(Message::AddNetringAllowlist)
            .style(iced::widget::button::primary),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    container(
        column![
            header,
            detectors,
            allowlist_row,
            add_row,
            text("Tuning applies without a sensor restart. Enabling a detector that was off at startup needs a restart.")
                .size(10)
                .style(muted),
        ]
        .spacing(10),
    )
    .into()
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
}
