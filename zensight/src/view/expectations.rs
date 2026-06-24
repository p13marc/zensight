//! Expectations authoring view — declare host expectations and push them to the
//! netlink sentinel over the Zenoh command channel (Plan 08).

use iced::widget::{Column, column, container, pick_list, row, rule, scrollable, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use crate::message::Message;
use crate::view::alerts::Severity;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use zensight_common::ComparisonOp;

/// The kind of expectation being authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpKind {
    /// A TCP port that must be listening.
    SocketListen,
    /// A TCP port that must NOT be listening.
    SocketForbid,
    /// An interface that must be up.
    LinkUp,
    /// A metric must satisfy `<op> <value>` (the generic threshold; lets any
    /// netlink metric — conntrack utilization, socket retransmits, … — be alerted
    /// on without a restart).
    MetricThreshold,
}

impl ExpKind {
    pub const ALL: &'static [ExpKind] = &[
        ExpKind::SocketListen,
        ExpKind::SocketForbid,
        ExpKind::LinkUp,
        ExpKind::MetricThreshold,
    ];
    fn label(&self) -> &'static str {
        match self {
            ExpKind::SocketListen => "Socket must listen",
            ExpKind::SocketForbid => "Socket must NOT listen",
            ExpKind::LinkUp => "Interface must be up",
            ExpKind::MetricThreshold => "Metric threshold",
        }
    }
}

impl std::fmt::Display for ExpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// One row of the currently-configured expectation set (from a status query).
#[derive(Debug, Clone)]
pub struct ExpRow {
    pub rule: String,
    pub detail: String,
    pub severity: String,
}

/// State for the expectations authoring view.
#[derive(Debug)]
pub struct ExpectationsState {
    pub new_kind: ExpKind,
    /// Name (socket) or interface (link) or rule label (metric).
    pub new_name: String,
    pub new_port: String,
    pub new_severity: Severity,
    /// Metric-threshold fields.
    pub new_metric: String,
    pub new_op: ComparisonOp,
    pub new_value: String,
    /// Current expectation set fetched from the sensor's status queryable.
    pub current: Vec<ExpRow>,
    pub status_note: Option<String>,
}

impl Default for ExpectationsState {
    fn default() -> Self {
        Self {
            new_kind: ExpKind::SocketListen,
            new_name: String::new(),
            new_port: String::new(),
            new_severity: Severity::Critical,
            new_metric: String::new(),
            new_op: ComparisonOp::GreaterThan,
            new_value: String::new(),
            current: Vec::new(),
            status_note: None,
        }
    }
}

/// Render the expectations authoring view.
pub fn expectations_view(state: &ExpectationsState) -> Element<'_, Message> {
    let content = column![
        render_header(),
        rule::horizontal(1),
        render_form(state),
        rule::horizontal(1),
        render_current(state),
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_header<'a>() -> Element<'a, Message> {
    let back = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseExpectations)
    .style(iced::widget::button::secondary);

    let refresh = button(text("Refresh").size(13))
        .on_press(Message::RefreshExpectations)
        .style(iced::widget::button::secondary);

    row![
        back,
        text("Expectations (netlink sentinel)").size(22),
        refresh
    ]
    .spacing(15)
    .align_y(Alignment::Center)
    .into()
}

fn render_form(state: &ExpectationsState) -> Element<'_, Message> {
    let kind = pick_list(
        ExpKind::ALL,
        Some(state.new_kind),
        Message::SetExpectationKind,
    )
    .width(Length::Fixed(220.0));

    let is_link = state.new_kind == ExpKind::LinkUp;
    let is_metric = state.new_kind == ExpKind::MetricThreshold;
    let name_placeholder = if is_link {
        "interface (eth0)"
    } else if is_metric {
        "rule name"
    } else {
        "name (sshd)"
    };
    let name = text_input(name_placeholder, &state.new_name)
        .on_input(Message::SetExpectationName)
        .padding(8)
        .width(Length::Fixed(140.0));

    let mut form = row![kind, name].spacing(10).align_y(Alignment::Center);

    if is_metric {
        // metric path + operator + threshold value.
        form = form.push(
            text_input("metric (conntrack/utilization)", &state.new_metric)
                .on_input(Message::SetExpectationMetric)
                .padding(8)
                .width(Length::Fixed(220.0)),
        );
        form = form.push(
            pick_list(
                ComparisonOp::ALL,
                Some(state.new_op),
                Message::SetExpectationOp,
            )
            .width(Length::Fixed(70.0)),
        );
        form = form.push(
            text_input("value", &state.new_value)
                .on_input(Message::SetExpectationValue)
                .padding(8)
                .width(Length::Fixed(90.0)),
        );
    } else if !is_link {
        let port = text_input("port", &state.new_port)
            .on_input(Message::SetExpectationPort)
            .padding(8)
            .width(Length::Fixed(90.0));
        form = form.push(port);
    }

    let severity = pick_list(
        Severity::ALL,
        Some(state.new_severity),
        Message::SetExpectationSeverity,
    )
    .width(Length::Fixed(120.0));

    let add = button(text("Add & Push").size(13))
        .on_press(Message::AddExpectation)
        .style(iced::widget::button::primary);

    column![
        text("Declare an expectation").size(18),
        form.push(severity).push(add),
        text("Pushed to all netlink sensors via the command channel.")
            .size(11)
            .style(dim),
    ]
    .spacing(10)
    .into()
}

fn render_current(state: &ExpectationsState) -> Element<'_, Message> {
    let title = text(format!("Configured ({})", state.current.len())).size(18);

    if state.current.is_empty() {
        let note = state
            .status_note
            .clone()
            .unwrap_or_else(|| "Press Refresh to load the current set.".into());
        return column![title, text(note).size(13).style(dim)]
            .spacing(8)
            .into();
    }

    let mut list = Column::new().spacing(5);
    for r in &state.current {
        let remove = button(text("Remove").size(11))
            .on_press(Message::RemoveExpectation(r.rule.clone()))
            .style(iced::widget::button::danger);
        list = list.push(
            row![
                text(&r.rule).size(13).width(Length::Fixed(200.0)),
                text(&r.detail).size(12).width(Length::Fixed(220.0)),
                text(&r.severity).size(11).width(Length::Fixed(80.0)),
                remove,
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    column![title, list].spacing(10).into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}

/// Parse a sentinel status reply (an `ExpectationsConfig` JSON) into rows.
pub fn parse_status(json: &str) -> Vec<ExpRow> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut rows = Vec::new();
    if let Some(sockets) = v.get("sockets").and_then(|s| s.as_array()) {
        for s in sockets {
            let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            let detail = if let Some(p) = s.get("listen").and_then(|x| x.as_u64()) {
                format!("listen :{p}")
            } else if let Some(p) = s.get("forbid_listen").and_then(|x| x.as_u64()) {
                format!("forbid :{p}")
            } else if let Some(t) = s.get("established_to").and_then(|x| x.as_str()) {
                format!("established → {t}")
            } else {
                "socket".into()
            };
            rows.push(ExpRow {
                rule: format!("socket:{name}"),
                detail,
                severity: s
                    .get("severity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("warning")
                    .to_string(),
            });
        }
    }
    if let Some(links) = v.get("links").and_then(|s| s.as_array()) {
        for l in links {
            let iface = l.get("iface").and_then(|x| x.as_str()).unwrap_or("?");
            let up = l.get("up").and_then(|x| x.as_bool()).unwrap_or(true);
            rows.push(ExpRow {
                rule: format!("link:{iface}"),
                detail: if up {
                    "must be up".into()
                } else {
                    "must be down".into()
                },
                severity: l
                    .get("severity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("warning")
                    .to_string(),
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_sockets_and_links() {
        let json = r#"{"eval_interval_secs":10,"default_for_secs":15,
            "sockets":[{"name":"sshd","listen":22,"severity":"critical"},
                       {"name":"no-telnet","forbid_listen":23,"severity":"warning"}],
            "links":[{"iface":"eth0","up":true,"severity":"critical"}]}"#;
        let rows = parse_status(json);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].rule, "socket:sshd");
        assert_eq!(rows[0].detail, "listen :22");
        assert_eq!(rows[1].detail, "forbid :23");
        assert_eq!(rows[2].rule, "link:eth0");
        assert_eq!(rows[2].detail, "must be up");
    }

    #[test]
    fn parse_status_empty_or_bad() {
        assert!(parse_status("not json").is_empty());
        assert!(parse_status(r#"{"sockets":[],"links":[]}"#).is_empty());
    }
}
