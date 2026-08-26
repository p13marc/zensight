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
//!
//! # Where the work happens (#745)
//!
//! The comparison is [`zenkey_fleet`]'s, not ours. A served slice and a
//! compiled-in one are both [`SliceSet`]s, and
//! [`SliceSet::diff`](zenkey_fleet::SliceSet::diff) is the set-level join —
//! including the two one-sided cases (served but unknown to us; declared here
//! but served by nobody) that this view used to spell by hand and get half
//! right. What is left here is the part that is genuinely ours: the
//! **per-origin** shape of the question. Upstream's `SliceSet::from_bus` and
//! `fleet_registry` collapse the fleet to one slice per producer — right for a
//! decoder that only needs *a* slice, wrong for an inventory whose whole
//! subject is which **host** disagrees. So the sweep keeps each reply's origin
//! (see [`FleetReply`]) and the fold diffs one `SliceSet` per host.
//!
//! The sweep itself is bounded, and the bound reports what it cost — see
//! [`FleetSweep::elided`]. A fleet larger than the bound must not read as a
//! fleet that answered.

use std::collections::BTreeMap;

use iced::widget::{button, column, text};
use iced::{Element, Length};

use zenkey_fleet::SliceSet;

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

/// One `introspect` sweep, whole: what came back **and what the bound refused**.
///
/// The second half is not decoration. The fan-in is bounded
/// ([`zenkey_fleet::DEFAULT_MAX_REPLIES`]), and a bound that hides data has to
/// say so (RFC 13 §3 O6) — otherwise a fleet too large for the bound renders
/// exactly like a fleet that answered in full, and does so *more* readily the
/// bigger the fleet gets, which is backwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetSweep {
    pub replies: Vec<FleetReply>,
    /// Replies that arrived and were not kept, across the sweep's queriers.
    pub elided: u64,
    /// The per-querier reply bound that refused them.
    pub bound: usize,
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
    /// RFC 08 §6 findings, already rendered by the engine
    /// ([`zenkey_fleet::report::ProducerDiff`]).
    pub findings: Vec<String>,
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
    /// What the last sweep's reply bound refused, and the bound itself.
    pub elided: u64,
    pub bound: usize,
}

impl FleetState {
    pub fn loading(&mut self) {
        self.rows = Fetch::Loading;
    }

    /// Fold the fan-out into rows: diff each host's served slices against the
    /// slices this build compiled in, and add a `silent` row for every alive
    /// producer that did not answer.
    pub fn apply(&mut self, result: Result<FleetSweep, String>, alive: &[AliveProducer]) {
        if let Ok(sweep) = &result {
            self.elided = sweep.elided;
            self.bound = sweep.bound;
        }
        self.rows = Fetch::from_result(result.map(|sweep| build_rows(&sweep, alive)));
    }
}

/// The slices *this build* compiled in, as a [`SliceSet`] — the local side of
/// every diff below.
///
/// `REGISTRIES` is the same TOML text `zenkey-build` compiled the typed
/// builders from, so a slice that fails to parse here is a build-time
/// impossibility rather than a runtime condition; it is skipped rather than
/// panicking a view.
fn local_registry() -> SliceSet {
    SliceSet::from_slices(
        zensight_common::registry::REGISTRIES
            .iter()
            .filter_map(|(_, toml)| zenkey::slice::parse_slice(toml).ok())
            .collect(),
    )
}

/// Pure fold — a sweep + who we know is alive → the table. Kept free of Iced so
/// it can be tested as what it is: a diff.
pub fn build_rows(sweep: &FleetSweep, alive: &[AliveProducer]) -> Vec<FleetRow> {
    let local = local_registry();

    let name_of = |origin: &str, producer: &str| -> String {
        alive
            .iter()
            .find(|(o, p, _)| o == origin && p == producer)
            .map(|(_, _, host)| host.clone())
            .unwrap_or_else(|| origin.to_string())
    };

    // One `SliceSet` per **origin**: the fleet's disagreements are per host, and
    // a set-level diff of the whole fleet at once would average them away.
    let mut served: BTreeMap<&str, Vec<zenkey::RegistrySlice>> = BTreeMap::new();
    let mut unreadable: Vec<(&str, &str, String)> = Vec::new();
    for reply in &sweep.replies {
        // A slice we cannot parse is itself a finding — not a reason to drop
        // the host from the inventory.
        match zenkey::slice::parse_slice(&reply.toml) {
            Ok(slice) => served.entry(&reply.origin).or_default().push(slice),
            Err(e) => unreadable.push((&reply.origin, &reply.producer, e.to_string())),
        }
    }

    let mut rows: Vec<FleetRow> = Vec::new();
    for (origin, slices) in served {
        let set = SliceSet::from_slices(slices);
        // Diff against *only* the producers this host serves. The full local
        // set would emit "declared locally, served by nobody" for every producer
        // this host does not happen to run — true of the fleet, a lie about the
        // host.
        let mine = SliceSet::from_slices(
            set.slices()
                .iter()
                .filter_map(|s| local.get(&s.name).cloned())
                .collect(),
        );

        for diff in set.diff(&mine).producers {
            let slice = set.get(&diff.producer);
            let status = if diff.findings.is_empty() {
                FleetStatus::InSync
            } else if diff.local_version.as_deref() != diff.served_version.as_deref() {
                // A version that differs — including one we have never heard
                // of, which is the same skew seen from the other side.
                FleetStatus::Skew
            } else {
                FleetStatus::Drift
            };
            rows.push(FleetRow {
                origin: origin.to_string(),
                host: name_of(origin, &diff.producer),
                version: diff.served_version.clone().unwrap_or_default(),
                subjects: slice.map_or(0, |s| s.subjects.len()),
                procedures: slice.map_or(0, |s| s.procedures.len()),
                status,
                findings: diff.findings,
                producer: diff.producer,
            });
        }
    }

    for (origin, producer, why) in unreadable {
        rows.push(FleetRow {
            origin: origin.to_string(),
            host: name_of(origin, producer),
            producer: producer.to_string(),
            version: "unreadable".into(),
            subjects: 0,
            procedures: 0,
            status: FleetStatus::Drift,
            findings: vec![format!("the served slice did not parse: {why}")],
        });
    }

    // Alive but silent: up on the bus, no answer to introspect.
    for (origin, producer, host) in alive {
        let answered = sweep
            .replies
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

    let mut body = column![header, blurb, refresh_button()]
        .spacing(space::SM)
        .padding(space::MD);
    if let Some(note) = elision_note(state) {
        body = body.push(note);
    }
    body = body.push(
        DataTable::new(columns)
            .searchable(FleetRow::search_key)
            .on_sort(Message::FleetTableSort)
            .on_filter(Message::FleetTableFilter)
            .noun("producers")
            .view(rows, &state.table),
    );

    if let Some(id) = &state.expanded
        && let Some(r) = rows.iter().find(|r| &row_id(r) == id)
    {
        body = body.push(findings_panel(r));
    }
    body.into()
}

/// What the sweep's reply bound cost, said out loud (#745).
///
/// Silent truncation is the failure mode a bounded fan-out invites: the table
/// looks complete, and looks *more* complete the larger the fleet grows. A
/// sweep that dropped replies is an incomplete inventory and says so.
fn elision_note(state: &FleetState) -> Option<Element<'static, Message>> {
    elision_summary(state.elided, state.bound).map(|note| badge(theme::STATUS_DEGRADED, note))
}

/// The sentence [`elision_note`] renders, as a value — so a test can pin the
/// wording without going through a widget tree.
pub fn elision_summary(elided: u64, bound: usize) -> Option<String> {
    (elided > 0).then(|| {
        format!(
            "incomplete: {elided} repl{} arrived past the {bound}-reply bound and were \
             dropped — this inventory is a sample, not the fleet",
            if elided == 1 { "y" } else { "ies" },
        )
    })
}

/// The findings for one row, spelled out. A count in a cell tells you something
/// is wrong; this tells you what.
fn findings_panel(r: &FleetRow) -> Element<'_, Message> {
    let mut col =
        column![text(format!("{} · {} — findings", r.host, r.producer)).size(font::EMPHASIS),]
            .spacing(space::XS);
    for f in &r.findings {
        col = col.push(text(f.clone()).size(font::CAPTION));
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

    fn sweep(replies: Vec<FleetReply>) -> FleetSweep {
        FleetSweep {
            replies,
            elided: 0,
            bound: zenkey_fleet::DEFAULT_MAX_REPLIES,
        }
    }

    fn compiled(producer: &str) -> String {
        zensight_common::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == producer)
            .map(|(_, t)| (*t).to_string())
            .expect("producer is in the compiled registry")
    }

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
        let rows = build_rows(
            &sweep(vec![FleetReply {
                origin: "h-aaaaaaaaaaaa".into(),
                producer: "sysinfo".into(),
                toml: compiled("sysinfo"),
            }]),
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
            &sweep(vec![FleetReply {
                origin: "h-bbbbbbbbbbbb".into(),
                producer: "sysinfo".into(),
                toml: slice_toml("sysinfo", "9.9", Some("cpu/usage")),
            }]),
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Skew);
        assert_eq!(rows[0].version, "9.9");
        assert!(
            rows[0].findings.iter().any(|f| f.contains("9.9")),
            "the engine's rendered version-skew finding names the served version: {:?}",
            rows[0].findings
        );
    }

    /// Same version, different content — the alarming one, and the case a
    /// version-only check misses entirely.
    #[test]
    fn same_version_different_content_is_drift() {
        let version = zenkey::slice::parse_slice(&compiled("sysinfo"))
            .unwrap()
            .version;
        let rows = build_rows(
            &sweep(vec![FleetReply {
                origin: "h-dddddddddddd".into(),
                producer: "sysinfo".into(),
                toml: slice_toml("sysinfo", &version, Some("cpu/invented")),
            }]),
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Drift);
        assert!(!rows[0].findings.is_empty());
    }

    /// A producer only the *fleet* knows is skew, not silence: it is newer than
    /// us, and the engine's set-level join is what says so.
    #[test]
    fn a_producer_we_never_compiled_in_is_skew() {
        let rows = build_rows(
            &sweep(vec![FleetReply {
                origin: "h-eeeeeeeeeeee".into(),
                producer: "invented".into(),
                toml: slice_toml("invented", "1.0", Some("thing/count")),
            }]),
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Skew);
        assert!(
            rows[0]
                .findings
                .iter()
                .any(|f| f.contains("absent from the local registry")),
            "{:?}",
            rows[0].findings
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
        let rows = build_rows(&sweep(Vec::new()), &alive);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, FleetStatus::Silent);
        assert_eq!(rows[0].host, "edge01");
    }

    /// Worst first: a drifting host must not sort below ten healthy ones.
    #[test]
    fn rows_sort_worst_first() {
        let rows = build_rows(
            &sweep(vec![
                FleetReply {
                    origin: "h-aaaaaaaaaaaa".into(),
                    producer: "sysinfo".into(),
                    toml: compiled("sysinfo"),
                },
                FleetReply {
                    origin: "h-bbbbbbbbbbbb".into(),
                    producer: "sysinfo".into(),
                    toml: "not toml at all {{{".into(),
                },
            ]),
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Drift);
        assert_eq!(rows[0].version, "unreadable");
        assert_eq!(rows[1].status, FleetStatus::InSync);
    }

    /// One host's disagreement must stay one host's: diffing per origin is what
    /// keeps a skewed edge box from tarring the server that is fine.
    #[test]
    fn hosts_are_diffed_independently() {
        let rows = build_rows(
            &sweep(vec![
                FleetReply {
                    origin: "h-aaaaaaaaaaaa".into(),
                    producer: "sysinfo".into(),
                    toml: compiled("sysinfo"),
                },
                FleetReply {
                    origin: "h-bbbbbbbbbbbb".into(),
                    producer: "sysinfo".into(),
                    toml: slice_toml("sysinfo", "0.1", None),
                },
            ]),
            &[],
        );
        assert_eq!(rows.len(), 2);
        let by_origin: std::collections::HashMap<_, _> =
            rows.iter().map(|r| (r.origin.as_str(), r.status)).collect();
        assert_eq!(by_origin["h-aaaaaaaaaaaa"], FleetStatus::InSync);
        assert_eq!(by_origin["h-bbbbbbbbbbbb"], FleetStatus::Skew);
    }

    #[test]
    fn renders_a_populated_table() {
        let mut state = FleetState::default();
        state.apply(
            Ok(sweep(vec![FleetReply {
                origin: "h-aaaaaaaaaaaa".into(),
                producer: "sysinfo".into(),
                toml: compiled("sysinfo"),
            }])),
            &[("h-aaaaaaaaaaaa".into(), "sysinfo".into(), "server01".into())],
        );
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(ui.find("server01").is_ok());
        assert!(ui.find("in sync").is_ok());
    }

    /// A truncated sweep says so. Without this the table reads as the whole
    /// fleet, and reads that way *more* readily the larger the fleet is.
    #[test]
    fn a_truncated_sweep_says_what_the_bound_cost() {
        let mut state = FleetState::default();
        state.apply(
            Ok(FleetSweep {
                replies: vec![FleetReply {
                    origin: "h-aaaaaaaaaaaa".into(),
                    producer: "sysinfo".into(),
                    toml: compiled("sysinfo"),
                }],
                elided: 7,
                bound: 1,
            }),
            &[],
        );
        let note = elision_summary(7, 1).expect("a sweep that dropped replies has a note");
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(ui.find(note.as_str()).is_ok(), "the note reads: {note}");
        assert_eq!(
            elision_summary(0, 4096),
            None,
            "a complete sweep says nothing about a bound it never hit"
        );
    }
}
