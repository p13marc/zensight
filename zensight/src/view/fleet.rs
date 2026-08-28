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
//! (we have its sensor doc) but answers no `introspect` is still listed.
//! Reporting only what answered would let exactly the hosts you most need to
//! see disappear from the inventory.
//!
//! # The four poles (#746)
//!
//! Every row is a judgement about one claim — *this host serves the slice we
//! compiled in* — and RFC 13 says a judgement has four poles, not two:
//!
//! | pole | rows | what it means |
//! |---|---|---|
//! | `Established` | `in sync` | asked, answered, and the claim holds |
//! | `NotEstablished` | `version skew`, `drift` | asked, answered, and it does not |
//! | `Unobservable` | `no answer`, `unreadable` | asked, and the answer cannot carry the claim |
//! | `NotAsked` | `not asked` | the question never reached this host |
//!
//! This used to be one state, `silent`, doing two jobs — and the dangerous
//! half is `NotAsked`. A host missing because the sweep's reply bound cut the
//! fan-in short rendered exactly like a fleet-wide failure to answer, and did
//! so *more* readily the larger the fleet grew, which is backwards. RFC 09
//! §5.1 O4: **not asked is not answered no.** Neither unestablished pole may
//! borrow the swatch of an answer, and neither may read as a passing check;
//! see [`crate::view::theme::JUDGEMENT_UNOBSERVABLE`] for the scale.
//!
//! [`FleetStatus`] is the *surface* vocabulary — six namings over the four
//! poles, because `version skew` and `drift` deserve different colours even
//! though both are `NotEstablished`. [`FleetStatus::judgement`] is the
//! documented mapping back onto the core, and it is what the tally line above
//! the table counts.
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

use zenkey_fleet::{Judgement, SliceSet};

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

/// What a host is, relative to us — the surface vocabulary over RFC 13's four
/// poles. [`FleetStatus::judgement`] is the mapping; see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetStatus {
    /// `Established` — serves exactly the slice we compiled in.
    InSync,
    /// `NotEstablished` — serves a different `[registry] version`.
    Skew,
    /// `NotEstablished` — same version, different content. The more alarming
    /// case of the two: a version number that agrees is a claim that the two
    /// builds are the same one.
    Drift,
    /// `Unobservable` — it answered, and the answer cannot be interpreted.
    Unreadable,
    /// `Unobservable` — alive on the bus, and it answered nothing. An old
    /// build with no queryable, or a broken one. **Not** a verdict about the
    /// slice it serves: RFC 05 §3.1, silence is not one condition.
    NoAnswer,
    /// `NotAsked` — the sweep never reached it. The question was not put, and
    /// "not asked" is not "answered no" (RFC 09 §5.1 O4).
    NotAsked,
}

impl FleetStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::InSync => "in sync",
            Self::Skew => "version skew",
            Self::Drift => "drift",
            Self::Unreadable => "unreadable",
            Self::NoAnswer => "no answer",
            Self::NotAsked => "not asked",
        }
    }

    /// The RFC 13 pole this naming maps onto, carrying the row's own reason.
    ///
    /// The convention every `to_judgement` in the upstream engine follows:
    /// *the judged claim is the claim*, so a host serving our slice is
    /// `Established` and one that does not is `NotEstablished`. The two
    /// unestablished poles carry the reason, because that is where the honesty
    /// lives — an empty `Unobservable` is barely better than a `false`.
    pub fn judgement(self, reason: &str) -> Judgement {
        match self {
            Self::InSync => Judgement::Established,
            Self::Skew | Self::Drift => Judgement::NotEstablished {
                reason: reason.to_string(),
            },
            Self::Unreadable | Self::NoAnswer => Judgement::Unobservable {
                reason: reason.to_string(),
            },
            Self::NotAsked => Judgement::NotAsked,
        }
    }

    /// Sort worst-first: the whole point of the view is to surface the odd one
    /// out, so an alphabetical sort on the status column would bury it.
    ///
    /// The unestablished poles sort **between** the findings and the clean
    /// rows, and that position is the whole ordering argument: they are not
    /// verdicts, so they must not outrank one — and they are not passing
    /// checks, so they must not sink below one either.
    fn severity(self) -> u8 {
        match self {
            Self::Drift => 0,
            Self::Unreadable => 1,
            Self::NoAnswer => 2,
            Self::Skew => 3,
            Self::NotAsked => 4,
            Self::InSync => 5,
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
    /// Served registry version; empty when nothing was served.
    pub version: String,
    pub subjects: usize,
    pub procedures: usize,
    pub status: FleetStatus,
    /// Why this row is not `in sync`, in one sentence — the `reason` RFC 13's
    /// two "with the reason" poles require. Empty for `InSync` and `NotAsked`,
    /// the two poles that carry none.
    pub reason: String,
    /// RFC 08 §6 findings, already rendered by the engine
    /// ([`zenkey_fleet::report::ProducerDiff`]).
    pub findings: Vec<String>,
}

impl FleetRow {
    fn search_key(&self) -> String {
        format!("{} {} {}", self.host, self.producer, self.status.label())
    }

    /// This row as RFC 13's four-pole core.
    pub fn judgement(&self) -> Judgement {
        self.status.judgement(&self.reason)
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
    /// Which row's `why` panel is expanded.
    pub expanded: Option<String>,
    /// What the last sweep's reply bound refused, and the bound itself.
    pub elided: u64,
    pub bound: usize,
}

impl FleetState {
    pub fn loading(&mut self) {
        self.rows = Fetch::Loading;
    }

    /// Fold the sweep into rows: diff each host's served slices against the
    /// slices this build compiled in, and give every alive producer that did
    /// not answer the unestablished pole it has actually earned.
    pub fn apply(&mut self, result: Result<FleetSweep, String>, alive: &[AliveProducer]) {
        // Zeroed on failure, not carried over: what a *previous* sweep's bound
        // refused says nothing about this one, and a stale banner is a claim.
        self.elided = result.as_ref().map_or(0, |s| s.elided);
        self.bound = result.as_ref().map_or(0, |s| s.bound);
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
                reason: diff.findings.join("; "),
                findings: diff.findings,
                producer: diff.producer,
            });
        }
    }

    // It answered, and we cannot read the answer. `Unobservable`, not `drift`:
    // drift is a claim about the *content* of a slice we managed to parse, and
    // this is the case where we did not (RFC 09 §5.1 O6).
    for (origin, producer, why) in unreadable {
        let reason = format!("the served slice did not parse: {why}");
        rows.push(FleetRow {
            origin: origin.to_string(),
            host: name_of(origin, producer),
            producer: producer.to_string(),
            version: "unreadable".into(),
            subjects: 0,
            procedures: 0,
            status: FleetStatus::Unreadable,
            findings: vec![reason.clone()],
            reason,
        });
    }

    // Alive on the bus and absent from the sweep. Which of the two unestablished
    // poles that is depends on whether the sweep was **whole** (#746).
    //
    // A truncated sweep is the dangerous case. Past the reply bound the replies
    // are drained but not kept, so this producer may have answered and had its
    // answer thrown away, or may never have been reached at all — and we cannot
    // tell which. Reporting "alive, and it answered nothing" about a host whose
    // answer we discarded is precisely the false verdict RFC 09 §5.1 O4
    // forbids, so a truncated sweep says `not asked` and names the bound.
    // A whole sweep genuinely did put the question, and got nothing back:
    // `Unobservable`, with the two explanations that fit.
    for (origin, producer, host) in alive {
        let answered = sweep
            .replies
            .iter()
            .any(|r| &r.origin == origin && &r.producer == producer);
        if answered {
            continue;
        }
        let (status, reason) = if sweep.elided > 0 {
            (
                FleetStatus::NotAsked,
                format!(
                    "the sweep's {}-reply bound refused {} repl{}, so the question may never \
                     have reached this producer — not asked is not answered no",
                    sweep.bound,
                    sweep.elided,
                    if sweep.elided == 1 { "y" } else { "ies" },
                ),
            )
        } else {
            (
                FleetStatus::NoAnswer,
                "alive on the bus, and served no introspect reply — an old build with no \
                 queryable, or a broken one"
                    .to_string(),
            )
        };
        rows.push(FleetRow {
            origin: origin.clone(),
            host: host.clone(),
            producer: producer.clone(),
            version: String::new(),
            subjects: 0,
            procedures: 0,
            status,
            reason,
            findings: Vec::new(),
        });
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

/// The badge for one row. The colour resolves through the pole, never through
/// the naming: the two unestablished poles get the two swatches that are not
/// answers, so `not asked` can never be read as `no` and neither can be read
/// as a passing check (#746, RFC 09 §5.1 O4/O6).
fn status_badge(status: FleetStatus) -> Element<'static, Message> {
    let color = match status.judgement("") {
        Judgement::Established => theme::STATUS_ONLINE,
        // Established(no) — the one axis the surface vocabulary is finer than
        // the core on: a version that differs is a rollout, content that
        // differs under an equal version is a lie about the build.
        Judgement::NotEstablished { .. } => match status {
            FleetStatus::Skew => theme::STATUS_DEGRADED,
            _ => theme::STATUS_OFFLINE,
        },
        Judgement::Unobservable { .. } => theme::JUDGEMENT_UNOBSERVABLE,
        Judgement::NotAsked => theme::STATUS_UNKNOWN,
    };
    badge(color, status.label())
}

/// The four-pole tally over the whole table (#746).
///
/// The line exists so the unestablished half of the inventory has a number
/// attached to it. A screen of rows where three say `not asked` reads as a
/// healthy fleet at a glance; "3 not asked" does not.
pub fn pole_tally(rows: &[FleetRow]) -> String {
    let (mut established, mut findings, mut unobservable, mut not_asked) = (0, 0, 0, 0);
    for row in rows {
        let judgement = row.judgement();
        match judgement.conclusive() {
            Some(true) => established += 1,
            Some(false) => findings += 1,
            None if judgement.is_not_asked() => not_asked += 1,
            None => unobservable += 1,
        }
    }
    format!(
        "{established} in sync · {findings} finding(s) · {unobservable} unobservable · \
         {not_asked} not asked"
    )
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
        // "why" rather than "findings": an unestablished row has no findings —
        // that is what unestablished means — but it does have a reason, and
        // burying it behind an empty cell was how `silent` got away with doing
        // two jobs (#746).
        TableColumn::fixed("why", 110.0, |r: &FleetRow| {
            let label = match (r.findings.len(), r.reason.is_empty()) {
                (0, true) => return text("—").size(font::CAPTION).into(),
                (0, false) => "why?".to_string(),
                (n, _) => format!("{n} finding(s)"),
            };
            button(text(label).size(font::CAPTION))
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
        text(pole_tally(rows)).size(font::CAPTION),
    ]
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

/// Why one row is not `in sync`, spelled out. A badge tells you something is
/// off; this tells you what — and for the two unestablished poles it is the
/// only place the `reason` RFC 13 requires them to carry is legible.
fn findings_panel(r: &FleetRow) -> Element<'_, Message> {
    let mut col = column![
        text(format!(
            "{} · {} — {}",
            r.host,
            r.producer,
            r.status.label()
        ))
        .size(font::EMPHASIS),
    ]
    .spacing(space::XS);
    if r.findings.is_empty() {
        col = col.push(text(r.reason.clone()).size(font::CAPTION));
    }
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
        assert_eq!(rows[0].judgement(), Judgement::Established);
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
        assert_eq!(
            rows[0].judgement().conclusive(),
            Some(false),
            "drift is an answer — we asked, it answered, and the claim does not hold"
        );
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

    fn alive_edge01() -> Vec<AliveProducer> {
        vec![(
            "h-cccccccccccc".to_string(),
            "netring".to_string(),
            "edge01".to_string(),
        )]
    }

    /// Alive on the bus but no answer: the row that would otherwise vanish, and
    /// the one you most need to see. The sweep was **whole**, so the question
    /// really was put and really got nothing back — `Unobservable`, with the
    /// reason RFC 13 requires.
    #[test]
    fn an_alive_producer_that_does_not_answer_is_unobservable() {
        let rows = build_rows(&sweep(Vec::new()), &alive_edge01());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, FleetStatus::NoAnswer);
        assert_eq!(rows[0].host, "edge01");
        let judgement = rows[0].judgement();
        assert!(judgement.is_unobservable(), "{judgement:?}");
        assert_eq!(judgement.conclusive(), None, "silence is not a verdict");
        assert!(rows[0].reason.contains("no introspect reply"));
    }

    /// **The point of #746.** The same missing producer, under a sweep whose
    /// reply bound cut the fan-in short, is `NotAsked` — not `NotEstablished`,
    /// and not the same pole as the host that was asked and stayed quiet.
    ///
    /// Truncation gets *more* likely the larger the fleet grows, so a fleet
    /// that outgrew the bound would otherwise read as a fleet-wide failure to
    /// answer, and read that way more confidently the worse the truncation.
    #[test]
    fn a_producer_beyond_the_reply_bound_is_not_asked_not_unanswered() {
        let truncated = FleetSweep {
            replies: Vec::new(),
            elided: 12,
            bound: 4,
        };
        let rows = build_rows(&truncated, &alive_edge01());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, FleetStatus::NotAsked);

        let judgement = rows[0].judgement();
        assert_eq!(judgement, Judgement::NotAsked);
        assert!(judgement.is_not_asked());
        assert!(
            !judgement.is_unobservable(),
            "not asked is a different pole from asked-and-could-not-tell (O6)"
        );
        assert_eq!(
            judgement.conclusive(),
            None,
            "not asked is not answered no (RFC 09 §5.1 O4)"
        );

        // And the same producer, asked properly, lands on the other pole.
        let whole = build_rows(&sweep(Vec::new()), &alive_edge01());
        assert_ne!(
            whole[0].status, rows[0].status,
            "a truncated sweep must not render like a fleet that answered nothing"
        );
        assert_ne!(whole[0].judgement(), rows[0].judgement());
    }

    /// The distinction has to survive into pixels, not just into the enum: the
    /// two unestablished poles get different labels *and* different swatches,
    /// and neither is an answer's swatch.
    #[test]
    fn not_asked_renders_distinguishably_from_every_other_pole() {
        use crate::view::theme;

        // Four labels, four different strings — the badge carries meaning in
        // text as well as colour, so a colour-blind reader gets it too.
        let labels: Vec<&str> = [
            FleetStatus::InSync,
            FleetStatus::Skew,
            FleetStatus::Drift,
            FleetStatus::Unreadable,
            FleetStatus::NoAnswer,
            FleetStatus::NotAsked,
        ]
        .iter()
        .map(|s| s.label())
        .collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");

        // The swatches: `not asked` shares none of them.
        let not_asked = theme::STATUS_UNKNOWN;
        for other in [
            theme::STATUS_ONLINE,
            theme::STATUS_DEGRADED,
            theme::STATUS_OFFLINE,
            theme::JUDGEMENT_UNOBSERVABLE,
        ] {
            assert_ne!(
                (not_asked.r, not_asked.g, not_asked.b),
                (other.r, other.g, other.b),
                "not asked must not borrow another pole's swatch"
            );
        }

        // And on screen, both rows are present and separately readable.
        let mut state = FleetState::default();
        state.apply(
            Ok(FleetSweep {
                replies: Vec::new(),
                elided: 3,
                bound: 1,
            }),
            &alive_edge01(),
        );
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(ui.find("not asked").is_ok());
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(
            ui.find("no answer").is_err(),
            "a truncated sweep must not claim the host answered nothing"
        );

        let mut state = FleetState::default();
        state.apply(Ok(sweep(Vec::new())), &alive_edge01());
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(ui.find("no answer").is_ok());
        let mut ui = iced_test::simulator(fleet_view(&state));
        assert!(ui.find("not asked").is_err());
    }

    /// The four poles, counted — so an inventory whose unestablished half is
    /// three rows deep says "3", instead of looking like a healthy fleet.
    #[test]
    fn the_tally_counts_all_four_poles() {
        let rows = build_rows(
            &FleetSweep {
                replies: vec![
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
                    FleetReply {
                        origin: "h-cccccccccccc".into(),
                        producer: "sysinfo".into(),
                        toml: "not toml at all {{{".into(),
                    },
                ],
                elided: 2,
                bound: 2,
            },
            &alive_edge01(),
        );
        assert_eq!(
            pole_tally(&rows),
            "1 in sync · 1 finding(s) · 1 unobservable · 1 not asked"
        );
    }

    /// A slice we cannot read is `Unobservable`, never `drift`: drift is a
    /// claim about the content of a slice we managed to parse.
    #[test]
    fn an_unreadable_slice_is_unobservable_not_drift() {
        let rows = build_rows(
            &sweep(vec![FleetReply {
                origin: "h-bbbbbbbbbbbb".into(),
                producer: "sysinfo".into(),
                toml: "not toml at all {{{".into(),
            }]),
            &[],
        );
        assert_eq!(rows[0].status, FleetStatus::Unreadable);
        assert!(rows[0].judgement().is_unobservable());
        assert!(rows[0].reason.contains("did not parse"));
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
        assert_eq!(rows[0].status, FleetStatus::Unreadable);
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
