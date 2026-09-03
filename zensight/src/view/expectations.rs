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
    Hostspec,
}

impl ExpTarget {
    pub const ALL: &'static [ExpTarget] =
        &[ExpTarget::Netlink, ExpTarget::Systemd, ExpTarget::Hostspec];
}

impl std::fmt::Display for ExpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ExpTarget::Netlink => "netlink",
                ExpTarget::Systemd => "systemd",
                ExpTarget::Hostspec => "hostspec",
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
    /// The timer fired within the window **and its service's last run
    /// succeeded** (#824) — the form that catches a timer firing on schedule
    /// while its service fails every run.
    TimerSucceeded,
    RestartRate,
    ForbidFailed,
}

impl SystemdExpKind {
    pub const ALL: &'static [SystemdExpKind] = &[
        SystemdExpKind::ServiceActive,
        SystemdExpKind::TargetActive,
        SystemdExpKind::TimerWithin,
        SystemdExpKind::TimerSucceeded,
        SystemdExpKind::RestartRate,
        SystemdExpKind::ForbidFailed,
    ];
    fn label(&self) -> &'static str {
        match self {
            SystemdExpKind::ServiceActive => "Service must be active",
            SystemdExpKind::TargetActive => "Target must be active",
            SystemdExpKind::TimerWithin => "Timer must fire within",
            SystemdExpKind::TimerSucceeded => "Timer service must succeed within",
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

/// The kind of hostspec assertion being authored (#821). Eight GUI kinds
/// over the sensor's seven: require- and forbid-listeners author differently
/// enough to deserve their own entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostspecExpKind {
    Mount,
    File,
    Listening,
    ListeningForbid,
    Symlink,
    Absent,
    Content,
    Perms,
}

impl HostspecExpKind {
    pub const ALL: &'static [HostspecExpKind] = &[
        HostspecExpKind::Mount,
        HostspecExpKind::File,
        HostspecExpKind::Listening,
        HostspecExpKind::ListeningForbid,
        HostspecExpKind::Symlink,
        HostspecExpKind::Absent,
        HostspecExpKind::Content,
        HostspecExpKind::Perms,
    ];
    fn label(&self) -> &'static str {
        match self {
            HostspecExpKind::Mount => "Mount (point / bind-of)",
            HostspecExpKind::File => "File freshness / size",
            HostspecExpKind::Listening => "Listener must exist",
            HostspecExpKind::ListeningForbid => "Listener must NOT exist",
            HostspecExpKind::Symlink => "Symlink target",
            HostspecExpKind::Absent => "Path must be absent",
            HostspecExpKind::Content => "File must contain",
            HostspecExpKind::Perms => "Permissions / owner",
        }
    }
}

impl std::fmt::Display for HostspecExpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// The accumulated hostspec assertion set (#821). Mirrors the sensor's
/// `ExpectationsConfig` JSON (the `expectations/set` body is the PLAIN
/// config — no command tag; validation happens sensor-side and a refusal
/// keeps the previous set). The form authors each kind's essential fields;
/// the long tail (regex `matches`, mount options, per-expectation
/// severity/debounce) is config-file territory, said so in the caption.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostspecExpDraft {
    pub eval_interval_secs: u64,
    /// `(name, path, is_bind_of?, fstype?)`
    pub mounts: Vec<(String, String, Option<String>, Option<String>)>,
    /// `(name, path, newer_than_secs?, size_within_pct?)`
    pub files: Vec<(String, String, Option<u64>, Option<f64>)>,
    /// `(name, port, addr?, forbid)`
    pub listening: Vec<(String, u16, Option<String>, bool)>,
    /// `(name, path, target)`
    pub symlinks: Vec<(String, String, String)>,
    /// `(name, path)`
    pub absent: Vec<(String, String)>,
    /// `(name, path, contains-needle)`
    pub content: Vec<(String, String, String)>,
    /// `(name, path, mode?, owner?)`
    pub perms: Vec<(String, String, Option<String>, Option<String>)>,
}

impl HostspecExpDraft {
    /// Build the `expectations/set` body (pure — unit-testable): the
    /// sensor's plain `ExpectationsConfig` shape.
    pub fn to_set_json(&self) -> serde_json::Value {
        let interval = if self.eval_interval_secs == 0 {
            60
        } else {
            self.eval_interval_secs
        };
        serde_json::json!({
            "eval_interval_secs": interval,
            "mounts": self.mounts.iter().map(|(n, p, b, f)| {
                let mut o = serde_json::json!({"name": n, "path": p});
                if let Some(b) = b { o["is_bind_of"] = b.clone().into(); }
                if let Some(f) = f { o["fstype"] = f.clone().into(); }
                o
            }).collect::<Vec<_>>(),
            "files": self.files.iter().map(|(n, p, secs, pct)| {
                let mut o = serde_json::json!({"name": n, "path": p});
                if let Some(s) = secs { o["newer_than_secs"] = (*s).into(); }
                if let Some(pc) = pct { o["size_within_pct_of_previous"] = (*pc).into(); }
                o
            }).collect::<Vec<_>>(),
            "listening": self.listening.iter().map(|(n, port, addr, forbid)| {
                let mut o = serde_json::json!({"name": n, "port": port, "forbid": forbid});
                if let Some(a) = addr { o["addr"] = a.clone().into(); }
                o
            }).collect::<Vec<_>>(),
            "symlinks": self.symlinks.iter().map(|(n, p, t)|
                serde_json::json!({"name": n, "path": p, "target": t})).collect::<Vec<_>>(),
            "absent": self.absent.iter().map(|(n, p)|
                serde_json::json!({"name": n, "path": p})).collect::<Vec<_>>(),
            "content": self.content.iter().map(|(n, p, c)|
                serde_json::json!({"name": n, "path": p, "contains": [c]})).collect::<Vec<_>>(),
            "perms": self.perms.iter().map(|(n, p, m, o_)| {
                let mut o = serde_json::json!({"name": n, "path": p});
                if let Some(m) = m { o["mode"] = m.clone().into(); }
                if let Some(ow) = o_ { o["owner"] = ow.clone().into(); }
                o
            }).collect::<Vec<_>>(),
        })
    }

    /// Parse an `@rpc/hostspec/expectations` reply into a draft (pure).
    /// Fields the form does not author (options, matches, severity, …)
    /// survive on the SENSOR unchanged only until the next push replaces the
    /// whole set — the caption warns about that.
    pub fn from_status(json: &str) -> Self {
        let v: serde_json::Value = match serde_json::from_str(json) {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let arr = |key: &str| {
            v.get(key)
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
        };
        let name = |s: &serde_json::Value| s.get("name").and_then(|x| x.as_str()).map(String::from);
        let path = |s: &serde_json::Value| s.get("path").and_then(|x| x.as_str()).map(String::from);
        let opt_s =
            |s: &serde_json::Value, k: &str| s.get(k).and_then(|x| x.as_str()).map(String::from);
        Self {
            eval_interval_secs: v
                .get("eval_interval_secs")
                .and_then(|x| x.as_u64())
                .unwrap_or(60),
            mounts: arr("mounts")
                .iter()
                .filter_map(|s| {
                    Some((
                        name(s)?,
                        path(s)?,
                        opt_s(s, "is_bind_of"),
                        opt_s(s, "fstype"),
                    ))
                })
                .collect(),
            files: arr("files")
                .iter()
                .filter_map(|s| {
                    Some((
                        name(s)?,
                        path(s)?,
                        s.get("newer_than_secs").and_then(|x| x.as_u64()),
                        s.get("size_within_pct_of_previous")
                            .and_then(|x| x.as_f64()),
                    ))
                })
                .collect(),
            listening: arr("listening")
                .iter()
                .filter_map(|s| {
                    Some((
                        name(s)?,
                        s.get("port").and_then(|x| x.as_u64())? as u16,
                        opt_s(s, "addr"),
                        s.get("forbid").and_then(|x| x.as_bool()).unwrap_or(false),
                    ))
                })
                .collect(),
            symlinks: arr("symlinks")
                .iter()
                .filter_map(|s| Some((name(s)?, path(s)?, opt_s(s, "target")?)))
                .collect(),
            absent: arr("absent")
                .iter()
                .filter_map(|s| Some((name(s)?, path(s)?)))
                .collect(),
            content: arr("content")
                .iter()
                .filter_map(|s| {
                    let needle = s
                        .get("contains")
                        .and_then(|x| x.as_array())
                        .and_then(|a| a.first())
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some((name(s)?, path(s)?, needle))
                })
                .collect(),
            perms: arr("perms")
                .iter()
                .filter_map(|s| Some((name(s)?, path(s)?, opt_s(s, "mode"), opt_s(s, "owner"))))
                .collect(),
        }
    }

    /// Remove one assertion by its rule slug (`<kind>:<name>`).
    pub fn remove_rule(&mut self, rule: &str) {
        let Some((kind, name)) = rule.split_once(':') else {
            return;
        };
        match kind {
            "mount" => self.mounts.retain(|(n, ..)| n != name),
            "file" => self.files.retain(|(n, ..)| n != name),
            "listening" => self.listening.retain(|(n, ..)| n != name),
            "symlink" => self.symlinks.retain(|(n, ..)| n != name),
            "absent" => self.absent.retain(|(n, ..)| n != name),
            "content" => self.content.retain(|(n, ..)| n != name),
            "perms" => self.perms.retain(|(n, ..)| n != name),
            _ => {}
        }
    }

    /// The configured-list rows for this draft — rule slugs match the
    /// sensor's (`<kind>:<name>`), so a row here IS the alert rule.
    pub fn rows(&self) -> Vec<ExpRow> {
        let mut rows = Vec::new();
        for (n, p, bind, fstype) in &self.mounts {
            let detail = match (bind, fstype) {
                (Some(b), _) => format!("{p} is a bind of {b}"),
                (None, Some(f)) => format!("{p} mounted as {f}"),
                (None, None) => format!("{p} is a mount point"),
            };
            rows.push(ExpRow {
                rule: format!("mount:{n}"),
                detail,
                severity: "warning".into(),
            });
        }
        for (n, p, secs, pct) in &self.files {
            let mut parts = vec![format!("{p} exists")];
            if let Some(s) = secs {
                parts.push(format!("newer than {s}s"));
            }
            if let Some(pc) = pct {
                parts.push(format!("size ±{pc}%"));
            }
            rows.push(ExpRow {
                rule: format!("file:{n}"),
                detail: parts.join(", "),
                severity: "warning".into(),
            });
        }
        for (n, port, addr, forbid) in &self.listening {
            let a = addr.as_deref().unwrap_or("*");
            let detail = if *forbid {
                format!("NO listener on {a}:{port}")
            } else {
                format!("listener on {a}:{port}")
            };
            rows.push(ExpRow {
                rule: format!("listening:{n}"),
                detail,
                severity: "warning".into(),
            });
        }
        for (n, p, t) in &self.symlinks {
            rows.push(ExpRow {
                rule: format!("symlink:{n}"),
                detail: format!("{p} -> {t}"),
                severity: "warning".into(),
            });
        }
        for (n, p) in &self.absent {
            rows.push(ExpRow {
                rule: format!("absent:{n}"),
                detail: format!("{p} absent"),
                severity: "warning".into(),
            });
        }
        for (n, p, c) in &self.content {
            rows.push(ExpRow {
                rule: format!("content:{n}"),
                detail: format!("{p} contains {c:?}"),
                severity: "warning".into(),
            });
        }
        for (n, p, m, o) in &self.perms {
            let mut parts = Vec::new();
            if let Some(m) = m {
                parts.push(format!("mode {m}"));
            }
            if let Some(o) = o {
                parts.push(format!("owner {o}"));
            }
            rows.push(ExpRow {
                rule: format!("perms:{n}"),
                detail: format!("{p}: {}", parts.join(", ")),
                severity: "warning".into(),
            });
        }
        rows
    }
}

/// The accumulated systemd expectation set (#278). Mirrors the sensor's
/// `ExpectationsConfig`; the GUI edits it and pushes the whole thing via
/// `SetExpectations`.
#[derive(Debug, Clone)]
pub struct SystemdExpDraft {
    pub eval_interval_secs: u64,
    pub for_secs: u64,
    /// The set's recovery hold (#932). Round-tripped even though nothing in
    /// this view edits it yet: `to_command_json` sends a WHOLE replacement
    /// set, so a field the draft does not carry is a field the next GUI push
    /// silently resets to its default.
    pub recover_after_secs: u64,
    pub services: Vec<String>,
    pub targets: Vec<String>,
    /// `(timer, within_secs, succeeded_within_secs)` — either window may be
    /// set, mirroring the sensor's two-strength `TimerExpectation` (#824).
    pub timers: Vec<(String, Option<u64>, Option<u64>)>,
    pub restart_rates: Vec<(String, u32, u64)>,
    pub forbid_failed: bool,
}

impl Default for SystemdExpDraft {
    fn default() -> Self {
        Self {
            eval_interval_secs: 10,
            for_secs: 15,
            recover_after_secs: 0,
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
            "recover_after_secs": self.recover_after_secs,
            "services_active": self.services.iter().map(|u| serde_json::json!({"unit": u})).collect::<Vec<_>>(),
            "targets_active": self.targets.iter().map(|t| serde_json::json!({"target": t})).collect::<Vec<_>>(),
            "timers": self.timers.iter().map(|(t, w, sw)| {
                let mut o = serde_json::json!({"timer": t});
                if let Some(w) = w { o["within_secs"] = (*w).into(); }
                if let Some(sw) = sw { o["succeeded_within_secs"] = (*sw).into(); }
                o
            }).collect::<Vec<_>>(),
            "restart_rates": self.restart_rates.iter().map(|(u, m, w)| serde_json::json!({"unit": u, "max": m, "window_secs": w})).collect::<Vec<_>>(),
            "forbid_failed": self.forbid_failed,
        })
    }

    /// Parse a systemd `@rpc/systemd/expectations` reply into a draft (pure).
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
            recover_after_secs: u64_at("recover_after_secs", 0),
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
                        s.get("within_secs").and_then(|x| x.as_u64()),
                        s.get("succeeded_within_secs").and_then(|x| x.as_u64()),
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
        for (t, w, sw) in &self.timers {
            let detail = match (w, sw) {
                (Some(w), Some(sw)) => format!("fired within {w}s, succeeded within {sw}s"),
                (Some(w), None) => format!("fired within {w}s"),
                (None, Some(sw)) => format!("succeeded within {sw}s"),
                (None, None) => "checks nothing".into(),
            };
            rows.push(ExpRow {
                rule: format!("timer:{t}"),
                detail,
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
            self.timers.retain(|(name, _, _)| name != t);
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
    /// The kind of hostspec assertion being authored (#821).
    pub hostspec_kind: HostspecExpKind,
    /// The accumulated hostspec assertion set (#821).
    pub hostspec: HostspecExpDraft,

    /// The verbatim `@rpc/hostspec/spec` answer (#867), so an empty assertion
    /// set can be shown as the state it is rather than as an absence.
    pub hostspec_spec: Option<String>,

    pub status_note: Option<String>,
    /// Schema verdict for the last netlink `expectations` status reply (#791).
    pub status_verdict: Option<zensight_common::schema::Verdict>,
    /// Schema verdict for the last systemd `expectations` status reply (#791).
    pub systemd_verdict: Option<zensight_common::schema::Verdict>,
    /// Schema verdict for the last hostspec `expectations` status reply (#791).
    pub hostspec_verdict: Option<zensight_common::schema::Verdict>,
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
            hostspec_kind: HostspecExpKind::Mount,
            hostspec: HostspecExpDraft::default(),
            hostspec_spec: None,
            status_note: None,
            status_verdict: None,
            systemd_verdict: None,
            hostspec_verdict: None,
        }
    }
}

/// Render the expectations authoring view.
pub fn expectations_view(state: &ExpectationsState) -> Element<'_, Message> {
    let form: Element<'_, Message> = match state.target {
        ExpTarget::Netlink => render_form(state),
        ExpTarget::Systemd => render_systemd_form(state),
        ExpTarget::Hostspec => render_hostspec_form(state),
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
            SystemdExpKind::TimerSucceeded => "timer (cosign.timer)",
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
        SystemdExpKind::TimerWithin | SystemdExpKind::TimerSucceeded => {
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

/// The hostspec assertion authoring form (#821). Whole-set semantics like
/// systemd's: each add mutates the draft and re-pushes the full set (which
/// the sensor VALIDATES before applying — a refusal keeps its previous set
/// and surfaces as a command-feedback toast).
fn render_hostspec_form(state: &ExpectationsState) -> Element<'_, Message> {
    let kind = pick_list(
        HostspecExpKind::ALL,
        Some(state.hostspec_kind),
        Message::SetHostspecExpKind,
    )
    .width(Length::Fixed(220.0));

    let mut form = row![kind].spacing(10).align_y(Alignment::Center);
    form = form.push(
        text_input("name (rule slug)", &state.new_name)
            .on_input(Message::SetExpectationName)
            .padding(8)
            .width(Length::Fixed(160.0)),
    );

    // Field semantics per kind ride the placeholders; the sensor's
    // validate() is the real gate.
    match state.hostspec_kind {
        HostspecExpKind::Listening | HostspecExpKind::ListeningForbid => {
            form = form.push(
                text_input("port", &state.new_port)
                    .on_input(Message::SetExpectationPort)
                    .padding(8)
                    .width(Length::Fixed(90.0)),
            );
            form = form.push(
                text_input("addr (optional; 0.0.0.0 and :: differ)", &state.new_value)
                    .on_input(Message::SetExpectationValue)
                    .padding(8)
                    .width(Length::Fixed(240.0)),
            );
        }
        kind => {
            form = form.push(
                text_input("path (absolute)", &state.new_metric)
                    .on_input(Message::SetExpectationMetric)
                    .padding(8)
                    .width(Length::Fixed(220.0)),
            );
            let (ph1, ph2) = match kind {
                HostspecExpKind::Mount => ("is_bind_of (optional)", "fstype (optional)"),
                HostspecExpKind::File => ("newer_than secs (optional)", "size ±% (optional)"),
                HostspecExpKind::Symlink => ("target", ""),
                HostspecExpKind::Content => ("must contain", ""),
                HostspecExpKind::Perms => ("mode (0600, optional)", "owner (optional)"),
                _ => ("", ""),
            };
            if !ph1.is_empty() {
                form = form.push(
                    text_input(ph1, &state.new_value)
                        .on_input(Message::SetExpectationValue)
                        .padding(8)
                        .width(Length::Fixed(190.0)),
                );
            }
            if !ph2.is_empty() {
                form = form.push(
                    text_input(ph2, &state.new_port)
                        .on_input(Message::SetExpectationPort)
                        .padding(8)
                        .width(Length::Fixed(150.0)),
                );
            }
        }
    }

    let add = button(text("Add & Push").size(13))
        .on_press(Message::AddExpectation)
        .style(iced::widget::button::primary);

    column![
        text("Declare a host assertion").size(18),
        form.push(add),
        text(
            "The whole set replaces the sensor's over expectations/set (validated there; \
             a refusal keeps its previous set). Regex matches, mount options and \
             per-assertion severity/debounce are config-file territory — pushing from \
             here rewrites the set with this form's fields only.",
        )
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
        ExpTarget::Hostspec => state.hostspec.rows(),
    };
    let title = text(format!("Configured ({})", rows.len())).size(18);
    // The reply's schema verdict rides beside the count (#791): three
    // states, and "not checked" reads as absent, never as a pass.
    let verdict = match state.target {
        ExpTarget::Netlink => state.status_verdict.as_ref(),
        ExpTarget::Systemd => state.systemd_verdict.as_ref(),
        ExpTarget::Hostspec => state.hostspec_verdict.as_ref(),
    };
    let title: Element<'_, Message> = match verdict {
        Some(v) => row![title, crate::view::components::verdict::verdict_badge(v)]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        None => title.into(),
    };

    if rows.is_empty() {
        // An empty set is not the same fact as an unfetched one, and for
        // hostspec it is a *designed* state — assertions are per-host operator
        // policy and the shipped default holds a host to nothing (#821). Until
        // #867 both read as the same blank pane, which is how the sensor got
        // reported as broken by the person who had built it two days earlier.
        if state.target == ExpTarget::Hostspec && state.hostspec_verdict.is_some() {
            let mut col = column![
                title,
                text("This host is held to nothing.").size(14),
                text(
                    "That is a valid state, not a failure: the sweep runs, the \
                     failing-assertion gauge reads 0, and the sensor is healthy. \
                     Author an assertion in the form above and press Push to hold \
                     this host to something."
                )
                .size(12)
                .style(dim),
            ]
            .spacing(8);
            // The sensor's own answer, verbatim — it is the authority on what
            // it is being held to, and a rendering of it would be a second
            // opinion nobody asked for.
            if let Some(spec) = state.hostspec_spec.as_deref() {
                col = col.push(text("@rpc/hostspec/spec answers:").size(11).style(dim));
                col = col.push(text(spec.to_string()).size(11).font(iced::Font::MONOSPACE));
            }
            return col.into();
        }
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
        d.timers
            .push(("logrotate.timer".into(), Some(90_000), None));
        d.timers.push(("cosign.timer".into(), None, Some(3_900)));
        d.restart_rates.push(("nginx.service".into(), 5, 600));
        d.forbid_failed = true;
        let cmd = d.to_command_json();
        assert_eq!(cmd["type"], "set_expectations");
        assert_eq!(cmd["services_active"][0]["unit"], "sshd.service");
        assert_eq!(cmd["timers"][0]["within_secs"], 90_000);
        // An unset window is absent, not zero — the sensor treats the two
        // forms independently (#824).
        assert!(cmd["timers"][0].get("succeeded_within_secs").is_none());
        assert_eq!(cmd["timers"][1]["succeeded_within_secs"], 3_900);
        assert!(cmd["timers"][1].get("within_secs").is_none());
        assert_eq!(cmd["restart_rates"][0]["max"], 5);
        assert_eq!(cmd["forbid_failed"], true);
    }

    #[test]
    fn systemd_draft_status_roundtrip() {
        let mut d = SystemdExpDraft::default();
        d.services.push("a.service".into());
        d.targets.push("multi-user.target".into());
        d.timers.push(("b.timer".into(), Some(120), Some(600)));
        d.forbid_failed = true;
        d.recover_after_secs = 90;
        // The command payload drops the "type" tag but is otherwise the config.
        let json = serde_json::to_string(&d.to_command_json()).unwrap();
        let back = SystemdExpDraft::from_status(&json);
        assert_eq!(back.services, vec!["a.service".to_string()]);
        assert_eq!(back.targets, vec!["multi-user.target".to_string()]);
        // Both windows survive the round-trip — the GUI must not silently
        // drop the succeeded form on refresh (#824).
        assert_eq!(
            back.timers,
            vec![("b.timer".to_string(), Some(120), Some(600))]
        );
        assert!(back.forbid_failed);
        // The recovery hold survives too (#932). `to_command_json` sends a
        // WHOLE replacement set, so a field the draft did not carry would be
        // silently reset to its default on the next GUI push — which is how a
        // hold an operator set over `@rpc` or `@desired` would vanish the
        // first time anyone opened this view and pressed submit.
        assert_eq!(back.recover_after_secs, 90);
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

#[cfg(test)]
mod hostspec_tests {
    use super::*;

    fn draft() -> HostspecExpDraft {
        HostspecExpDraft {
            eval_interval_secs: 60,
            mounts: vec![(
                "vt".into(),
                "/var/tmp".into(),
                Some("/scratch/tmp".into()),
                None,
            )],
            files: vec![(
                "backup".into(),
                "/backup/db.dump".into(),
                Some(93600),
                Some(40.0),
            )],
            listening: vec![
                ("vpn".into(), 8443, Some("10.8.0.1".into()), false),
                ("not-any4".into(), 8443, Some("0.0.0.0".into()), true),
            ],
            symlinks: vec![(
                "java".into(),
                "/etc/alternatives/java".into(),
                "/usr/lib/jvm/x".into(),
            )],
            absent: vec![("left".into(), "/tmp/debug.sock".into())],
            content: vec![(
                "hairpin".into(),
                "/etc/hosts".into(),
                "10.0.0.5 registry".into(),
            )],
            perms: vec![(
                "key".into(),
                "/etc/deploy/key.pem".into(),
                Some("0600".into()),
                Some("deploy".into()),
            )],
        }
    }

    /// The push body is the sensor's PLAIN `ExpectationsConfig` JSON (no
    /// command tag — hostspec's `expectations/set` deserializes the config
    /// directly), and `from_status` reads the same shape back: the round
    /// trip pins that the GUI and the sensor speak one dialect. (The sensor's
    /// own e2e drives `expectations/set` with JSON of exactly this shape, so
    /// the cross-crate contract is exercised from both sides without a
    /// dependency between them.)
    #[test]
    fn set_json_round_trips_through_from_status() {
        let d = draft();
        let json = d.to_set_json();
        assert!(json.get("type").is_none(), "plain config — no command tag");
        assert_eq!(json["listening"][1]["forbid"], serde_json::json!(true));
        assert_eq!(json["content"][0]["contains"][0], "10.0.0.5 registry");
        let back = HostspecExpDraft::from_status(&json.to_string());
        assert_eq!(d, back);
    }

    /// Row slugs are the sensor's alert rules (`<kind>:<name>`), so the
    /// configured list's Remove button removes the thing that is firing.
    #[test]
    fn rows_and_remove_share_the_rule_slug() {
        let mut d = draft();
        let slugs: Vec<String> = d.rows().iter().map(|r| r.rule.clone()).collect();
        assert!(slugs.contains(&"mount:vt".to_string()));
        assert!(slugs.contains(&"listening:not-any4".to_string()));
        d.remove_rule("mount:vt");
        assert!(!d.rows().iter().any(|r| r.rule == "mount:vt"));
        d.remove_rule("nonsense-no-colon");
        assert_eq!(d.rows().len(), 7);
    }
}
