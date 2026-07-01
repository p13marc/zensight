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

/// Which sensor's sentinel we're authoring for (#278). Netlink uses incremental
/// add/remove commands; systemd uses a full-set `SetExpectations` replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpTarget {
    Netlink,
    Systemd,
}

impl ExpTarget {
    pub const ALL: &'static [ExpTarget] = &[ExpTarget::Netlink, ExpTarget::Systemd];
}

impl std::fmt::Display for ExpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ExpTarget::Netlink => "netlink",
                ExpTarget::Systemd => "systemd",
            }
        )
    }
}

/// The kind of systemd expectation being authored (#278).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemdExpKind {
    ServiceActive,
    TargetActive,
    TimerWithin,
    RestartRate,
    ForbidFailed,
}

impl SystemdExpKind {
    pub const ALL: &'static [SystemdExpKind] = &[
        SystemdExpKind::ServiceActive,
        SystemdExpKind::TargetActive,
        SystemdExpKind::TimerWithin,
        SystemdExpKind::RestartRate,
        SystemdExpKind::ForbidFailed,
    ];
    fn label(&self) -> &'static str {
        match self {
            SystemdExpKind::ServiceActive => "Service must be active",
            SystemdExpKind::TargetActive => "Target must be active",
            SystemdExpKind::TimerWithin => "Timer must fire within",
            SystemdExpKind::RestartRate => "Restart-rate ceiling",
            SystemdExpKind::ForbidFailed => "Forbid any failed unit",
        }
    }
}

impl std::fmt::Display for SystemdExpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// The accumulated systemd expectation set (#278). Mirrors the sensor's
/// `ExpectationsConfig`; the GUI edits it and pushes the whole thing via
/// `SetExpectations`.
#[derive(Debug, Clone)]
pub struct SystemdExpDraft {
    pub eval_interval_secs: u64,
    pub for_secs: u64,
    pub services: Vec<String>,
    pub targets: Vec<String>,
    pub timers: Vec<(String, u64)>,
    pub restart_rates: Vec<(String, u32, u64)>,
    pub forbid_failed: bool,
}

impl Default for SystemdExpDraft {
    fn default() -> Self {
        Self {
            eval_interval_secs: 10,
            for_secs: 15,
            services: Vec::new(),
            targets: Vec::new(),
            timers: Vec::new(),
            restart_rates: Vec::new(),
            forbid_failed: false,
        }
    }
}

impl SystemdExpDraft {
    /// Build the `SetExpectations` command payload (pure — unit-testable).
    pub fn to_command_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "set_expectations",
            "eval_interval_secs": self.eval_interval_secs,
            "for_secs": self.for_secs,
            "services_active": self.services.iter().map(|u| serde_json::json!({"unit": u})).collect::<Vec<_>>(),
            "targets_active": self.targets.iter().map(|t| serde_json::json!({"target": t})).collect::<Vec<_>>(),
            "timers": self.timers.iter().map(|(t, w)| serde_json::json!({"timer": t, "within_secs": w})).collect::<Vec<_>>(),
            "restart_rates": self.restart_rates.iter().map(|(u, m, w)| serde_json::json!({"unit": u, "max": m, "window_secs": w})).collect::<Vec<_>>(),
            "forbid_failed": self.forbid_failed,
        })
    }

    /// Parse a systemd `@/status/expectations` reply into a draft (pure).
    pub fn from_status(json: &str) -> Self {
        let v: serde_json::Value = match serde_json::from_str(json) {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let u64_at =
            |key: &str, default: u64| v.get(key).and_then(|x| x.as_u64()).unwrap_or(default);
        let arr = |key: &str| {
            v.get(key)
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
        };
        Self {
            eval_interval_secs: u64_at("eval_interval_secs", 10),
            for_secs: u64_at("for_secs", 15),
            services: arr("services_active")
                .iter()
                .filter_map(|s| s.get("unit").and_then(|x| x.as_str()).map(String::from))
                .collect(),
            targets: arr("targets_active")
                .iter()
                .filter_map(|s| s.get("target").and_then(|x| x.as_str()).map(String::from))
                .collect(),
            timers: arr("timers")
                .iter()
                .filter_map(|s| {
                    Some((
                        s.get("timer").and_then(|x| x.as_str())?.to_string(),
                        s.get("within_secs").and_then(|x| x.as_u64())?,
                    ))
                })
                .collect(),
            restart_rates: arr("restart_rates")
                .iter()
                .filter_map(|s| {
                    Some((
                        s.get("unit").and_then(|x| x.as_str())?.to_string(),
                        s.get("max").and_then(|x| x.as_u64())? as u32,
                        s.get("window_secs").and_then(|x| x.as_u64())?,
                    ))
                })
                .collect(),
            forbid_failed: v
                .get("forbid_failed")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        }
    }

    /// The configured-list rows for this draft.
    pub fn rows(&self) -> Vec<ExpRow> {
        let mut rows = Vec::new();
        for u in &self.services {
            rows.push(ExpRow {
                rule: format!("service:{u}"),
                detail: "must be active".into(),
                severity: "critical".into(),
            });
        }
        for t in &self.targets {
            rows.push(ExpRow {
                rule: format!("target:{t}"),
                detail: "must be active".into(),
                severity: "warning".into(),
            });
        }
        for (t, w) in &self.timers {
            rows.push(ExpRow {
                rule: format!("timer:{t}"),
                detail: format!("within {w}s"),
                severity: "warning".into(),
            });
        }
        for (u, m, w) in &self.restart_rates {
            rows.push(ExpRow {
                rule: format!("restart:{u}"),
                detail: format!("< {m}/{w}s"),
                severity: "warning".into(),
            });
        }
        if self.forbid_failed {
            rows.push(ExpRow {
                rule: "forbid:failed".into(),
                detail: "no failed units".into(),
                severity: "critical".into(),
            });
        }
        rows
    }

    /// Remove an entry addressed by its `rows()` rule key.
    pub fn remove_rule(&mut self, rule: &str) {
        if let Some(u) = rule.strip_prefix("service:") {
            self.services.retain(|s| s != u);
        } else if let Some(t) = rule.strip_prefix("target:") {
            self.targets.retain(|s| s != t);
        } else if let Some(t) = rule.strip_prefix("timer:") {
            self.timers.retain(|(name, _)| name != t);
        } else if let Some(u) = rule.strip_prefix("restart:") {
            self.restart_rates.retain(|(name, _, _)| name != u);
        } else if rule == "forbid:failed" {
            self.forbid_failed = false;
        }
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
    /// Which sensor's sentinel we're authoring for (#278).
    pub target: ExpTarget,
    pub new_kind: ExpKind,
    /// The systemd expectation kind (when `target == Systemd`).
    pub systemd_kind: SystemdExpKind,
    /// Name (socket) or interface (link) or rule label (metric) or systemd
    /// unit/target/timer.
    pub new_name: String,
    pub new_port: String,
    pub new_severity: Severity,
    /// Metric-threshold fields.
    pub new_metric: String,
    pub new_op: ComparisonOp,
    pub new_value: String,
    /// Current expectation set fetched from the sensor's status queryable.
    pub current: Vec<ExpRow>,
    /// The accumulated systemd expectation set (#278).
    pub systemd: SystemdExpDraft,
    pub status_note: Option<String>,
}

impl Default for ExpectationsState {
    fn default() -> Self {
        Self {
            target: ExpTarget::Netlink,
            new_kind: ExpKind::SocketListen,
            systemd_kind: SystemdExpKind::ServiceActive,
            new_name: String::new(),
            new_port: String::new(),
            new_severity: Severity::Critical,
            new_metric: String::new(),
            new_op: ComparisonOp::GreaterThan,
            new_value: String::new(),
            current: Vec::new(),
            systemd: SystemdExpDraft::default(),
            status_note: None,
        }
    }
}

/// Render the expectations authoring view.
pub fn expectations_view(state: &ExpectationsState) -> Element<'_, Message> {
    let form: Element<'_, Message> = match state.target {
        ExpTarget::Netlink => render_form(state),
        ExpTarget::Systemd => render_systemd_form(state),
    };
    let content = column![
        render_header(state),
        rule::horizontal(1),
        form,
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

fn render_header(state: &ExpectationsState) -> Element<'_, Message> {
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

    // Sensor target selector (#278): netlink vs systemd sentinel.
    let target = pick_list(ExpTarget::ALL, Some(state.target), Message::SetExpTarget)
        .width(Length::Fixed(120.0));

    row![
        back,
        text(format!("Expectations ({} sentinel)", state.target)).size(22),
        target,
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

/// The systemd expectation authoring form (#278). Unlike netlink's incremental
/// commands, each add mutates the accumulated draft and re-pushes the full set.
fn render_systemd_form(state: &ExpectationsState) -> Element<'_, Message> {
    let kind = pick_list(
        SystemdExpKind::ALL,
        Some(state.systemd_kind),
        Message::SetSystemdExpKind,
    )
    .width(Length::Fixed(220.0));

    let mut form = row![kind].spacing(10).align_y(Alignment::Center);

    // The name field (unit/target/timer) is hidden for the field-less
    // "forbid failed" kind.
    if state.systemd_kind != SystemdExpKind::ForbidFailed {
        let placeholder = match state.systemd_kind {
            SystemdExpKind::TargetActive => "target (multi-user.target)",
            SystemdExpKind::TimerWithin => "timer (logrotate.timer)",
            _ => "unit (sshd.service)",
        };
        form = form.push(
            text_input(placeholder, &state.new_name)
                .on_input(Message::SetExpectationName)
                .padding(8)
                .width(Length::Fixed(200.0)),
        );
    }
    match state.systemd_kind {
        SystemdExpKind::TimerWithin => {
            form = form.push(
                text_input("within (secs)", &state.new_value)
                    .on_input(Message::SetExpectationValue)
                    .padding(8)
                    .width(Length::Fixed(120.0)),
            );
        }
        SystemdExpKind::RestartRate => {
            form = form.push(
                text_input("max restarts", &state.new_value)
                    .on_input(Message::SetExpectationValue)
                    .padding(8)
                    .width(Length::Fixed(110.0)),
            );
            form = form.push(
                text_input("window (secs)", &state.new_port)
                    .on_input(Message::SetExpectationPort)
                    .padding(8)
                    .width(Length::Fixed(120.0)),
            );
        }
        _ => {}
    }

    let add = button(text("Add & Push").size(13))
        .on_press(Message::AddExpectation)
        .style(iced::widget::button::primary);

    column![
        text("Declare a systemd expectation").size(18),
        form.push(add),
        text("The full expectation set is pushed to the systemd sentinel via SetExpectations.")
            .size(11)
            .style(dim),
    ]
    .spacing(10)
    .into()
}

fn render_current(state: &ExpectationsState) -> Element<'_, Message> {
    let rows = match state.target {
        ExpTarget::Netlink => state.current.clone(),
        ExpTarget::Systemd => state.systemd.rows(),
    };
    let title = text(format!("Configured ({})", rows.len())).size(18);

    if rows.is_empty() {
        let note = state
            .status_note
            .clone()
            .unwrap_or_else(|| "Press Refresh to load the current set.".into());
        return column![title, text(note).size(13).style(dim)]
            .spacing(8)
            .into();
    }

    let mut list = Column::new().spacing(5);
    for r in rows {
        let remove = button(text("Remove").size(11))
            .on_press(Message::RemoveExpectation(r.rule.clone()))
            .style(iced::widget::button::danger);
        list = list.push(
            row![
                text(r.rule).size(13).width(Length::Fixed(200.0)),
                text(r.detail).size(12).width(Length::Fixed(220.0)),
                text(r.severity).size(11).width(Length::Fixed(80.0)),
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
    // Metric-threshold expectations (#50) — previously dropped, so a pushed
    // threshold was invisible/unremovable in the Configured list.
    if let Some(metrics) = v.get("metrics").and_then(|s| s.as_array()) {
        for m in metrics {
            let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            let metric = m.get("metric").and_then(|x| x.as_str()).unwrap_or("?");
            let op = m.get("op").and_then(|x| x.as_str()).unwrap_or("?");
            let value = m.get("value").and_then(|x| x.as_f64()).unwrap_or(0.0);
            rows.push(ExpRow {
                rule: format!("metric:{name}"),
                detail: format!("{metric} {op} {value}"),
                severity: m
                    .get("severity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("warning")
                    .to_string(),
            });
        }
    }
    if let Some(neighbors) = v.get("neighbors").and_then(|s| s.as_array()) {
        for n in neighbors {
            let ip = n.get("ip").and_then(|x| x.as_str()).unwrap_or("?");
            let reachable = n.get("reachable").and_then(|x| x.as_bool()).unwrap_or(true);
            rows.push(ExpRow {
                rule: format!("neighbor:{ip}"),
                detail: if reachable {
                    "must be reachable".into()
                } else {
                    "must be unreachable".into()
                },
                severity: n
                    .get("severity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("warning")
                    .to_string(),
            });
        }
    }
    if let Some(routes) = v.get("routes").and_then(|s| s.as_array()) {
        for r in routes {
            let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            let via = r.get("default_via").and_then(|x| x.as_str());
            rows.push(ExpRow {
                rule: format!("route:{name}"),
                detail: match via {
                    Some(gw) => format!("default via {gw}"),
                    None => "default present".into(),
                },
                severity: r
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
    fn parse_status_includes_metric_thresholds() {
        // #50: pushed metric/neighbor/route rules must be visible in the list.
        let rows = parse_status(
            r#"{"sockets":[],"links":[],
                "metrics":[{"name":"retx","metric":"sockets/tcp/retransmits_total","op":"GreaterThan","value":100.0,"severity":"critical"}],
                "neighbors":[{"ip":"10.0.0.1","reachable":true,"severity":"warning"}],
                "routes":[{"name":"default","default_present":true,"severity":"warning"}]}"#,
        );
        assert!(rows.iter().any(|r| r.rule == "metric:retx"
            && r.detail.contains("sockets/tcp/retransmits_total")
            && r.severity == "critical"));
        assert!(rows.iter().any(|r| r.rule == "neighbor:10.0.0.1"));
        assert!(rows.iter().any(|r| r.rule == "route:default"));
    }

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

    // ── systemd expectations authoring (#278) ──

    #[test]
    fn systemd_draft_command_shape() {
        let mut d = SystemdExpDraft::default();
        d.services.push("sshd.service".into());
        d.timers.push(("logrotate.timer".into(), 90_000));
        d.restart_rates.push(("nginx.service".into(), 5, 600));
        d.forbid_failed = true;
        let cmd = d.to_command_json();
        assert_eq!(cmd["type"], "set_expectations");
        assert_eq!(cmd["services_active"][0]["unit"], "sshd.service");
        assert_eq!(cmd["timers"][0]["within_secs"], 90_000);
        assert_eq!(cmd["restart_rates"][0]["max"], 5);
        assert_eq!(cmd["forbid_failed"], true);
    }

    #[test]
    fn systemd_draft_status_roundtrip() {
        let mut d = SystemdExpDraft::default();
        d.services.push("a.service".into());
        d.targets.push("multi-user.target".into());
        d.timers.push(("b.timer".into(), 120));
        d.forbid_failed = true;
        // The command payload drops the "type" tag but is otherwise the config.
        let json = serde_json::to_string(&d.to_command_json()).unwrap();
        let back = SystemdExpDraft::from_status(&json);
        assert_eq!(back.services, vec!["a.service".to_string()]);
        assert_eq!(back.targets, vec!["multi-user.target".to_string()]);
        assert_eq!(back.timers, vec![("b.timer".to_string(), 120)]);
        assert!(back.forbid_failed);
    }

    #[test]
    fn systemd_draft_rows_and_remove() {
        let mut d = SystemdExpDraft::default();
        d.services.push("sshd.service".into());
        d.forbid_failed = true;
        let rows = d.rows();
        assert!(rows.iter().any(|r| r.rule == "service:sshd.service"));
        assert!(rows.iter().any(|r| r.rule == "forbid:failed"));
        d.remove_rule("service:sshd.service");
        assert!(d.services.is_empty());
        d.remove_rule("forbid:failed");
        assert!(!d.forbid_failed);
    }
}
