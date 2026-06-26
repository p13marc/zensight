//! Unified **Incident** object (#129) — turns the flat alert stream into a triage
//! surface.
//!
//! An [`Incident`] groups the currently-firing sensor alerts for one host into a
//! single object with a merged **timeline** (firing→resolved transitions across
//! all member alerts) and **evidence** anchors that pivot to the offending
//! metric, netring flows, and the host's logs. Grouping is by host (the
//! PagerDuty "same-entity coalesce" model) over the existing `alert_key` dedup;
//! the per-key transition history becomes the incident timeline.
//!
//! The model + [`group_incidents`] are pure and unit-tested; [`incidents_view`]
//! renders worst-first cards with one-click evidence pivots (which reuse the
//! existing `InvestigateAlert` navigation). Additive over the `Alert` stream —
//! no wire/keyspace change.

use std::collections::BTreeMap;

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length, Theme};

use zensight_common::{Alert as SensorAlert, AlertSeverity, AlertState, Protocol};

use crate::message::{DeviceId, Message};
use crate::view::alerts::{AlertsState, Severity};
use crate::view::components::{badge, card, empty_state, section_header};
use crate::view::formatting::format_timestamp;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// One entry in an incident's merged timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentEvent {
    pub at: i64,
    pub rule: String,
    pub state: AlertState,
    pub summary: String,
}

/// Cross-domain evidence anchors for an incident's pivots (#129). Derived from
/// the member alerts' labels: the netlink sentinel tags a `metric`, netring
/// detectors tag the offending `src` IP; logs are always host-scoped.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub host: String,
    /// Protocol of the sensor that raised the incident (its own device).
    pub protocol: Protocol,
    /// Offending metric path (from a `metric` label), if any.
    pub metric: Option<String>,
    /// Offending source IP (from a `src` label on an anomaly), if any.
    pub flow_src: Option<String>,
}

/// A unified incident: the firing alerts for one host, with a timeline + evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Incident {
    /// Stable id for selection/expansion (`inc-<host>`).
    pub id: String,
    pub host: String,
    pub severity: AlertSeverity,
    pub started: i64,
    pub last_change: i64,
    pub summary: String,
    pub alert_keys: Vec<String>,
    /// How many member alerts are not yet acknowledged.
    pub unacked: usize,
    pub timeline: Vec<IncidentEvent>,
    pub evidence: Evidence,
}

/// Group currently-firing alerts into incidents (#129). Pure: callers pass the
/// firing set plus closures for the ack flag and per-key transition history, so
/// this is testable without an `AlertsState`.
///
/// Alerts coalesce by host; severity is the max across members, the timeline is
/// the member transitions merged oldest-first, and evidence is taken from the
/// first member carrying each label. Output is worst-first (severity, then
/// unacked, then most-recent).
pub fn group_incidents(
    firing: &[&SensorAlert],
    is_acked: impl Fn(&str) -> bool,
    timeline_of: impl Fn(&str) -> Vec<(AlertState, i64)>,
) -> Vec<Incident> {
    let mut by_host: BTreeMap<&str, Vec<&SensorAlert>> = BTreeMap::new();
    for a in firing {
        by_host.entry(a.source.as_str()).or_default().push(a);
    }

    let mut out: Vec<Incident> = Vec::new();
    for (host, alerts) in by_host {
        let severity = alerts
            .iter()
            .map(|a| a.severity)
            .max()
            .unwrap_or(AlertSeverity::Info);

        let mut keys: Vec<String> = Vec::new();
        let mut events: Vec<IncidentEvent> = Vec::new();
        for a in &alerts {
            let key = a.alert_key();
            for (state, at) in timeline_of(&key) {
                events.push(IncidentEvent {
                    at,
                    rule: a.rule.clone(),
                    state,
                    summary: a.summary.clone(),
                });
            }
            keys.push(key);
        }
        events.sort_by_key(|e| e.at);

        let started = events
            .first()
            .map(|e| e.at)
            .unwrap_or_else(|| alerts.iter().map(|a| a.timestamp).min().unwrap_or(0));
        let last_change = events
            .last()
            .map(|e| e.at)
            .unwrap_or_else(|| alerts.iter().map(|a| a.timestamp).max().unwrap_or(0));
        let unacked = keys.iter().filter(|k| !is_acked(k)).count();

        // Representative alert: highest severity, then most recent.
        let top = alerts
            .iter()
            .copied()
            .max_by(|a, b| {
                a.severity
                    .cmp(&b.severity)
                    .then(a.timestamp.cmp(&b.timestamp))
            })
            .expect("host group is non-empty");

        let evidence = Evidence {
            host: host.to_string(),
            protocol: top.protocol,
            metric: alerts.iter().find_map(|a| a.labels.get("metric").cloned()),
            flow_src: alerts.iter().find_map(|a| a.labels.get("src").cloned()),
        };

        out.push(Incident {
            id: format!("inc-{host}"),
            host: host.to_string(),
            severity,
            started,
            last_change,
            summary: top.summary.clone(),
            alert_keys: keys,
            unacked,
            timeline: events,
            evidence,
        });
    }

    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.unacked.cmp(&a.unacked))
            .then(b.last_change.cmp(&a.last_change))
    });
    out
}

/// View state for the Incidents screen (#129): which incident is expanded.
#[derive(Debug, Clone, Default)]
pub struct IncidentsState {
    /// Expanded incident id, if any.
    pub selected: Option<String>,
}

/// Render the Incidents view: worst-first cards, each expandable to a timeline +
/// evidence pivots (#129).
pub fn incidents_view<'a>(
    alerts: &'a AlertsState,
    state: &'a IncidentsState,
) -> Element<'a, Message> {
    let incidents = alerts.incidents();
    let mut content = column![section_header(
        format!("Incidents ({})", incidents.len()),
        None
    )]
    .spacing(space::MD);

    if incidents.is_empty() {
        content = content.push(empty_state(
            "No active incidents — all clear, or no sensor alerts firing",
            None,
        ));
    } else {
        for inc in &incidents {
            content = content.push(card(render_incident(inc, state)));
        }
    }

    container(scrollable(content.padding(space::LG)))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_incident<'a>(inc: &Incident, state: &IncidentsState) -> Element<'a, Message> {
    let sev_color = Severity::from(inc.severity).color();
    let expanded = state.selected.as_deref() == Some(inc.id.as_str());

    // Header row: severity badge · host · summary · age · counts. The whole row
    // toggles expansion.
    let mut header = row![
        badge::<Message>(sev_color, Severity::from(inc.severity).name().to_string()),
        text(inc.host.clone()).size(font::EMPHASIS),
        text(inc.summary.clone()).size(font::CAPTION).style(dim),
    ]
    .spacing(space::SM)
    .align_y(iced::Alignment::Center);
    header = header.push(
        text(format!(
            "{} alert(s) · started {}",
            inc.alert_keys.len(),
            format_timestamp(inc.started)
        ))
        .size(font::CAPTION)
        .style(dim),
    );
    if inc.unacked > 0 {
        header = header.push(badge::<Message>(
            sev_color,
            format!("{} unacked", inc.unacked),
        ));
    }

    let toggle = if expanded { None } else { Some(inc.id.clone()) };
    let header_btn = button(header)
        .on_press(Message::SelectIncident(toggle))
        .padding(space::XS)
        .style(iced::widget::button::text)
        .width(Length::Fill);

    let mut col = column![header_btn].spacing(space::SM);
    if expanded {
        col = col.push(render_timeline(inc)).push(render_evidence(inc));
    }
    col.into()
}

/// The merged firing→resolved timeline strip, most-recent first.
fn render_timeline<'a>(inc: &Incident) -> Element<'a, Message> {
    if inc.timeline.is_empty() {
        return text("(no recorded transitions)")
            .size(font::CAPTION)
            .style(dim)
            .into();
    }
    let mut col = column![text("Timeline").size(font::CAPTION).style(dim)].spacing(2);
    for ev in inc.timeline.iter().rev().take(20) {
        let (label, color) = match ev.state {
            AlertState::Firing => ("firing", Severity::Critical.color()),
            AlertState::Resolved => ("resolved", Severity::Info.color()),
        };
        col = col.push(
            row![
                text(format_timestamp(ev.at))
                    .size(font::CAPTION)
                    .width(Length::Fixed(90.0))
                    .style(dim),
                badge::<Message>(color, label.to_string()),
                text(ev.rule.clone()).size(font::CAPTION),
            ]
            .spacing(space::SM)
            .align_y(iced::Alignment::Center),
        );
    }
    col.into()
}

/// Evidence pivots: one-click navigation to the offending metric / flows / logs /
/// host. All reuse the existing `InvestigateAlert` navigation, which opens the
/// right device view (prefetch-on-open populates flows/sockets/etc).
fn render_evidence<'a>(inc: &Incident) -> Element<'a, Message> {
    let host = inc.evidence.host.clone();
    let pivot = |label: &str, device: DeviceId, metric: Option<String>| -> Element<'a, Message> {
        button(text(label.to_string()).size(font::CAPTION))
            .on_press(Message::InvestigateAlert { device, metric })
            .padding([2, 8])
            .into()
    };

    let mut row_items: Vec<Element<'a, Message>> =
        vec![text("Evidence:").size(font::CAPTION).style(dim).into()];

    // The host/device that raised the incident.
    row_items.push(pivot(
        "host ↗",
        DeviceId::new(inc.evidence.protocol, host.clone()),
        None,
    ));
    // Offending metric → device chart.
    if let Some(metric) = &inc.evidence.metric {
        row_items.push(pivot(
            "metric ↗",
            DeviceId::new(inc.evidence.protocol, host.clone()),
            Some(metric.clone()),
        ));
    }
    // Offending flows → netring device (its flow panel).
    if inc.evidence.flow_src.is_some() {
        row_items.push(pivot(
            "flows ↗",
            DeviceId::new(Protocol::Netring, host.clone()),
            None,
        ));
    }
    // Host logs → syslog device (filtered to this host).
    row_items.push(pivot(
        "logs ↗",
        DeviceId::new(Protocol::Logs, host.clone()),
        None,
    ));

    iced::widget::Row::with_children(row_items)
        .spacing(space::SM)
        .align_y(iced::Alignment::Center)
        .wrap()
        .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn alert(source: &str, rule: &str, sev: AlertSeverity, labels: &[(&str, &str)]) -> SensorAlert {
        let mut a = SensorAlert::new(
            source,
            Protocol::Netlink,
            zensight_common::AlertKind::Expectation,
            rule,
            sev,
            "summary",
        );
        let mut map = HashMap::new();
        for (k, v) in labels {
            map.insert(k.to_string(), v.to_string());
        }
        a.labels = map;
        a
    }

    #[test]
    fn groups_alerts_by_host_with_max_severity() {
        let a1 = alert("host1", "cpu", AlertSeverity::Warning, &[]);
        let a2 = alert("host1", "mem", AlertSeverity::Critical, &[]);
        let a3 = alert("host2", "disk", AlertSeverity::Info, &[]);
        let firing = vec![&a1, &a2, &a3];

        let incs = group_incidents(&firing, |_| false, |_| vec![]);
        assert_eq!(incs.len(), 2);
        // Worst-first: host1 (Critical) before host2 (Info).
        assert_eq!(incs[0].host, "host1");
        assert_eq!(incs[0].severity, AlertSeverity::Critical);
        assert_eq!(incs[0].alert_keys.len(), 2);
        assert_eq!(incs[1].host, "host2");
    }

    #[test]
    fn derives_metric_and_flow_evidence_from_labels() {
        let a1 = alert(
            "host1",
            "retrans",
            AlertSeverity::Warning,
            &[("metric", "sockets/tcp/retransmits_total")],
        );
        let mut a2 = alert(
            "host1",
            "beacon",
            AlertSeverity::Critical,
            &[("src", "10.0.0.5")],
        );
        a2.protocol = Protocol::Netring;
        let firing = vec![&a1, &a2];

        let incs = group_incidents(&firing, |_| false, |_| vec![]);
        assert_eq!(incs.len(), 1);
        let ev = &incs[0].evidence;
        assert_eq!(ev.metric.as_deref(), Some("sockets/tcp/retransmits_total"));
        assert_eq!(ev.flow_src.as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn unacked_count_and_timeline_merge() {
        let a1 = alert("h", "r1", AlertSeverity::Warning, &[]);
        let a2 = alert("h", "r2", AlertSeverity::Warning, &[]);
        let k1 = a1.alert_key();
        let firing = vec![&a1, &a2];

        // a1 acked; a1 has two transitions, a2 has one.
        let incs = group_incidents(
            &firing,
            |k| k == k1,
            |k| {
                if k == k1 {
                    vec![(AlertState::Firing, 100), (AlertState::Resolved, 300)]
                } else {
                    vec![(AlertState::Firing, 200)]
                }
            },
        );
        assert_eq!(incs.len(), 1);
        let inc = &incs[0];
        assert_eq!(inc.unacked, 1); // only a2 unacked
        // merged + sorted oldest-first: 100, 200, 300
        assert_eq!(
            inc.timeline.iter().map(|e| e.at).collect::<Vec<_>>(),
            vec![100, 200, 300]
        );
        assert_eq!(inc.started, 100);
        assert_eq!(inc.last_change, 300);
    }
}
