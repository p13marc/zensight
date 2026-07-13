//! Security view — a network-anomaly lens over sensor-pushed alerts.
//!
//! ZenSight's first security surface: it filters the `external` alert set to
//! [`AlertKind::Anomaly`] (port scans, beacons, DGA, ...) and presents them
//! grouped by detector with the evidence each detector emitted, a by-source
//! rollup, a severity filter, and a per-anomaly drill-down (#48).

use std::collections::BTreeMap;

use iced::widget::{Column, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;
use zensight_common::{Alert, AlertKind, AlertSeverity};

use zensight_common::FlowRecord;

use crate::message::Message;
use crate::view::alerts::{AlertsState, Severity};
use crate::view::components::{badge, empty_state};
use crate::view::icons::{self, IconSize};
use crate::view::specialized::fetch::Fetch;
use crate::view::theme;
use crate::view::tokens::font;

/// View-local state for the Security view (#48): a severity filter and the
/// currently expanded anomaly (by `alert_key`).
#[derive(Debug, Default, Clone)]
pub struct SecurityState {
    /// When true, Info-severity anomalies (benign scans, inconclusive DGA) are
    /// hidden from the cards and feed.
    pub hide_info: bool,
    /// The anomaly whose evidence is expanded in the drill-down, if any.
    pub selected: Option<String>,
    /// On-demand flows pivoted from an anomaly (#119): the `@rpc/netring/flows`
    /// reply, filtered to the offending source.
    pub flows: Fetch<Vec<FlowRecord>>,
    /// Which anomaly (`alert_key`) the fetched `flows` belong to, so the pivot
    /// table renders only under the anomaly it was requested from.
    pub flows_for: Option<String>,
    /// The netring capture-to-disk file index (#327), fetched when an anomaly is
    /// expanded, so a triggered capture matching the anomaly can offer its
    /// evidence file for download right in the drill-down.
    pub captures: Fetch<Vec<zensight_common::CaptureRecord>>,
    /// The flow↔process join result for one pivot-flow row (#309): `(flow key,
    /// fetched attribution)`. One in-flight join at a time.
    pub attribution: Option<(
        String,
        Fetch<Option<crate::view::specialized::attribution::AttributedProcess>>,
    )>,
}

/// Find a triggered capture matching this anomaly (#327): same detector slug,
/// with the anomaly's timestamp inside (or near) the capture's packet window.
/// The slack absorbs clock skew and the gap between the detector firing and the
/// trigger flushing.
pub fn matching_capture<'c>(
    a: &Alert,
    captures: Option<&'c Vec<zensight_common::CaptureRecord>>,
) -> Option<&'c zensight_common::CaptureRecord> {
    const SLACK_MS: i64 = 120_000;
    captures?.iter().find(|c| {
        c.mode == "triggered"
            && c.trigger_kind.as_deref() == Some(a.rule.as_str())
            && c.start_ms.saturating_sub(SLACK_MS) <= a.timestamp
            && a.timestamp <= c.end_ms.saturating_add(SLACK_MS)
    })
}

/// Labels shown elsewhere (summary/source), so we don't repeat them as evidence.
// `technique` is surfaced as a dedicated ATT&CK badge (#117), so it's excluded
// from the generic evidence drill-down to avoid showing it twice.
const NON_EVIDENCE_LABELS: &[&str] = &["src", "dst", "proto", "technique"];

/// MITRE ATT&CK metadata for a technique ID the netring sensor tags on anomalies
/// (#117): the human tactic it belongs to and a deep link to the ATT&CK page.
/// Returns `None` for unknown IDs.
fn attack_meta(technique: &str) -> Option<(&'static str, String)> {
    let tactic = match technique {
        "T1046" => "Discovery",
        "T1071" | "T1071.004" | "T1568" | "T1568.002" => "Command & Control",
        "T1021.001" | "T1021.002" => "Lateral Movement",
        "T1499" | "T1565" => "Impact",
        "T1040" => "Credential Access",
        // Network Boundary Bridging (#323): the netlink sentinel tags policy-
        // routing rule expectation violations (traffic diversion) with T1599.
        "T1599" => "Defense Evasion",
        _ => return None,
    };
    // Sub-techniques deep-link as /Txxxx/00n/.
    let url = match technique.split_once('.') {
        Some((base, sub)) => format!("https://attack.mitre.org/techniques/{base}/{sub}/"),
        None => format!("https://attack.mitre.org/techniques/{technique}/"),
    };
    Some((tactic, url))
}

/// The ATT&CK technique tagged on an anomaly, if any (#117).
fn anomaly_technique(a: &Alert) -> Option<&str> {
    a.labels.get("technique").map(String::as_str)
}

/// Map a detector `rule` slug to a human title + one-line "what it means",
/// so each detector reads as a first-class card rather than a raw slug. Covers
/// the built-in detectors and the netring 0.27 threat-intel kinds (flow-risk /
/// IOC / Sigma / cleartext SNMP, #69/#72); unknown slugs fall back to the slug
/// itself with no description.
fn detector_meta(rule: &str) -> (String, &'static str) {
    match rule {
        // Built-in flow/DNS detectors (Pillar A).
        "PortScanTRW" => ("Port scan".into(), "Many ports probed on a host (TRW)"),
        "BeaconCv" => (
            "Beaconing / C2".into(),
            "Periodic, size-consistent flows (RITA-style)",
        ),
        "ConnectionFlood" => (
            "Connection flood".into(),
            "Many connections to one (dst, port) in a window",
        ),
        "DgaScorer" => (
            "DGA / DNS tunneling".into(),
            "Random-looking DNS query names (low bigram likelihood)",
        ),
        // netring 0.27 threat-intel (#69): flow-risk scoring.
        "obsolete_tls" => (
            "Obsolete TLS".into(),
            "Deprecated TLS version / cipher negotiated",
        ),
        "cleartext_http_credentials" => (
            "Cleartext HTTP credentials".into(),
            "Credentials sent over unencrypted HTTP",
        ),
        "flow_risk" => ("Flow risk".into(), "Passive nDPI-style flow-risk finding"),
        // IOC / Sigma.
        "ioc_match" => (
            "IOC match".into(),
            "Flow matched a known indicator of compromise",
        ),
        "sigma_match" => ("Sigma rule".into(), "Flow matched a Sigma detection rule"),
        // Cleartext SNMP (#72).
        "cleartext-snmp" => (
            "Cleartext SNMP".into(),
            "SNMP v1/v2c community string sent in cleartext",
        ),
        other => {
            // Sentinel policy-rule expectations (#323) carry a `rules:<name>` slug.
            if let Some(name) = other.strip_prefix("rules:") {
                return (
                    format!("Policy rule: {name}"),
                    "Policy-routing rule forbidden/required by the netlink sentinel",
                );
            }
            (other.to_string(), "")
        }
    }
}

/// Render the security view.
pub fn security_view<'a>(
    alerts: &'a AlertsState,
    sec: &'a SecurityState,
    tuning: &'a crate::view::detection_tuning::DetectionTuningState,
) -> Element<'a, Message> {
    // Anomalies, plus sentinel Expectation violations that carry an ATT&CK
    // technique label (#323: a policy-rule diversion is a security event, not
    // just an operational deviation) — so they appear in the tactic lens.
    let mut anomalies: Vec<&Alert> = alerts
        .active_external()
        .into_iter()
        .filter(|a| {
            a.kind == AlertKind::Anomaly
                || (a.kind == AlertKind::Expectation && a.labels.contains_key("technique"))
        })
        .filter(|a| !sec.hide_info || a.severity != AlertSeverity::Info)
        .collect();
    // Stable order: severity desc, then most recent.
    anomalies.sort_by(|a, b| {
        sev_rank(b.severity)
            .cmp(&sev_rank(a.severity))
            .then(b.timestamp.cmp(&a.timestamp))
    });

    let content = column![
        render_header(anomalies.len(), sec),
        rule::horizontal(1),
        crate::view::detection_tuning::detection_tuning_panel(tuning),
        rule::horizontal(1),
        render_by_tactic(&anomalies),
        rule::horizontal(1),
        render_by_source(&anomalies),
        rule::horizontal(1),
        render_by_detector(&anomalies, sec),
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn sev_rank(s: AlertSeverity) -> u8 {
    match s {
        AlertSeverity::Critical => 3,
        AlertSeverity::Warning => 2,
        AlertSeverity::Info => 1,
    }
}

fn render_header<'a>(count: usize, sec: &SecurityState) -> Element<'a, Message> {
    let back = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseSecurity)
    .style(iced::widget::button::secondary);

    // Cross-link back to the operational Alerts surface (#39).
    let all_alerts = button(text("← All alerts").size(13))
        .on_press(Message::OpenAlerts)
        .style(iced::widget::button::secondary);

    // Severity filter toggle (#48).
    let filter = button(
        text(if sec.hide_info {
            "Show info"
        } else {
            "Hide info"
        })
        .size(13),
    )
    .on_press(Message::ToggleSecurityHideInfo)
    .style(if sec.hide_info {
        iced::widget::button::primary
    } else {
        iced::widget::button::secondary
    });

    let header_row = row![
        back,
        text("Security — Network Anomalies").size(22),
        text(format!("({count} active)")).size(13).style(dim),
        all_alerts,
        filter,
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    let subtitle = text("Network security anomalies — scans, beacons, DGA, flow-risk, IOC, Sigma")
        .size(font::CAPTION)
        .style(dim);

    column![header_row, subtitle].spacing(4).into()
}

/// Group anomalies by MITRE ATT&CK tactic (#117) — the analyst-grade lens every
/// NDR console leads with. Anomalies whose detector carries no technique fall
/// into an "Untagged" bucket so nothing is hidden.
fn render_by_tactic<'a>(anomalies: &[&'a Alert]) -> Element<'a, Message> {
    let title = text("By ATT&CK tactic").size(18);
    // tactic -> (count, set of technique IDs seen)
    let mut by_tactic: BTreeMap<&'static str, (usize, std::collections::BTreeSet<String>)> =
        BTreeMap::new();
    for a in anomalies {
        let (tactic, tech) = match anomaly_technique(a) {
            Some(t) => (attack_meta(t).map(|(ta, _)| ta).unwrap_or("Other"), Some(t)),
            None => ("Untagged", None),
        };
        let entry = by_tactic.entry(tactic).or_default();
        entry.0 += 1;
        if let Some(t) = tech {
            entry.1.insert(t.to_string());
        }
    }
    if by_tactic.is_empty() {
        return column![title, empty_state("No anomalies", None)]
            .spacing(8)
            .into();
    }
    let mut ranked: Vec<(&'static str, (usize, std::collections::BTreeSet<String>))> =
        by_tactic.into_iter().collect();
    ranked.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));

    let mut list = Column::new().spacing(4);
    for (tactic, (n, techs)) in &ranked {
        let techs_line = if techs.is_empty() {
            String::new()
        } else {
            techs.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        list = list.push(
            row![
                text(tactic.to_string())
                    .size(13)
                    .width(Length::Fixed(180.0)),
                text(format!("{n} anomalies")).size(12).style(dim),
                text(techs_line).size(11).style(dim),
            ]
            .spacing(10),
        );
    }
    column![title, list].spacing(8).into()
}

/// Rank offending sources by anomaly count.
fn render_by_source<'a>(anomalies: &[&'a Alert]) -> Element<'a, Message> {
    let title = text("Top offenders").size(18);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for a in anomalies {
        let src = a
            .labels
            .get("src")
            .cloned()
            .unwrap_or_else(|| a.source.clone());
        *counts.entry(src).or_default() += 1;
    }
    if counts.is_empty() {
        return column![title, empty_state("No anomalies", None)]
            .spacing(8)
            .into();
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let mut list = Column::new().spacing(4);
    for (src, n) in ranked.iter().take(20) {
        list = list.push(
            row![
                text(src.clone()).size(13).width(Length::Fixed(240.0)),
                text(format!("{n} anomalies")).size(12).style(dim),
            ]
            .spacing(10),
        );
    }
    column![title, list].spacing(8).into()
}

/// Group anomalies by detector (`rule`) and render one card per detector, each
/// row clickable to expand its evidence labels (#48).
fn render_by_detector<'a>(anomalies: &[&'a Alert], sec: &'a SecurityState) -> Element<'a, Message> {
    let title = text("By detector").size(18);
    if anomalies.is_empty() {
        return column![
            title,
            text("Quiet — no active anomalies").size(13).style(dim)
        ]
        .spacing(8)
        .into();
    }

    // Preserve the incoming (severity-sorted) order within each detector group.
    let mut groups: Vec<(String, Vec<&Alert>)> = Vec::new();
    for a in anomalies {
        match groups.iter_mut().find(|(r, _)| *r == a.rule) {
            Some((_, v)) => v.push(a),
            None => groups.push((a.rule.clone(), vec![a])),
        }
    }
    groups.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));

    let mut col = Column::new().spacing(12).push(title);
    for (rule_name, group) in groups {
        let top_sev = group
            .iter()
            .map(|a| a.severity)
            .max_by_key(|s| sev_rank(*s))
            .unwrap_or(AlertSeverity::Info);
        let (title_text, description) = detector_meta(&rule_name);
        let mut header = row![
            badge(Severity::from(top_sev).color(), title_text),
            text(format!("{} detection(s)", group.len()))
                .size(font::CAPTION)
                .style(dim),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        // ATT&CK technique badge (#117) — the lingua franca of every NDR console.
        if let Some(tech) = group.first().and_then(|a| anomaly_technique(a)) {
            let tactic = attack_meta(tech).map(|(t, _)| t).unwrap_or("");
            let label = if tactic.is_empty() {
                format!("ATT&CK {tech}")
            } else {
                format!("ATT&CK {tech} · {tactic}")
            };
            header = header.push(badge(theme::ACCENT_ANOMALY, label));
        }

        let mut card_col = Column::new().spacing(4).push(header);
        // First-class "what this detector means" line (empty for unknown slugs).
        if !description.is_empty() {
            card_col = card_col.push(text(description).size(font::CAPTION).style(dim));
        }
        for a in group {
            card_col = card_col.push(render_anomaly_row(a, sec));
        }
        let card_body: Element<'a, Message> = card_col.into();
        col = col.push(crate::view::components::card(card_body));
    }
    col.into()
}

/// One anomaly row: clickable to toggle its evidence drill-down (#48).
fn render_anomaly_row<'a>(a: &'a Alert, sec: &'a SecurityState) -> Element<'a, Message> {
    let key = a.alert_key();
    let expanded = sec.selected.as_deref() == Some(key.as_str());
    let sev: Severity = a.severity.into();
    let color = sev.color();

    let summary_line = row![
        text(if expanded { "▾" } else { "▸" }).size(11),
        text(a.summary.clone()).size(13).width(Length::Fixed(420.0)),
        text(sev.name())
            .size(11)
            .style(move |_t: &Theme| text::Style { color: Some(color) }),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Clicking toggles the drill-down (select, or deselect if already open).
    let toggle = button(summary_line)
        .on_press(Message::SelectAnomaly(if expanded {
            None
        } else {
            Some(key.clone())
        }))
        .style(iced::widget::button::text)
        .padding(2);

    if !expanded {
        return toggle.into();
    }

    // Drill-down: endpoints + every evidence label the detector emitted (the
    // "why it fired" the view used to discard).
    let mut detail = Column::new().spacing(2);
    if let Some(src) = a.labels.get("src") {
        detail = detail.push(evidence_line("src", src));
    }
    if let Some(dst) = a.labels.get("dst") {
        detail = detail.push(evidence_line("dst", dst));
    }
    let mut evidence: Vec<(&String, &String)> = a
        .labels
        .iter()
        .filter(|(k, _)| !NON_EVIDENCE_LABELS.contains(&k.as_str()))
        .collect();
    evidence.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in evidence {
        detail = detail.push(evidence_line(k, v));
    }
    if a.labels.is_empty() {
        detail = detail.push(text("no evidence labels").size(font::CAPTION).style(dim));
    }

    // Flow drill-down pivot (#119) — the central NDR workflow: from a detection,
    // pull the netring flows for the offending source. Only detections that name
    // a source can pivot.
    if let Some(src) = a.labels.get("src") {
        let loading = sec.flows_for.as_deref() == Some(key.as_str()) && sec.flows.is_loading();
        let pivot = button(
            text(if loading {
                "Fetching flows…"
            } else {
                "Show flows"
            })
            .size(11),
        )
        .padding([3, 9])
        .style(iced::widget::button::secondary)
        .on_press(Message::FetchAnomalyFlows {
            key: key.clone(),
            src: src.clone(),
        });
        detail = detail.push(pivot);

        if sec.flows_for.as_deref() == Some(key.as_str()) {
            detail = detail.push(render_pivot_flows(&sec.flows, sec));
        }
    }

    // Evidence-file pivot (#327): a triggered capture that fired on this
    // detector around this time offers the packets for download.
    if let Some(cap) = matching_capture(a, sec.captures.ready()) {
        let size_mib = cap.bytes as f64 / (1024.0 * 1024.0);
        let mut line = row![
            text(format!(
                "Capture available: {} · {} pkts · {size_mib:.1} MiB",
                cap.filename, cap.packets
            ))
            .size(11)
            .style(dim),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(id) = &cap.artifact_id {
            line = line.push(
                button(text("Download").size(11))
                    .padding([3, 9])
                    .style(iced::widget::button::secondary)
                    .on_press(Message::DownloadCaptureBlob {
                        producer: "netring".to_string(),
                        artifact_id: id.clone(),
                        filename: cap.filename.clone(),
                    }),
            );
        } else {
            line = line.push(text("(expired — on sensor disk only)").size(10).style(dim));
        }
        detail = detail.push(line);
    }

    column![toggle, container(detail).padding([4, 20]),]
        .spacing(2)
        .into()
}

/// Render the flow-pivot result table for the expanded anomaly (#119), with a
/// per-row flow↔process join affordance (#309).
fn render_pivot_flows<'a>(
    flows: &'a Fetch<Vec<FlowRecord>>,
    sec: &'a SecurityState,
) -> Element<'a, Message> {
    if let Some(err) = flows.error() {
        return text(format!("flow fetch failed: {err}"))
            .size(font::CAPTION)
            .style(dim)
            .into();
    }
    let Some(records) = flows.ready() else {
        return Column::new().into();
    };
    if records.is_empty() {
        return text("no matching recent flows")
            .size(font::CAPTION)
            .style(dim)
            .into();
    }
    let mut list = Column::new().spacing(2).push(
        row![
            text("src").size(10).width(Length::Fixed(170.0)),
            text("dst").size(10).width(Length::Fixed(170.0)),
            text("proto").size(10).width(Length::Fixed(50.0)),
            text("bytes").size(10).width(Length::Fixed(80.0)),
            text("community_id").size(10).width(Length::Fixed(260.0)),
            text("process").size(10).width(Length::Fixed(60.0)),
        ]
        .spacing(8),
    );
    for f in records.iter().take(100) {
        // Flow ↔ process join (#309): "this beacon is curl run by uid 1000".
        let who = button(text("who?").size(11))
            .padding([1, 6])
            .style(iced::widget::button::text)
            .on_press(Message::FetchFlowAttribution {
                target: crate::message::AttributionTarget::Security,
                key: crate::view::specialized::attribution::flow_key(&f.src, &f.dst),
                src: f.src.clone(),
                dst: f.dst.clone(),
            });
        list = list.push(
            row![
                text(f.src.clone()).size(11).width(Length::Fixed(170.0)),
                text(f.dst.clone()).size(11).width(Length::Fixed(170.0)),
                text(f.proto.clone()).size(11).width(Length::Fixed(50.0)),
                text(f.bytes.to_string())
                    .size(11)
                    .width(Length::Fixed(80.0)),
                text(f.community_id.clone().unwrap_or_else(|| "-".into()))
                    .size(11)
                    .width(Length::Fixed(260.0)),
                who,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    let mut colm = column![
        text(format!("{} flows for source", records.len()))
            .size(font::CAPTION)
            .style(dim),
        list,
    ]
    .spacing(2);
    // The last-asked row's attribution outcome, labelled with its source.
    if let Some((key, fetch)) = &sec.attribution {
        let line: Element<'a, Message> = match fetch {
            Fetch::Idle | Fetch::Loading => text(format!("{key}: looking up owning process…"))
                .size(11)
                .style(dim)
                .into(),
            Fetch::Error(e) => text(format!("{key}: unattributed ({e})"))
                .size(11)
                .style(dim)
                .into(),
            Fetch::Ready(Some(a)) => {
                text(format!("{key}: {} — endpoint {}", a.display(), a.endpoint))
                    .size(11)
                    .into()
            }
            Fetch::Ready(None) => text(format!(
                "{key}: unattributed (no matching socket on any netlink host)"
            ))
            .size(11)
            .style(dim)
            .into(),
        };
        colm = colm.push(line);
    }
    colm.into()
}

fn evidence_line<'a>(k: &str, v: &str) -> Element<'a, Message> {
    row![
        text(format!("{k}:"))
            .size(11)
            .width(Length::Fixed(160.0))
            .style(dim),
        text(v.to_string()).size(11),
    ]
    .spacing(8)
    .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

/// Count of active anomaly alerts (for the sidebar badge).
pub fn anomaly_count(alerts: &AlertsState) -> usize {
    alerts
        .external
        .values()
        .filter(|a| a.kind == AlertKind::Anomaly && a.severity != AlertSeverity::Info)
        .count()
}

#[cfg(test)]
mod tests {
    use zensight_common::{CaptureRecord, Protocol};

    use super::*;

    fn anomaly(rule: &str, ts: i64) -> Alert {
        let mut a = Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::Anomaly,
            rule,
            AlertSeverity::Warning,
            "test",
        );
        a.timestamp = ts;
        a
    }

    fn capture(kind: &str, start_ms: i64, end_ms: i64) -> CaptureRecord {
        CaptureRecord {
            filename: format!("zensight-trigger-{kind}.pcap.zst"),
            mode: "triggered".into(),
            trigger_kind: Some(kind.into()),
            start_ms,
            end_ms,
            ..Default::default()
        }
    }

    #[test]
    fn matching_capture_joins_on_kind_and_window() {
        let caps = vec![
            capture("PortScanTRW", 1_000_000, 1_010_000),
            capture("BeaconRita", 2_000_000, 2_010_000),
        ];
        // Same kind, timestamp inside the window (+slack).
        let hit = matching_capture(&anomaly("BeaconRita", 2_005_000), Some(&caps));
        assert_eq!(hit.unwrap().trigger_kind.as_deref(), Some("BeaconRita"));
        // Slack absorbs the detector-to-flush gap.
        assert!(matching_capture(&anomaly("BeaconRita", 1_950_000), Some(&caps)).is_some());
        // Wrong kind → no match even in-window.
        assert!(matching_capture(&anomaly("DgaScorer", 2_005_000), Some(&caps)).is_none());
        // Way outside the window → no match.
        assert!(matching_capture(&anomaly("BeaconRita", 9_000_000), Some(&caps)).is_none());
        // No captures fetched yet → graceful None.
        assert!(matching_capture(&anomaly("BeaconRita", 2_005_000), None).is_none());
    }

    #[test]
    fn matching_capture_skips_rotating_spool_files() {
        let mut c = capture("manual", 0, i64::MAX / 2);
        c.mode = "rotating".into();
        c.trigger_kind = None;
        assert!(matching_capture(&anomaly("manual", 1000), Some(&vec![c])).is_none());
    }
}
