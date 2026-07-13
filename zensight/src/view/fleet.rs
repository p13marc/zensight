//! Fleet capabilities — what each host actually speaks (#469, RFC 08 §6).
//!
//! Every sensor has served `introspect` since the keyspace-v2 cutover, replying
//! with the registry slice its build was compiled against. Nothing ever called
//! it. This view does: it fans the procedure out across the fleet and answers,
//! in one screen, the four questions that otherwise need an SSH session:
//!
//! 1. **What does this host speak?** — its producers, and how many subjects and
//!    procedures each serves.
//! 2. **Is it the same build as us?** — served `[registry] version` against the
//!    version this GUI compiled in.
//! 3. **Is it serving anything deprecated?** — cross-referenced against the
//!    host's own retirement ledger. Quiet until the first deprecation lands,
//!    which is the correct amount of noise for a question that today has no
//!    answer at all.
//! 4. **Does the registry match reality?** — the subject/procedure diff. RFC 08
//!    §6 is explicit that a disagreement here is a *finding*, not an ambiguity.
//!
//! Plus the honest reverse direction: a producer that is **alive** on the bus
//! (we have its sensor doc) but answers no `introspect` is listed as `silent` —
//! an old build, or a broken queryable. Reporting only what answered would let
//! exactly the hosts you most need to see disappear from the inventory.

use iced::widget::{button, column, text};
use iced::{Element, Length};

use zensight_keyspace::slice::{SliceFinding, parse_slice};

use crate::message::Message;
use crate::view::components::{
    Column as TableColumn, DataTable, SortKey, TableState, badge, empty_state, section_header,
};
use crate::view::specialized::fetch::Fetch;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// One `introspect` reply: which host, which producer, and the raw slice it
/// served. The origin is recovered from the *answering key*, not the payload —
/// a registry slice does not name the host it runs on, and it should not
/// (RFC 08 §2: the slice describes the build, the key describes the deployment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetReply {
    pub origin: String,
    pub producer: String,
    pub toml: String,
}

/// What a host is, relative to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetStatus {
    /// Serves exactly the slice we compiled in.
    InSync,
    /// Serves a different `[registry] version`.
    Skew,
    /// Same version, different content — the more alarming case of the two.
    Drift,
    /// Alive on the bus, but answered no `introspect`.
    Silent,
}

impl FleetStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::InSync => "in sync",
            Self::Skew => "version skew",
            Self::Drift => "drift",
            Self::Silent => "silent",
        }
    }

    /// Sort worst-first: the whole point of the view is to surface the odd one
    /// out, so an alphabetical sort on the status column would bury it.
    fn severity(self) -> u8 {
        match self {
            Self::Drift => 0,
            Self::Silent => 1,
            Self::Skew => 2,
            Self::InSync => 3,
        }
    }
}

/// One (host, producer) pair and what it told us.
#[derive(Debug, Clone)]
pub struct FleetRow {
    pub origin: String,
    /// The friendly source name if we know it, else the origin.
    pub host: String,
    pub producer: String,
    /// Served registry version; empty when the producer is silent.
    pub version: String,
    pub subjects: usize,
    pub procedures: usize,
    pub status: FleetStatus,
    pub findings: Vec<SliceFinding>,
}

impl FleetRow {
    fn search_key(&self) -> String {
        format!("{} {} {}", self.host, self.producer, self.status.label())
    }
}

/// A producer known to be alive on the bus: `(origin, producer, host name)`.
/// Fed from the app's sensor-registration map, so that a producer which is up
/// but does not answer `introspect` still gets a row.
pub type AliveProducer = (String, String, String);

#[derive(Debug, Default)]
pub struct FleetState {
    pub rows: Fetch<Vec<FleetRow>>,
    pub table: TableState,
    /// Which row's findings are expanded.
    pub expanded: Option<String>,
}

impl FleetState {
    pub fn loading(&mut self) {
        self.rows = Fetch::Loading;
    }

    /// Fold the fan-out into rows: parse each reply, diff it against the slice
    /// this build compiled in, and add a `silent` row for every alive producer
    /// that did not answer.
    pub fn apply(&mut self, result: Result<Vec<FleetReply>, String>, alive: &[AliveProducer]) {
        self.rows = Fetch::from_result(result.map(|replies| build_rows(replies, alive)));
    }
}

/// Pure fold — replies + who we know is alive → the table. Kept free of Iced so
/// it can be tested as what it is: a diff.
pub fn build_rows(replies: Vec<FleetReply>, alive: &[AliveProducer]) -> Vec<FleetRow> {
    let name_of = |origin: &str, producer: &str| -> String {
        alive
            .iter()
            .find(|(o, p, _)| o == origin && p == producer)
            .map(|(_, _, host)| host.clone())
            .unwrap_or_else(|| origin.to_string())
    };

    let mut rows: Vec<FleetRow> = Vec::new();
    for reply in &replies {
        let host = name_of(&reply.origin, &reply.producer);
        // A slice we cannot parse is itself a finding — not a reason to drop
        // the host from the inventory.
        let Ok(served) = parse_slice(&reply.toml) else {
            rows.push(FleetRow {
                origin: reply.origin.clone(),
                host,
                producer: reply.producer.clone(),
                version: "unreadable".into(),
                subjects: 0,
                procedures: 0,
                status: FleetStatus::Drift,
                findings: Vec::new(),
            });
            continue;
        };

        // The slice this build compiled in, for the same producer. A producer
        // we have never heard of has nothing to diff against — it is newer than
        // us, which is exactly the skew we want reported, not hidden.
        let local = zensight_keyspace::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == reply.producer)
            .and_then(|(_, t)| parse_slice(t).ok());

        let (status, findings) = match &local {
            Some(local) => {
                let findings = zensight_keyspace::slice::diff(&served, local);
                let status = if findings.is_empty() {
                    FleetStatus::InSync
                } else if findings
                    .iter()
                    .any(|f| matches!(f, SliceFinding::VersionSkew { .. }))
                {
                    FleetStatus::Skew
                } else {
                    FleetStatus::Drift
                };
                (status, findings)
            }
            None => (FleetStatus::Skew, Vec::new()),
        };

        rows.push(FleetRow {
            origin: reply.origin.clone(),
            host,
            producer: reply.producer.clone(),
            version: served.version.clone(),
            subjects: served.subjects.len(),
            procedures: served.procedures.len(),
            status,
            findings,
        });
    }

    // Alive but silent: up on the bus, no answer to introspect.
    for (origin, producer, host) in alive {
        let answered = replies
            .iter()
            .any(|r| &r.origin == origin && &r.producer == producer);
        if !answered {
            rows.push(FleetRow {
                origin: origin.clone(),
                host: host.clone(),
                producer: producer.clone(),
                version: String::new(),
                subjects: 0,
                procedures: 0,
                status: FleetStatus::Silent,
                findings: Vec::new(),
            });
        }
    }

    rows.sort_by(|a, b| {
        a.status
            .severity()
            .cmp(&b.status.severity())
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| a.producer.cmp(&b.producer))
    });
    rows
}

fn status_badge(status: FleetStatus) -> Element<'static, Message> {
    let color = match status {
        FleetStatus::InSync => theme::STATUS_ONLINE,
        FleetStatus::Skew => theme::STATUS_DEGRADED,
        FleetStatus::Drift => theme::STATUS_OFFLINE,
        FleetStatus::Silent => theme::STATUS_UNKNOWN,
    };
    badge(color, status.label())
}

pub fn fleet_view(state: &FleetState) -> Element<'_, Message> {
    let header = section_header("Fleet capabilities", None);
    let blurb = text(
        "What each host's build says it serves (@rpc introspect, RFC 08 §6), \
         diffed against the registry this GUI compiled in.",
    )
    .size(font::CAPTION);

    if state.rows.is_loading() {
        return column![header, blurb, empty_state("Asking the fleet…", None)]
            .spacing(space::SM)
            .padding(space::MD)
            .into();
    }
    if let Some(err) = state.rows.error() {
        return column![
            header,
            blurb,
            empty_state(
                format!("Introspect failed: {err}"),
                Some(refresh_button().into())
            )
        ]
        .spacing(space::SM)
        .padding(space::MD)
        .into();
    }
    let Some(rows) = state.rows.ready() else {
        return column![header, blurb, refresh_button()]
            .spacing(space::SM)
            .padding(space::MD)
            .into();
    };
    if rows.is_empty() {
        return column![
            header,
            blurb,
            empty_state(
                "No producer answered introspect. Is anything connected?",
                Some(refresh_button().into())
            )
        ]
        .spacing(space::SM)
        .padding(space::MD)
        .into();
    }

    let columns = vec![
        TableColumn::fill("host", 3, |r: &FleetRow| {
            text(r.host.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &FleetRow| SortKey::Text(r.host.clone())),
        TableColumn::fill("producer", 2, |r: &FleetRow| {
            text(r.producer.clone()).size(font::CAPTION).into()
        })
        .sortable(|r: &FleetRow| SortKey::Text(r.producer.clone())),
        TableColumn::fixed("registry", 90.0, |r: &FleetRow| {
            let v = if r.version.is_empty() {
                "—".to_string()
            } else {
                r.version.clone()
            };
            text(v).size(font::CAPTION).into()
        })
        .sortable(|r: &FleetRow| SortKey::Text(r.version.clone())),
        TableColumn::fixed("subjects", 80.0, |r: &FleetRow| {
            text(r.subjects.to_string()).size(font::CAPTION).into()
        })
        .sortable(|r: &FleetRow| SortKey::Num(r.subjects as f64)),
        TableColumn::fixed("procedures", 90.0, |r: &FleetRow| {
            text(r.procedures.to_string()).size(font::CAPTION).into()
        })
        .sortable(|r: &FleetRow| SortKey::Num(r.procedures as f64)),
        TableColumn::fixed("status", 120.0, |r: &FleetRow| status_badge(r.status))
            .sortable(|r: &FleetRow| SortKey::Num(r.status.severity() as f64)),
        TableColumn::fixed("findings", 110.0, |r: &FleetRow| {
            if r.findings.is_empty() {
                return text("—").size(font::CAPTION).into();
            }
            button(text(format!("{} finding(s)", r.findings.len())).size(font::CAPTION))
                .padding([2, 8])
                .on_press(Message::ToggleFleetFindings(row_id(r)))
                .style(iced::widget::button::text)
                .into()
        }),
    ];

    let mut body = column![
        header,
        blurb,
        refresh_button(),
        DataTable::new(columns)
            .searchable(FleetRow::search_key)
            .on_sort(Message::FleetTableSort)
            .on_filter(Message::FleetTableFilter)
            .noun("producers")
            .view(rows, &state.table),
    ]
    .spacing(space::SM)
    .padding(space::MD);

    if let Some(id) = &state.expanded
        && let Some(r) = rows.iter().find(|r| &row_id(r) == id)
    {
        body = body.push(findings_panel(r));
    }
    body.into()
}

/// The findings for one row, spelled out. A count in a cell tells you something
/// is wrong; this tells you what.
fn findings_panel(r: &FleetRow) -> Element<'_, Message> {
    let mut col =
        column![text(format!("{} · {} — findings", r.host, r.producer)).size(font::EMPHASIS),]
            .spacing(space::XS);
    for f in &r.findings {
        col = col.push(text(f.summary()).size(font::CAPTION));
    }
    col.width(Length::Fill).into()
}

fn row_id(r: &FleetRow) -> String {
    format!("{}/{}", r.origin, r.producer)
}

fn refresh_button() -> iced::widget::Button<'static, Message> {
    button(text("Ask the fleet").size(font::CAPTION))
        .padding([4, 12])
        .on_press(Message::RefreshFleet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice_toml(producer: &str, version: &str, extra_subject: Option<&str>) -> String {
        let mut s = format!(
            "[registry]\nversion = \"{version}\"\napp = \"zensight\"\nconvention = 1\n\
             [producer]\nname = \"{producer}\"\n\
             [[procedure]]\npath = \"introspect\"\nkind = \"read\"\n"
        );
        if let Some(p) = extra_subject {
            s.push_str(&format!(
                "[[subject]]\npath = \"{p}\"\nclass = \"telemetry\"\ntype = \"TelemetryPoint\"\n"
            ));
        }
        s
    }

    /// A host serving exactly what we compiled in is `in sync` — the answer the
    /// view should be able to give at a glance.
    #[test]
    fn a_matching_build_is_in_sync() {
        let (_, local) = zensight_keyspace::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == "sysinfo")
            .unwrap();
        let rows = build_rows(
            vec![FleetReply {
                origin: "h-aaaaaaaaaaaa".into(),
                producer: "sysinfo".into(),
                toml: (*local).to_string(),
            }],
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, FleetStatus::InSync);
        assert!(rows[0].findings.is_empty());
        assert!(rows[0].subjects > 0, "sysinfo serves real subjects (#468)");
    }

    /// A different registry version is skew; the row still reports what it saw.
    #[test]
    fn a_different_version_is_skew() {
        let rows = build_rows(
            vec![FleetReply {
                origin: "h-bbbbbbbbbbbb".into(),
                producer: "sysinfo".into(),
                toml: slice_toml("sysinfo", "9.9", Some("cpu/usage")),
            }],
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Skew);
        assert_eq!(rows[0].version, "9.9");
        assert!(
            rows[0]
                .findings
                .iter()
                .any(|f| matches!(f, SliceFinding::VersionSkew { .. }))
        );
    }

    /// Alive on the bus but no answer: the row that would otherwise vanish, and
    /// the one you most need to see.
    #[test]
    fn an_alive_producer_that_does_not_answer_is_silent() {
        let alive = vec![(
            "h-cccccccccccc".to_string(),
            "netring".to_string(),
            "edge01".to_string(),
        )];
        let rows = build_rows(Vec::new(), &alive);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, FleetStatus::Silent);
        assert_eq!(rows[0].host, "edge01");
    }

    /// Worst first: a drifting host must not sort below ten healthy ones.
    #[test]
    fn rows_sort_worst_first() {
        let (_, sysinfo) = zensight_keyspace::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == "sysinfo")
            .unwrap();
        let rows = build_rows(
            vec![
                FleetReply {
                    origin: "h-aaaaaaaaaaaa".into(),
                    producer: "sysinfo".into(),
                    toml: (*sysinfo).to_string(),
                },
                FleetReply {
                    origin: "h-bbbbbbbbbbbb".into(),
                    producer: "sysinfo".into(),
                    toml: "not toml at all {{{".into(),
                },
            ],
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Drift);
        assert_eq!(rows[0].version, "unreadable");
        assert_eq!(rows[1].status, FleetStatus::InSync);
    }

    #[test]
    fn renders_a_populated_table() {
        let (_, sysinfo) = zensight_keyspace::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == "sysinfo")
            .unwrap();
        let mut state = FleetState::default();
        state.apply(
            Ok(vec![FleetReply {
                origin: "h-aaaaaaaaaaaa".into(),
                producer: "sysinfo".into(),
                toml: (*sysinfo).to_string(),
            }]),
            &[("h-aaaaaaaaaaaa".into(), "sysinfo".into(), "server01".into())],
        );
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(ui.find("server01").is_ok());
        assert!(ui.find("in sync").is_ok());
    }
}
