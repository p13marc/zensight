//! Alerts view for threshold-based notifications.

use std::collections::{BTreeMap, HashMap, VecDeque};

use iced::widget::{Column, Row, column, container, row, rule, scrollable, text, tooltip};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{Alert as SensorAlert, AlertState as SensorAlertState, Protocol};

use crate::message::Message;
use crate::view::components::{badge, empty_state, section_header};
use crate::view::formatting::format_timestamp;
use crate::view::icons::{self, IconSize};
use crate::view::tokens::{font, space};

/// Current wall-clock time in epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A saved external-alert filter combination (#27). Applying it sets both the
/// severity and source filters at once; persisted in `PersistentSettings`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AlertFilterPreset {
    /// Display name (auto-derived from the filters at save time).
    pub name: String,
    /// Severity filter (`None` = any severity).
    #[serde(default)]
    pub severity: Option<zensight_common::AlertSeverity>,
    /// Source filter (`None` = any source).
    #[serde(default)]
    pub source: Option<String>,
}

/// Alert severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    /// Informational alert.
    Info,
    /// Warning that may need attention.
    #[default]
    Warning,
    /// Critical issue requiring immediate attention.
    Critical,
}

impl Severity {
    /// All severity levels.
    pub const ALL: &'static [Severity] = &[Severity::Info, Severity::Warning, Severity::Critical];

    /// Get the display name for this severity.
    pub fn name(&self) -> &'static str {
        match self {
            Severity::Info => "Info",
            Severity::Warning => "Warning",
            Severity::Critical => "Critical",
        }
    }

    /// Get the color for this severity (RGB). Sourced from the shared severity
    /// palette (D2 single source of truth).
    pub fn color(&self) -> iced::Color {
        match self {
            Severity::Info => crate::view::theme::SEVERITY_INFO,
            Severity::Warning => crate::view::theme::SEVERITY_WARNING,
            Severity::Critical => crate::view::theme::SEVERITY_CRITICAL,
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl From<zensight_common::AlertSeverity> for Severity {
    fn from(s: zensight_common::AlertSeverity) -> Self {
        use zensight_common::AlertSeverity;
        match s {
            AlertSeverity::Info => Severity::Info,
            AlertSeverity::Warning => Severity::Warning,
            AlertSeverity::Critical => Severity::Critical,
        }
    }
}

/// The other direction (#933): the authoring form picks a [`Severity`], and a
/// threshold rule the sensor evaluates carries an `AlertSeverity`. Without
/// this the form would have to spell the mapping at its one call site, which
/// is how the two ended up able to drift.
impl From<Severity> for zensight_common::AlertSeverity {
    fn from(s: Severity) -> Self {
        use zensight_common::AlertSeverity;
        match s {
            Severity::Info => AlertSeverity::Info,
            Severity::Warning => AlertSeverity::Warning,
            Severity::Critical => AlertSeverity::Critical,
        }
    }
}

/// Comparison operators for alert rules — shared with the sensors' headless
/// `metric-threshold` expectations (see `zensight_common::ComparisonOp`).
pub use zensight_common::ComparisonOp;

/// State for the alerts system.
#[derive(Debug, Default)]
pub struct AlertsState {
    /// Sensor-pushed alerts (anomalies + expectation violations), keyed by the
    /// alert's stable `alert_key`. Lifecycle-managed: firing inserts/updates,
    /// resolved removes. Rendered alongside rule-triggered alerts (Plan 07).
    pub external: HashMap<String, SensorAlert>,
    /// Acknowledgements, **as published by the catalog** (#925).
    ///
    /// This was `acknowledged_external: HashSet<String>` — an ack lived in this
    /// process, died with the window, was invisible to a second GUI, and could
    /// not be told from a new alert by either exporter. It is a projection of
    /// `@catalog/state/ack/*` now; the GUI writes through `@rpc/@catalog/ack`
    /// and reads back what the catalog decided.
    ///
    /// The **projection rule** (RFC 06 §5.5) is applied on read, in
    /// [`AlertsState::is_acked`], never on ingest: an ack applies only while a
    /// firing alert with `timestamp <= fired_at` exists. So an orphan is inert
    /// and a re-fire is not acknowledged, without this map having to be
    /// pruned in step with the alert feed.
    acks: BTreeMap<zensight_common::alert::AlertRef, zensight_common::ack::AlertAck>,
    /// The publishing origin chunk (`h-<12hex>`) of each firing external
    /// alert, by its in-GUI key — read from the key the alert arrived on.
    /// A Delete tombstone carries no payload, so the origin and the hash are
    /// all it has; this is what lets it find the `(source, hash)` entry.
    external_origins: HashMap<String, String>,
    /// Suppression windows, **as published by the catalog** (#925).
    ///
    /// This was `silenced_sources: HashMap<String, i64>` — whole-source only,
    /// no matchers, no author, and local to one window. A `Silence` matches on
    /// origin / producer / source / rule / `labels.*`, which is what "mute the
    /// disk alerts on rack 3 while the SAN is down" needs and a source list
    /// never could.
    silences: Vec<zensight_common::silence::Silence>,
    /// The catalog's incident documents, by id (#925).
    ///
    /// Preferred over the local grouping when present, because the catalog
    /// keys by **entity** — a host that publishes under three origins is one
    /// incident there and three here. The local `group_incidents` stays as the
    /// **offline fallback**: a GUI with no catalog must still show what is on
    /// fire, one join weaker, which is exactly what RFC 06 §5 promises
    /// consumers.
    catalog_incidents: BTreeMap<String, zensight_common::incident::Incident>,
    /// Whether `@catalog/state/alive` is present.
    ///
    /// The catalog is the only writer of acks and silences, so with it gone
    /// the GUI cannot acknowledge anything — and must **say so** rather than
    /// offering a button that quietly does nothing. `None` = not yet known.
    pub catalog_alive: Option<bool>,
    /// Per-`alert_key` incident timeline: firing→resolved transitions (#26).
    /// Bounded to the most recent transitions so it never grows unbounded.
    timelines: HashMap<String, VecDeque<TransitionEvent>>,
    /// Severity filter for the external-alerts feed (#27). `None` = show all;
    /// otherwise only alerts of exactly this severity are shown.
    pub external_severity_filter: Option<zensight_common::AlertSeverity>,
    /// Source filter for the external-alerts feed (#27). `None` = all sources;
    /// otherwise only alerts from this source are shown.
    pub external_source_filter: Option<String>,
    /// Protocol filter for the external-alerts feed (#582). `None` = all;
    /// set by the pill row or an overview tile's click-through.
    pub external_protocol_filter: Option<zensight_common::Protocol>,
    /// The `<source>/<alert_key>` an operator pivoted to from an event record
    /// (#651). Highlights that row, and deliberately survives the alert
    /// resolving so the view can say "no longer firing" instead of showing
    /// nothing — a link that silently lands on an empty list is worse than the
    /// device-scoped pivot it replaced.
    pub focused_external: Option<String>,
    /// Saved filter presets (#27): named severity+source combinations the user
    /// can re-apply in one click. Persisted in `PersistentSettings`.
    pub alert_filter_presets: Vec<AlertFilterPreset>,
}

/// One firing/resolved transition in an incident's timeline (#26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionEvent {
    /// The state entered at this transition.
    pub state: SensorAlertState,
    /// When it happened (epoch ms).
    pub at: i64,
}

/// Max transitions kept per incident timeline (bounded buffer).
const MAX_TIMELINE_EVENTS: usize = 32;

/// A group of firing external alerts from one source (an "incident"), for the
/// grouped/acknowledge-able anomalies feed.
pub struct ExternalIncident<'a> {
    pub source: &'a str,
    /// Alerts in this group, severity-then-recency order.
    pub alerts: Vec<&'a SensorAlert>,
    /// How many of them are not yet acknowledged.
    pub unacked: usize,
    /// Highest severity in the group.
    pub top_severity: Option<zensight_common::AlertSeverity>,
}

/// Outcome of ingesting a sensor-pushed alert, so the app can decide whether to
/// raise a toast (new), stay quiet (update), or toast a recovery (resolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAlertOutcome {
    New,
    Updated,
    Resolved,
    /// A resolve for an alert we weren't tracking — ignored.
    Unknown,
}

impl AlertsState {
    /// Create a new alerts state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a sensor-pushed alert. Firing alerts are inserted/updated by
    /// `alert_key`; resolved alerts are removed. Returns what happened so the
    /// caller can toast appropriately.
    /// The in-GUI identity of an external alert. v1 (epic #453): the
    /// `alert_key` hash no longer includes the source — the wire key's
    /// origin chunk scopes it — so the GUI scopes by (source, hash).
    pub fn external_key(alert: &SensorAlert) -> String {
        format!("{}/{}", alert.source, alert.alert_key())
    }

    pub fn ingest_external(&mut self, alert: SensorAlert) -> ExternalAlertOutcome {
        self.ingest_external_from(None, alert)
    }

    /// [`ingest_external`](Self::ingest_external), remembering the origin
    /// chunk of the key the alert arrived on (when the caller has it) so a
    /// later tombstone from that origin can find the entry.
    pub fn ingest_external_from(
        &mut self,
        origin: Option<String>,
        alert: SensorAlert,
    ) -> ExternalAlertOutcome {
        // v1 (epic #453): the alert_key hash no longer includes the source —
        // the wire key's origin chunk scopes it. Keep the in-GUI map keyed by
        // (source, hash) so distinct hosts' alerts never collide here.
        let key = Self::external_key(&alert);
        match alert.state {
            SensorAlertState::Resolved => {
                if self.external.remove(&key).is_some() {
                    // The ack projection needs no pruning here (#925): an ack
                    // applies only while a firing alert with
                    // `timestamp <= fired_at` exists, so a resolve makes it
                    // stop applying by itself. That is the same property the
                    // old `HashSet` needed this line to fake — and faked
                    // incompletely, because it could not see a *re-fire*.
                    // The catalog tombstones the document; until it does, the
                    // rule already reads it as not-acked.
                    self.external_origins.remove(&key);
                    self.record_transition(&key, SensorAlertState::Resolved, alert.timestamp);
                    ExternalAlertOutcome::Resolved
                } else {
                    ExternalAlertOutcome::Unknown
                }
            }
            SensorAlertState::Firing => {
                if let Some(o) = origin {
                    self.external_origins.insert(key.clone(), o);
                }
                let outcome = if self.external.contains_key(&key) {
                    ExternalAlertOutcome::Updated
                } else {
                    // Only a fresh firing (not an update of an existing one) is a
                    // new timeline transition.
                    self.record_transition(&key, SensorAlertState::Firing, alert.timestamp);
                    ExternalAlertOutcome::New
                };
                self.external.insert(key, alert);
                outcome
            }
        }
    }

    /// Append a transition to an incident's bounded timeline (#26).
    fn record_transition(&mut self, key: &str, state: SensorAlertState, at: i64) {
        let tl = self.timelines.entry(key.to_string()).or_default();
        tl.push_back(TransitionEvent { state, at });
        while tl.len() > MAX_TIMELINE_EVENTS {
            tl.pop_front();
        }
    }

    /// The recorded transition timeline for an incident, oldest-first (#26).
    pub fn timeline(&self, alert_key: &str) -> Vec<TransitionEvent> {
        self.timelines
            .get(alert_key)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default()
    }

    /// A firing external alert's **wire ref** (#925), or `None` when the GUI
    /// never saw the origin its key arrived on.
    ///
    /// The three components come from three places, and none of them is the
    /// payload's `source`: the origin from the key the alert arrived on, the
    /// producer from its protocol, the hash from the in-GUI key. `source` is
    /// the polled device for a proxy sensor (#883), so building a ref from it
    /// would address the wrong host.
    ///
    /// `None` is honest rather than a guess: the ack the GUI would write goes
    /// on `ack/<alert_ref>`, and a ref naming the wrong origin is an ack for
    /// somebody else's alert.
    pub fn alert_ref_for(&self, in_gui_key: &str) -> Option<zensight_common::alert::AlertRef> {
        let alert = self.external.get(in_gui_key)?;
        let origin = self.external_origins.get(in_gui_key)?;
        let hash = in_gui_key.rsplit('/').next()?;
        zensight_common::alert::AlertRef::parse(&format!("{origin}.{}.{hash}", alert.protocol)).ok()
    }

    /// Replace the ack projection from the bus (#925).
    pub fn set_acks(
        &mut self,
        acks: BTreeMap<zensight_common::alert::AlertRef, zensight_common::ack::AlertAck>,
    ) {
        self.acks = acks;
    }

    /// Apply one ack document (a live sample).
    pub fn ingest_ack(&mut self, ack: zensight_common::ack::AlertAck) {
        self.acks.insert(ack.alert_ref.clone(), ack);
    }

    /// Apply an ack tombstone.
    pub fn retire_ack(&mut self, r: &zensight_common::alert::AlertRef) {
        self.acks.remove(r);
    }

    /// Replace the silence projection from the bus (#925).
    pub fn set_silences(&mut self, silences: Vec<zensight_common::silence::Silence>) {
        self.silences = silences;
    }

    /// Apply one silence document.
    pub fn ingest_silence(&mut self, s: zensight_common::silence::Silence) {
        self.silences.retain(|x| x.id != s.id);
        self.silences.push(s);
    }

    /// Apply a silence tombstone.
    pub fn retire_silence(&mut self, id: &str) {
        self.silences.retain(|x| x.id != id);
    }

    /// Whether any live silence suppresses this firing alert at `now_ms`
    /// (#925).
    ///
    /// Evaluated per **alert**, not per source: a `Silence` matches on
    /// origin / producer / source / rule / `labels.*`, and collapsing that to
    /// "is this source muted" would throw away every matcher that made the
    /// window worth opening.
    pub fn is_silenced_alert(&self, in_gui_key: &str, now_ms: i64) -> bool {
        let Some(alert) = self.external.get(in_gui_key) else {
            return false;
        };
        let origin = self
            .external_origins
            .get(in_gui_key)
            .map(String::as_str)
            .unwrap_or_default();
        let producer = alert.protocol.to_string();
        zensight_common::silence::Silence::any_matches(
            &self.silences,
            now_ms,
            origin,
            &producer,
            alert,
        )
    }

    /// How many firing alerts a live silence is currently suppressing (the
    /// muted chip).
    pub fn silenced_count(&self, now_ms: i64) -> usize {
        self.external
            .keys()
            .filter(|k| self.is_silenced_alert(k, now_ms))
            .count()
    }

    /// The live silence windows, for the authoring pane.
    pub fn silences(&self) -> &[zensight_common::silence::Silence] {
        &self.silences
    }

    /// Apply one incident document.
    pub fn ingest_incident(&mut self, inc: zensight_common::incident::Incident) {
        self.catalog_incidents.insert(inc.id.clone(), inc);
    }

    /// Apply an incident tombstone — no member is firing any more.
    pub fn retire_incident(&mut self, id: &str) {
        self.catalog_incidents.remove(id);
    }

    /// The catalog's incidents, worst-first, or empty when it has published
    /// none (which is also what "no catalog" looks like).
    pub fn catalog_incidents(&self) -> Vec<&zensight_common::incident::Incident> {
        let mut v: Vec<_> = self.catalog_incidents.values().collect();
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.open().cmp(&a.open()))
                .then(b.last_change.cmp(&a.last_change))
                .then(a.id.cmp(&b.id))
        });
        v
    }

    /// Whether the incident list is the catalog's or this GUI's fallback.
    ///
    /// Rendered, not hidden: an operator reading a triage surface should know
    /// whether they are looking at the fleet's conclusion or their own
    /// window's approximation of it.
    pub fn incidents_are_from_catalog(&self) -> bool {
        !self.catalog_incidents.is_empty()
    }

    /// Whether the GUI can write an ack or a silence right now.
    ///
    /// The catalog is the only writer of both, so with it absent the buttons
    /// must be disabled and say why — an ack that silently does nothing is
    /// worse than one that refuses.
    pub fn can_write(&self) -> bool {
        self.catalog_alive.unwrap_or(false)
    }

    /// Clear an external alert by its in-GUI key (`<source>/<hash>`). Returns
    /// the removed alert, if any.
    pub fn clear_external(&mut self, key: &str) -> Option<SensorAlert> {
        self.external_origins.remove(key);
        self.external.remove(key)
    }

    /// Clear the external alert a Delete tombstone names. A tombstone has no
    /// payload, so it carries the origin chunk and the hash — never the
    /// `source` the in-GUI key is scoped by. For a while the bare hash was
    /// looked up directly and matched nothing, so every tombstone was a
    /// no-op and a stale Firing retired by #882's adoption sweep stayed on
    /// screen for good.
    ///
    /// The entry is the one under that hash whose recorded origin matches;
    /// when no entry recorded an origin (the demo feed, or an alert seeded
    /// before the origin was kept) a *unique* hash is enough. Two hosts
    /// firing the same rule with the same labels share a hash, and one's
    /// tombstone must not clear the other's — so an ambiguous hash with no
    /// origin to break the tie clears nothing.
    pub fn clear_external_from(&mut self, origin: &str, alert_key: &str) -> Option<SensorAlert> {
        let suffix = format!("/{alert_key}");
        let candidates: Vec<String> = self
            .external
            .keys()
            .filter(|k| k.ends_with(&suffix))
            .cloned()
            .collect();
        let key = match candidates.as_slice() {
            [] => return None,
            [only] if self.external_origins.get(only).is_none_or(|o| o == origin) => only.clone(),
            many => many
                .iter()
                .find(|k| self.external_origins.get(*k).is_some_and(|o| o == origin))?
                .clone(),
        };
        self.clear_external(&key)
    }

    /// The sources a live **source-wide** silence currently mutes, sorted —
    /// so that mute can be lifted from the UI, one source at a time (#925).
    ///
    /// Only single-`source` matcher sets appear here, because that is what the
    /// per-source Mute button opens and what its Unmute can honestly lift. A
    /// window matching `labels.unit` across a rack is not a "silenced source"
    /// and rendering it as one would offer an Unmute that lifted far more than
    /// it named; those live in the silences pane instead.
    pub fn silenced_sources_at(&self, now_ms: i64) -> Vec<String> {
        let mut out: Vec<String> = self
            .silences
            .iter()
            .filter(|s| now_ms >= s.starts_at && now_ms < s.ends_at)
            .filter(|s| s.matchers.len() == 1 && s.matchers[0].name == "source")
            .map(|s| s.matchers[0].value.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Iterate currently-firing sensor-pushed alerts, severity-then-recency order.
    /// How many firing bus alerts are not acknowledged (#934).
    ///
    /// This used to be a `unacknowledged_count` field maintained by the local
    /// rule engine, counting alerts that existed only in this process. The
    /// badge it feeds means more now, not less: it counts what the *fleet* is
    /// telling this GUI, which is the only alerting there is.
    pub fn unacknowledged_external(&self) -> usize {
        self.external
            .keys()
            .filter(|k| !self.is_external_acked(k))
            .count()
    }

    pub fn active_external(&self) -> Vec<&SensorAlert> {
        let mut v: Vec<&SensorAlert> = self.external.values().collect();
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.timestamp.cmp(&a.timestamp))
        });
        v
    }

    /// Has this external alert been acknowledged — **by the projection rule**
    /// (RFC 06 §5.5), not merely by an ack document existing?
    ///
    /// An ack applies only while a firing alert with `timestamp <= fired_at`
    /// exists. So an orphan left by a dead catalog reads as *not
    /// acknowledged*, and an alert that cleared and came back is not
    /// acknowledged either — the operator said "I am on this" about a
    /// different occurrence. The rule is applied here rather than on ingest so
    /// this map never has to be pruned in step with the alert feed.
    pub fn is_external_acked(&self, in_gui_key: &str) -> bool {
        let Some(alert) = self.external.get(in_gui_key) else {
            return false;
        };
        self.alert_ref_for(in_gui_key)
            .and_then(|r| self.acks.get(&r))
            .is_some_and(|ack| ack.applies_to(Some(alert)))
    }

    /// The ack document for a firing alert, when one applies (for the "acked
    /// by <who>" chip — the fact a `HashSet` could never carry).
    pub fn ack_for(&self, in_gui_key: &str) -> Option<&zensight_common::ack::AlertAck> {
        let alert = self.external.get(in_gui_key)?;
        let r = self.alert_ref_for(in_gui_key)?;
        self.acks.get(&r).filter(|a| a.applies_to(Some(alert)))
    }

    /// The refs of every firing alert from `source` — what an "Ack" button on
    /// an incident sends to `@rpc/@catalog/ack`, one call each (#925).
    ///
    /// It returns refs rather than acknowledging anything: since #925 this GUI
    /// is not the authority. An alert whose origin the GUI never saw is
    /// **skipped**, not guessed at — see [`AlertsState::alert_ref_for`].
    pub fn refs_for_source(&self, source: &str) -> Vec<zensight_common::alert::AlertRef> {
        self.external
            .iter()
            .filter(|(_, a)| a.source == source)
            .filter_map(|(k, _)| self.alert_ref_for(k))
            .collect()
    }

    /// The refs of every firing alert.
    pub fn all_refs(&self) -> Vec<zensight_common::alert::AlertRef> {
        self.external
            .keys()
            .filter_map(|k| self.alert_ref_for(k))
            .collect()
    }

    /// Group currently-firing alerts into unified [`Incident`]s (#129), excluding
    /// silenced sources. The per-key transition history becomes each incident's
    /// timeline. See [`crate::view::incident::group_incidents`].
    ///
    /// [`Incident`]: crate::view::incident::Incident
    pub fn incidents(&self) -> Vec<crate::view::incident::Incident> {
        self.incidents_at(now_ms())
    }

    /// Clock-injected form of [`Self::incidents`] (testable).
    pub fn incidents_at(&self, now_ms: i64) -> Vec<crate::view::incident::Incident> {
        let firing: Vec<&SensorAlert> = self
            .external
            .iter()
            .filter(|(k, _)| !self.is_silenced_alert(k, now_ms))
            .map(|(_, a)| a)
            .collect();
        let mut firing = firing;
        firing.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.timestamp.cmp(&a.timestamp))
        });
        crate::view::incident::group_incidents(
            &firing,
            |k| self.is_external_acked(k),
            |k| {
                self.timeline(k)
                    .into_iter()
                    .map(|t| (t.state, t.at))
                    .collect()
            },
        )
    }

    /// Firing external alerts grouped by source (current time). Silenced sources
    /// are hidden. See [`Self::external_by_source_at`] for a clock-injected form.
    pub fn external_by_source(&self) -> Vec<ExternalIncident<'_>> {
        self.external_by_source_at(now_ms())
    }

    /// Firing external alerts grouped by source, each group sorted by severity
    /// then recency, with its un-acknowledged count and highest severity. Groups
    /// are ordered by (has-unacked, highest-severity, source). Sources silenced
    /// at `now_ms` are excluded (#26). Pure given the clock.
    pub fn external_by_source_at(&self, now_ms: i64) -> Vec<ExternalIncident<'_>> {
        let mut by_source: HashMap<&str, Vec<&SensorAlert>> = HashMap::new();
        for (key, alert) in &self.external {
            if self.is_silenced_alert(key, now_ms) {
                continue;
            }
            if !self.passes_external_filters(alert) {
                continue;
            }
            by_source.entry(&alert.source).or_default().push(alert);
        }
        let mut groups: Vec<ExternalIncident<'_>> = by_source
            .into_iter()
            .map(|(source, mut alerts)| {
                alerts.sort_by(|a, b| {
                    b.severity
                        .cmp(&a.severity)
                        .then(b.timestamp.cmp(&a.timestamp))
                });
                let unacked = alerts
                    .iter()
                    .filter(|a| !self.is_external_acked(&Self::external_key(a)))
                    .count();
                let top_severity = alerts.iter().map(|a| a.severity).max();
                ExternalIncident {
                    source,
                    alerts,
                    unacked,
                    top_severity,
                }
            })
            .collect();
        groups.sort_by(|a, b| {
            (b.unacked > 0)
                .cmp(&(a.unacked > 0))
                .then(b.top_severity.cmp(&a.top_severity))
                .then(a.source.cmp(b.source))
        });
        groups
    }

    /// Whether an external alert passes the active severity + source filters (#27).
    fn passes_external_filters(&self, alert: &SensorAlert) -> bool {
        if let Some(sev) = self.external_severity_filter
            && alert.severity != sev
        {
            return false;
        }
        if let Some(src) = &self.external_source_filter
            && &alert.source != src
        {
            return false;
        }
        if let Some(proto) = self.external_protocol_filter
            && alert.protocol != proto
        {
            return false;
        }
        true
    }

    /// Derive a preset display name from a severity+source combination (#27).
    fn preset_name(
        severity: Option<zensight_common::AlertSeverity>,
        source: Option<&str>,
    ) -> String {
        let sev = severity.map(|s| {
            let n = s.as_str();
            let mut c = n.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        });
        match (sev, source) {
            (Some(s), Some(src)) => format!("{s} · {src}"),
            (Some(s), None) => s,
            (None, Some(src)) => src.to_string(),
            (None, None) => "All".to_string(),
        }
    }

    /// Whether a preset matching the current filter combination already exists.
    pub fn current_filter_is_saved(&self) -> bool {
        self.alert_filter_presets.iter().any(|p| {
            p.severity == self.external_severity_filter && p.source == self.external_source_filter
        })
    }

    /// Save the current external-filter combination as a preset (#27). No-op
    /// (returns `false`) when no filter is active or an identical preset exists.
    pub fn save_current_filter_preset(&mut self) -> bool {
        let severity = self.external_severity_filter;
        let source = self.external_source_filter.clone();
        if severity.is_none() && source.is_none() {
            return false; // nothing to save
        }
        if self.current_filter_is_saved() {
            return false; // already saved
        }
        let name = Self::preset_name(severity, source.as_deref());
        self.alert_filter_presets.push(AlertFilterPreset {
            name,
            severity,
            source,
        });
        true
    }

    /// Apply a saved preset by index, setting both filters (#27).
    pub fn apply_filter_preset(&mut self, index: usize) {
        if let Some(preset) = self.alert_filter_presets.get(index) {
            self.external_severity_filter = preset.severity;
            self.external_source_filter = preset.source.clone();
        }
    }

    /// Delete a saved preset by index (#27).
    pub fn delete_filter_preset(&mut self, index: usize) {
        if index < self.alert_filter_presets.len() {
            self.alert_filter_presets.remove(index);
        }
    }

    /// Distinct sources among currently-firing, non-silenced external alerts,
    /// sorted — drives the source filter pills (#27). Ignores the active source
    /// filter so the pills stay stable as you switch between them.
    pub fn external_sources(&self, now_ms: i64) -> Vec<&str> {
        let mut sources: Vec<&str> = self
            .external
            .iter()
            .filter(|(k, _)| !self.is_silenced_alert(k, now_ms))
            .map(|(_, a)| a.source.as_str())
            .collect();
        sources.sort_unstable();
        sources.dedup();
        sources
    }

    /// Count of *un-acknowledged*, *non-silenced* firing external alerts (badge).
    pub fn external_count(&self) -> usize {
        let now = now_ms();
        self.external
            .keys()
            .filter(|k| !self.is_external_acked(k) && !self.is_silenced_alert(k, now))
            .count()
    }
}

/// Render the alerts view.
///
/// Everything here comes off the bus (#934). The rule form, the rule list and
/// the local alert history are gone with the engine that fed them: they were a
/// second alerting authority that lived in one process's memory, persisted to
/// one laptop, and whose alerts reached nothing — not the bus, not the
/// exporters, not the notifier. Thresholds are authored on the sensor now
/// (#931), through the Expectations view's `thresholds` target (#933).
pub fn alerts_view(state: &AlertsState) -> Element<'_, Message> {
    let header = render_header(state);
    let external_section = render_external_alerts_section(state);

    let content = column![header, rule::horizontal(1), external_section]
        .spacing(15)
        .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render header with back button.
fn render_header(state: &AlertsState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseAlerts)
    .style(iced::widget::button::secondary);

    let title = row![
        icons::alert(IconSize::XLarge),
        text("Alerts & Notifications").size(24)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let unacked = state.unacknowledged_external();
    let unack_badge: Element<'_, Message> = if unacked > 0 {
        row![
            icons::status_warning(IconSize::Small),
            text(format!("{unacked} unacknowledged"))
                .size(14)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).warning()),
                })
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into()
    } else {
        row![].into()
    };

    let expectations_button = button(text("Expectations").size(13))
        .on_press(Message::OpenExpectations)
        .style(iced::widget::button::secondary);

    let security_button = button(text("Security").size(13))
        .on_press(Message::OpenSecurity)
        .style(iced::widget::button::secondary);

    let header_row = row![
        back_button,
        title,
        unack_badge,
        expectations_button,
        security_button
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    // Scope subtitle so Alerts vs Security is legible (#39): this view owns
    // operational, threshold-based alerts; Security owns network anomalies.
    let subtitle = text(
        "Operational alerts, as the sensors publish them — thresholds are authored on the sensor",
    )
    .size(font::CAPTION)
    .style(|theme: &Theme| text::Style {
        color: Some(crate::view::theme::colors(theme).text_dimmed()),
    });

    column![header_row, subtitle].spacing(4).into()
}

/// Render the alerts section.
/// Render the sensor-pushed alerts section (anomalies + expectation violations).
fn render_external_alerts_section(state: &AlertsState) -> Element<'_, Message> {
    let groups = state.external_by_source();
    let total: usize = groups.iter().map(|g| g.alerts.len()).sum();

    let actions: Option<Element<'_, Message>> = if state.external_count() > 0 {
        Some(
            button(text("Ack all").size(font::CAPTION))
                .on_press(Message::AcknowledgeAllExternal)
                .padding([space::XS, space::SM])
                .style(iced::widget::button::secondary)
                .into(),
        )
    } else {
        None
    };
    let now = now_ms();
    let muted_sources = state.silenced_sources_at(now);
    let muted = muted_sources.len();
    let title = if muted > 0 {
        format!("Anomalies & Expectations ({total}) · {muted} muted")
    } else {
        format!("Anomalies & Expectations ({total})")
    };
    // A mute must be liftable from where it is shown. "Mute 24h" used to be
    // undoable only by waiting: the count was text, and nothing emitted
    // `UnsilenceSource`.
    let actions: Option<Element<'_, Message>> = if muted_sources.is_empty() {
        actions
    } else {
        let mut row = row![].spacing(space::XS).align_y(Alignment::Center);
        for source in muted_sources {
            row = row.push(
                button(text(format!("Unmute {source}")).size(font::CAPTION))
                    .on_press(Message::UnsilenceSource(source.clone()))
                    .padding([space::XS, space::SM])
                    .style(iced::widget::button::secondary),
            );
        }
        if let Some(a) = actions {
            row = row.push(a);
        }
        Some(row.into())
    };
    let section_title = section_header(title, actions);

    let sources = state.external_sources(now_ms());
    let filtering =
        state.external_severity_filter.is_some() || state.external_source_filter.is_some();

    // Nothing firing and no filter to clear: the plain empty state.
    if sources.is_empty() && !filtering {
        return column![section_title, empty_state("No active sensor alerts", None)]
            .spacing(space::SM)
            .into();
    }

    let pills = render_alert_filter_pills(state, &sources);

    // The alert an event record linked to has since resolved (#651). Say so,
    // with its firing→resolved strip, instead of dropping the operator on an
    // empty list — a link that silently lands nowhere is worse than the
    // device-scoped pivot it replaced. `clear_external` removes the alert from
    // `external` but keeps its timeline, which is what makes this possible.
    let resolved_focus: Option<Element<'_, Message>> = state
        .focused_external
        .as_ref()
        .filter(|k| !state.external.contains_key(*k))
        .map(|key| {
            let mut col = Column::new().spacing(space::XS).push(
                row![
                    text("The linked alert is no longer firing.").size(font::CAPTION),
                    container(text("")).width(Length::Fill),
                    button(text("Clear").size(font::CAPTION))
                        .on_press(Message::ClearAlertFocus)
                        .padding([space::XS, space::SM])
                        .style(iced::widget::button::text),
                ]
                .align_y(Alignment::Center),
            );
            let tl = state.timeline(key);
            if tl.len() > 1 {
                col = col.push(render_timeline(&tl));
            }
            container(col)
                .padding(space::SM)
                .style(container::rounded_box)
                .into()
        });

    let body: Element<'_, Message> = if groups.is_empty() {
        // A filter is active and hides everything — keep the pills visible so it
        // can be cleared.
        empty_state("No alerts match the current filter", None)
    } else {
        let mut list = Column::new().spacing(space::SM);
        for group in &groups {
            list = list.push(render_incident(state, group));
        }
        list.into()
    };

    let mut out = Column::new()
        .spacing(space::SM)
        .push(section_title)
        .push(pills);
    if let Some(banner) = resolved_focus {
        out = out.push(banner);
    }
    out.push(body).into()
}

/// Severity + source filter pills for the external-alerts feed (#27). Each row is
/// a single-select set where the active pill uses the primary button style.
fn render_alert_filter_pills<'a>(
    state: &'a AlertsState,
    sources: &[&'a str],
) -> Element<'a, Message> {
    use zensight_common::AlertSeverity;

    let pill = |label: String, selected: bool, msg: Message| -> Element<'a, Message> {
        button(text(label).size(font::CAPTION))
            .on_press(msg)
            .padding([space::XS, space::SM])
            .style(if selected {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            })
            .into()
    };

    // Severity row: All · Critical · Warning · Info.
    let sev = state.external_severity_filter;
    let mut sev_row = row![
        text("Severity").size(font::CAPTION).style(|theme: &Theme| {
            text::Style {
                color: Some(crate::view::theme::colors(theme).text_dimmed()),
            }
        }),
        pill(
            "All".into(),
            sev.is_none(),
            Message::SetAlertSeverityFilter(None)
        ),
    ]
    .spacing(space::XS)
    .align_y(Alignment::Center);
    for s in [
        AlertSeverity::Critical,
        AlertSeverity::Warning,
        AlertSeverity::Info,
    ] {
        let label = {
            let n = s.as_str();
            let mut c = n.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        };
        sev_row = sev_row.push(pill(
            label,
            sev == Some(s),
            Message::SetAlertSeverityFilter(Some(s)),
        ));
    }

    let mut col = Column::new().spacing(space::XS).push(sev_row);

    // Protocol row (#582): shown when more than one protocol is firing, or a
    // protocol filter is active (an overview tile's click-through must stay
    // visible and clearable even when only its own protocol fires).
    let mut protocols: Vec<zensight_common::Protocol> =
        state.external.values().map(|a| a.protocol).collect();
    protocols.sort_by_key(std::string::ToString::to_string);
    protocols.dedup();
    let active_proto = state.external_protocol_filter;
    if protocols.len() > 1 || active_proto.is_some() {
        let mut proto_row = row![
            text("Protocol").size(font::CAPTION).style(|theme: &Theme| {
                text::Style {
                    color: Some(crate::view::theme::colors(theme).text_dimmed()),
                }
            }),
            pill(
                "All".into(),
                active_proto.is_none(),
                Message::SetAlertProtocolFilter(None)
            ),
        ]
        .spacing(space::XS)
        .align_y(Alignment::Center);
        for p in protocols {
            proto_row = proto_row.push(pill(
                p.to_string(),
                active_proto == Some(p),
                Message::SetAlertProtocolFilter(Some(p)),
            ));
        }
        col = col.push(proto_row.wrap());
    }

    // Source row only when more than one source is firing (a single source needs
    // no filter). Always offers "All" to reset.
    if sources.len() > 1 {
        let active = state.external_source_filter.as_deref();
        let mut src_row = row![
            text("Source").size(font::CAPTION).style(|theme: &Theme| {
                text::Style {
                    color: Some(crate::view::theme::colors(theme).text_dimmed()),
                }
            }),
            pill(
                "All".into(),
                active.is_none(),
                Message::SetAlertSourceFilter(None)
            ),
        ]
        .spacing(space::XS)
        .align_y(Alignment::Center);
        for &s in sources {
            src_row = src_row.push(pill(
                s.to_string(),
                active == Some(s),
                Message::SetAlertSourceFilter(Some(s.to_string())),
            ));
        }
        // Many sources can overflow the width, so let the source pills wrap.
        col = col.push(src_row.wrap());
    }

    // Presets row (#27): saved severity+source combinations. Each chip applies
    // the preset; the trailing ✕ deletes it. A "Save" affordance appears when a
    // filter is active and not already saved.
    let filtering = sev.is_some() || state.external_source_filter.is_some();
    let can_save = filtering && !state.current_filter_is_saved();
    if !state.alert_filter_presets.is_empty() || can_save {
        let mut presets_row =
            row![
                text("Presets")
                    .size(font::CAPTION)
                    .style(|theme: &Theme| text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    }),
            ]
            .spacing(space::XS)
            .align_y(Alignment::Center);

        for (i, preset) in state.alert_filter_presets.iter().enumerate() {
            let active = state.external_severity_filter == preset.severity
                && state.external_source_filter == preset.source;
            let chip = row![
                button(text(preset.name.clone()).size(font::CAPTION))
                    .on_press(Message::ApplyAlertFilterPreset(i))
                    .padding([space::XS, space::SM])
                    .style(if active {
                        iced::widget::button::primary
                    } else {
                        iced::widget::button::secondary
                    }),
                button(text("✕").size(font::CAPTION))
                    .on_press(Message::DeleteAlertFilterPreset(i))
                    .padding([space::XS, space::XS])
                    .style(iced::widget::button::text),
            ]
            .spacing(2)
            .align_y(Alignment::Center);
            presets_row = presets_row.push(chip);
        }

        if can_save {
            presets_row = presets_row.push(
                button(text("+ Save filter").size(font::CAPTION))
                    .on_press(Message::SaveAlertFilterPreset)
                    .padding([space::XS, space::SM])
                    .style(iced::widget::button::secondary),
            );
        }

        col = col.push(presets_row.wrap());
    }

    col.into()
}

/// Render one source-grouped incident: a header (source · count · severity ·
/// Ack) followed by its alert rows.
fn render_incident<'a>(
    state: &'a AlertsState,
    incident: &ExternalIncident<'a>,
) -> Element<'a, Message> {
    let sev_color = incident
        .top_severity
        .map(|s| Severity::from(s).color())
        .unwrap_or_else(|| Severity::Info.color());

    let count_label = if incident.unacked > 0 {
        format!("{} ({} new)", incident.alerts.len(), incident.unacked)
    } else {
        format!("{} acknowledged", incident.alerts.len())
    };

    let mut header = row![
        badge(sev_color, incident.source.to_string()),
        text(count_label)
            .size(font::CAPTION)
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).text_dimmed()),
            }),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    // Right-aligned action cluster: View + Ack (if unacked) + Mute 1h/4h/24h.
    let spacer = container(text("")).width(Length::Fill);
    header = header.push(spacer);
    // #35: jump to the source device that raised this incident — and, since
    // #934, to the *metric* where the alert names one.
    //
    // A threshold alert carries `labels["metric"]` (#931), so the pivot can be
    // as precise as it was from the local rule engine's rows, which is where
    // that precision used to live and which this release deletes. An
    // expectation or an anomaly names no single metric, and `None` is then the
    // honest answer rather than a guess. `alerts` is severity-sorted, so the
    // metric is the worst one's.
    if let Some(first) = incident.alerts.first() {
        let protocol = first.protocol;
        let source = incident.source.to_string();
        let metric = first.labels.get("metric").cloned();
        header = header.push(
            button(text("View").size(font::CAPTION))
                .on_press(Message::InvestigateAlert {
                    protocol,
                    source,
                    metric,
                })
                .padding([space::XS, space::SM])
                .style(iced::widget::button::secondary),
        );
    }
    // Both actions are catalog WRITES since #925, so both are disabled when
    // the catalog is not there to record them — `on_press` omitted, which is
    // how iced greys a button — with the reason beside them rather than a
    // control that quietly does nothing.
    let can_write = state.can_write();
    if incident.unacked > 0 {
        let mut ack = button(text("Ack").size(font::CAPTION))
            .padding([space::XS, space::SM])
            .style(iced::widget::button::secondary);
        if can_write {
            ack = ack.on_press(Message::AcknowledgeExternalSource(
                incident.source.to_string(),
            ));
        }
        header = header.push(ack);
    }
    for (label, dur) in [
        ("Mute 1h", 3_600_000i64),
        ("4h", 14_400_000),
        ("24h", 86_400_000),
    ] {
        let mut b = button(text(label).size(font::CAPTION))
            .padding([space::XS, space::SM])
            .style(iced::widget::button::text);
        if can_write {
            b = b.on_press(Message::SilenceSource(incident.source.to_string(), dur));
        }
        header = header.push(b);
    }
    if !can_write {
        header = header.push(
            text("catalog offline — cannot acknowledge or silence")
                .size(font::CAPTION)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).text_dimmed()),
                }),
        );
    }

    let mut col = Column::new().spacing(2).push(header);
    for alert in &incident.alerts {
        let key = AlertsState::external_key(alert);
        let acked = state.is_external_acked(&key);
        let focused = state.focused_external.as_deref() == Some(key.as_str());
        col = col.push(render_external_alert_row(alert, acked, focused));
        // WHO acknowledged it, and what they said — the fact a `HashSet`
        // could not carry, and the next operator's first question.
        if let Some(ack) = state.ack_for(&key) {
            let mut line = format!("acknowledged by {}", ack.by);
            if !ack.note.is_empty() {
                line.push_str(&format!(" — {}", ack.note));
            }
            col = col.push(
                text(line)
                    .size(font::CAPTION)
                    .style(|theme: &Theme| text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    }),
            );
        }
        // Incident timeline strip: firing→resolved transitions (#26).
        let tl = state.timeline(&AlertsState::external_key(alert));
        if tl.len() > 1 {
            col = col.push(render_timeline(&tl));
        }
    }
    col.into()
}

/// Render an incident timeline strip: "Firing 10:42 → Resolved 10:45 → ..." (#26).
fn render_timeline<'a>(events: &[TransitionEvent]) -> Element<'a, Message> {
    let parts: Vec<String> = events
        .iter()
        .map(|e| {
            let state = match e.state {
                SensorAlertState::Firing => "Firing",
                SensorAlertState::Resolved => "Resolved",
            };
            format!("{state} {}", format_timestamp(e.at))
        })
        .collect();
    text(format!("  {}", parts.join(" → ")))
        .size(font::CAPTION)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        })
        .into()
}

/// Render a single sensor-pushed alert row (dimmed when acknowledged).
fn render_external_alert_row<'a>(
    alert: &'a SensorAlert,
    acked: bool,
    focused: bool,
) -> Element<'a, Message> {
    let severity: Severity = alert.severity.into();
    let icon: Element<'a, Message> = match severity {
        Severity::Critical => icons::status_error(IconSize::Small),
        Severity::Warning => icons::status_warning(IconSize::Small),
        Severity::Info => icons::info(IconSize::Small),
    };

    // Severity as a color+label badge (#28 L5): never color alone.
    let severity_badge = badge(severity.color(), severity.name());

    // The row an event record linked to (#651), marked so the operator can see
    // which of a device's alerts they were sent to.
    let link_badge: Option<Element<'a, Message>> =
        focused.then(|| badge(crate::view::theme::SEVERITY_INFO, "linked from event"));

    let kind = text(if acked { "ack'd" } else { alert.kind.as_str() })
        .size(10)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let summary: Element<'a, Message> = if alert.summary.len() > MAX_ALERT_MESSAGE_LEN {
        let truncated = format!("{}...", &alert.summary[..MAX_ALERT_MESSAGE_LEN]);
        tooltip(
            text(truncated).size(13),
            container(text(alert.summary.clone()).size(12))
                .padding(8)
                .max_width(400.0)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into()
    } else {
        text(alert.summary.clone()).size(13).into()
    };

    let source = text(format!("{}/{}", alert.protocol, alert.source))
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let time = text(format_timestamp(alert.timestamp))
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let top = Row::new()
        .push(icon)
        .push(severity_badge)
        .push(kind)
        .push(summary)
        .push(source)
        .push(time)
        .spacing(10)
        .align_y(Alignment::Center);

    // Context block (#558): surface the rich labels the sensor stamped (unit,
    // burn ratio, template, coredump details, …) instead of hiding them. Generic
    // — renders whatever known label groups are present, so it degrades cleanly
    // for any protocol's alerts.
    let mut col = Column::new().spacing(4);
    if let Some(b) = link_badge {
        col = col.push(b);
    }
    col = col.push(top);
    if let Some(detail) = alert_detail_line(&alert.labels) {
        col = col.push(detail);
    }
    // Pivot to the offending log lines for log-sourced alerts (#558).
    if alert.protocol == Protocol::Logs {
        let unit = alert.labels.get("unit").cloned();
        // Novelty carries the masked template; fall back to the sample line.
        let pattern = alert
            .labels
            .get("template")
            .or_else(|| alert.labels.get("sample"))
            .cloned();
        if unit.is_some() || pattern.is_some() {
            col = col.push(
                button(text("view logs →").size(11))
                    .on_press(Message::PivotToLogsFromAlert {
                        rule: alert.rule.clone(),
                        unit,
                        pattern,
                        severity_min: None,
                        at_ms: Some(alert.timestamp),
                    })
                    .padding([space::XS, space::SM])
                    .style(iced::widget::button::text),
            );
        }
    }
    col.into()
}

/// Compact, generic detail line from an alert's labels (#558): renders known
/// label groups as `key: value` chips, in a stable order, skipping absent ones.
/// `None` when no known label is present. Not logs-specific — any protocol's
/// alert that stamps these keys gets the block.
fn alert_detail_line<'a>(labels: &HashMap<String, String>) -> Option<Element<'a, Message>> {
    let pairs = alert_detail_pairs(labels);
    if pairs.is_empty() {
        return None;
    }
    let chips: Vec<Element<'a, Message>> = pairs
        .into_iter()
        .map(|(disp, v)| {
            text(format!("{disp}: {v}"))
                .size(10)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).text_muted()),
                })
                .into()
        })
        .collect();
    Some(Row::with_children(chips).spacing(12).into())
}

/// The high-signal context labels an alert carries, as ordered
/// `(display-label, value)` pairs (#558). Pure — the render + tests share it.
/// Absent labels are skipped; long values are truncated.
fn alert_detail_pairs(labels: &HashMap<String, String>) -> Vec<(&'static str, String)> {
    // Curated, ordered (key, display) — the context the sensors stamp.
    const KNOWN: &[(&str, &str)] = &[
        ("unit", "unit"),
        ("app", "app"),
        ("error_ratio", "err ratio"),
        ("target_ratio", "target"),
        ("burn_rate", "burn"),
        ("template", "template"),
        ("template_id", "template id"),
        ("message_id", "message id"),
        ("event", "event"),
        ("coredump_exe", "exe"),
        ("coredump_signal", "signal"),
        ("count", "count"),
    ];
    KNOWN
        .iter()
        .filter_map(|(key, disp)| {
            let v = labels.get(*key).filter(|v| !v.is_empty())?;
            let v = if v.chars().count() > 60 {
                format!("{}…", v.chars().take(60).collect::<String>())
            } else {
                v.clone()
            };
            Some((*disp, v))
        })
        .collect()
}

/// Maximum length for alert message before truncation.
const MAX_ALERT_MESSAGE_LEN: usize = 60;

#[cfg(test)]
mod tests {
    use super::*;

    /// #558: the alert context block surfaces known label groups in a stable
    /// order and is empty for an alert with no known labels.
    #[test]
    fn alert_detail_pairs_surfaces_known_labels() {
        let mut labels = HashMap::new();
        labels.insert("unit".to_string(), "nginx.service".to_string());
        labels.insert("error_ratio".to_string(), "0.42".to_string());
        labels.insert("irrelevant".to_string(), "x".to_string());
        let pairs = alert_detail_pairs(&labels);
        // unit before err ratio (curated order), irrelevant dropped.
        assert_eq!(
            pairs,
            vec![
                ("unit", "nginx.service".to_string()),
                ("err ratio", "0.42".to_string()),
            ]
        );
        // No known labels → empty (block hidden; degrades for other protocols).
        let mut other = HashMap::new();
        other.insert("some_other_key".to_string(), "y".to_string());
        assert!(alert_detail_pairs(&other).is_empty());
    }

    #[test]
    fn test_comparison_operators() {
        assert_eq!(ComparisonOp::GreaterThan.symbol(), ">");
        assert_eq!(ComparisonOp::LessOrEqual.symbol(), "<=");
        assert_eq!(ComparisonOp::NotEqual.symbol(), "!=");
    }

    fn ext_alert(rule: &str, sev: zensight_common::AlertSeverity) -> SensorAlert {
        SensorAlert::new(
            "host1",
            Protocol::Netlink,
            zensight_common::AlertKind::Expectation,
            rule,
            sev,
            "summary",
        )
    }

    #[test]
    fn ingest_external_lifecycle() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let a = ext_alert("ssh-listening", AlertSeverity::Critical);
        let key = AlertsState::external_key(&a);

        assert_eq!(state.ingest_external(a.clone()), ExternalAlertOutcome::New);
        assert_eq!(state.external_count(), 1);
        // Same key again → Updated, no duplicate.
        assert_eq!(
            state.ingest_external(a.clone()),
            ExternalAlertOutcome::Updated
        );
        assert_eq!(state.external_count(), 1);
        // Resolve removes it.
        assert_eq!(
            state.ingest_external(a.resolved()),
            ExternalAlertOutcome::Resolved
        );
        assert_eq!(state.external_count(), 0);
        // Resolve again → Unknown.
        let b = ext_alert("ssh-listening", AlertSeverity::Critical).resolved();
        assert_eq!(state.ingest_external(b), ExternalAlertOutcome::Unknown);
        // clear_external by key is a no-op now.
        assert!(state.clear_external(&key).is_none());
    }

    /// A Delete tombstone carries the origin chunk and the hash, never the
    /// `source` the in-GUI key is scoped by. It must still find its entry —
    /// and must not clear another host's alert that happens to share the
    /// hash (same rule, same labels).
    #[test]
    fn a_tombstone_finds_its_entry_by_origin_and_hash() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let a = ext_alert("ssh-listening", AlertSeverity::Critical);
        let hash = a.alert_key();
        assert_eq!(
            state.ingest_external_from(Some("h-aaaaaaaaaaaa".into()), a.clone()),
            ExternalAlertOutcome::New
        );
        // Another host, same rule and labels: same hash, different key.
        let mut b = a.clone();
        b.source = "host2".into();
        assert_eq!(b.alert_key(), hash);
        assert_eq!(
            state.ingest_external_from(Some("h-bbbbbbbbbbbb".into()), b),
            ExternalAlertOutcome::New
        );
        assert_eq!(state.external_count(), 2);

        // The bare hash matches nothing directly (the old, dead lookup).
        assert!(state.clear_external(&hash).is_none());
        // A tombstone from an origin nobody recorded clears nothing.
        assert!(state.clear_external_from("h-cccccccccccc", &hash).is_none());
        assert_eq!(state.external_count(), 2);
        // The right origin clears exactly its own entry.
        let cleared = state
            .clear_external_from("h-bbbbbbbbbbbb", &hash)
            .expect("host2's");
        assert_eq!(cleared.source, "host2");
        assert_eq!(state.external_count(), 1);
        // With one candidate left and its origin recorded, only that origin
        // may clear it.
        assert!(state.clear_external_from("h-bbbbbbbbbbbb", &hash).is_none());
        assert!(state.clear_external_from("h-aaaaaaaaaaaa", &hash).is_some());
        assert_eq!(state.external_count(), 0);

        // An entry ingested without an origin (demo feed) is cleared by a
        // unique hash alone.
        state.ingest_external(ext_alert("ssh-listening", AlertSeverity::Critical));
        assert!(state.clear_external_from("h-dddddddddddd", &hash).is_some());
    }

    /// Ingest a firing alert with a known origin and hand back its in-GUI key
    /// and its wire ref — the pair every ack test needs (#925).
    fn ingest_with_origin(
        state: &mut AlertsState,
        origin: &str,
        alert: SensorAlert,
    ) -> (String, zensight_common::alert::AlertRef) {
        let key = AlertsState::external_key(&alert);
        state.ingest_external_from(Some(origin.to_string()), alert);
        let r = state
            .alert_ref_for(&key)
            .expect("a known origin yields a ref");
        (key, r)
    }

    fn ack_doc(
        r: &zensight_common::alert::AlertRef,
        fired_at: i64,
    ) -> zensight_common::ack::AlertAck {
        zensight_common::ack::AlertAck {
            alert_ref: r.clone(),
            fired_at,
            by: "marc".into(),
            note: String::new(),
            at: fired_at,
        }
    }

    /// **The projection rule, in the GUI** (#925). An ack is of one
    /// occurrence: a re-fire must arrive un-acknowledged, and this now falls
    /// out of `fired_at` rather than out of a `HashSet` the ingest path had to
    /// remember to prune.
    #[test]
    fn an_ack_does_not_outlive_its_firing() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let mut a = ext_alert("ssh-listening", AlertSeverity::Critical);
        a.timestamp = 1_000;
        let (key, r) = ingest_with_origin(&mut state, "h-aaaaaaaaaaaa", a.clone());
        state.ingest_ack(ack_doc(&r, 1_000));
        assert!(state.is_external_acked(&key));

        // Cleared, then fired again — a LATER occurrence.
        state.ingest_external(a.clone().resolved());
        let mut again = a;
        again.timestamp = 2_000;
        state.ingest_external_from(Some("h-aaaaaaaaaaaa".into()), again);
        assert!(
            !state.is_external_acked(&key),
            "a re-fired alert is a new incident, not a pre-acked one"
        );
    }

    /// **An orphan is inert.** An ack whose alert is not firing — a catalog
    /// died holding it — must read as nothing, never as a suppression.
    #[test]
    fn an_ack_with_no_firing_alert_applies_to_nothing() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let mut a = ext_alert("ssh-listening", AlertSeverity::Critical);
        a.timestamp = 1_000;
        let (key, r) = ingest_with_origin(&mut state, "h-aaaaaaaaaaaa", a.clone());
        state.ingest_ack(ack_doc(&r, 1_000));
        state.ingest_external(a.resolved());
        assert!(!state.is_external_acked(&key));
        assert_eq!(state.external_count(), 0);
    }

    /// An alert whose origin the GUI never saw has no ref, so the GUI declines
    /// to acknowledge it rather than addressing an ack to a guessed host.
    #[test]
    fn an_alert_without_a_known_origin_has_no_ref() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let a = ext_alert("ssh-listening", AlertSeverity::Critical);
        let key = AlertsState::external_key(&a);
        state.ingest_external(a); // no origin
        assert!(state.alert_ref_for(&key).is_none());
        assert!(state.refs_for_source("host1").is_empty());
    }

    fn source_silence(
        id: &str,
        source: &str,
        starts: i64,
        ends: i64,
    ) -> zensight_common::silence::Silence {
        zensight_common::silence::Silence {
            id: id.into(),
            matchers: vec![zensight_common::silence::Matcher {
                name: "source".into(),
                op: zensight_common::silence::MatchOp::Eq,
                value: source.into(),
            }],
            starts_at: starts,
            ends_at: ends,
            by: "marc".into(),
            note: String::new(),
        }
    }

    #[test]
    fn silence_hides_source_and_expires() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.ingest_external(ext_alert("ssh", AlertSeverity::Critical));
        assert_eq!(state.external_by_source_at(0).len(), 1);

        state.ingest_silence(source_silence("s1", "host1", 0, 3_600_000));
        assert_eq!(state.silenced_count(1_000), 1);
        assert!(state.external_by_source_at(1_000).is_empty());

        // Past `ends_at` it stops applying **without a tombstone** — a
        // partitioned reader cannot keep an expired suppression alive
        // (RFC 06 §5.5).
        assert_eq!(state.external_by_source_at(3_600_001).len(), 1);
        assert_eq!(state.silenced_count(3_600_001), 0);
    }

    /// **The thing a source list could not do.** A matcher set mutes one
    /// rule across every host, and leaves the rest of those hosts audible.
    #[test]
    fn a_label_matcher_mutes_across_hosts_and_spares_the_rest() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.ingest_external(ext_alert_from(
            "hostA",
            "disk-full",
            AlertSeverity::Critical,
        ));
        state.ingest_external(ext_alert_from(
            "hostB",
            "disk-full",
            AlertSeverity::Critical,
        ));
        state.ingest_external(ext_alert_from("hostA", "ssh-down", AlertSeverity::Critical));
        assert_eq!(state.external_count(), 3);

        state.ingest_silence(zensight_common::silence::Silence {
            id: "s1".into(),
            matchers: vec![zensight_common::silence::Matcher {
                name: "rule".into(),
                op: zensight_common::silence::MatchOp::Eq,
                value: "disk-full".into(),
            }],
            starts_at: 0,
            ends_at: i64::MAX,
            by: "marc".into(),
            note: String::new(),
        });
        assert_eq!(state.silenced_count(1_000), 2, "both disk alerts");
        // hostA is not silenced as a *source*: its ssh alert still shows.
        let groups = state.external_by_source_at(1_000);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].source, "hostA");
        assert_eq!(groups[0].alerts.len(), 1);
    }

    #[test]
    fn unsilence_lifts_immediately() {
        let mut state = AlertsState::new();
        state.ingest_silence(source_silence("s1", "h", 0, 3_600_000));
        assert_eq!(state.silenced_sources_at(10), vec!["h".to_string()]);
        state.retire_silence("s1");
        assert!(state.silenced_sources_at(10).is_empty());
    }

    /// The catalog is the only writer, so with it gone the GUI must not offer
    /// to write.
    #[test]
    fn writes_are_refused_when_the_catalog_is_absent() {
        let mut state = AlertsState::new();
        assert!(!state.can_write(), "unknown is not permission");
        state.catalog_alive = Some(false);
        assert!(!state.can_write());
        state.catalog_alive = Some(true);
        assert!(state.can_write());
    }

    fn ext_alert_from(
        source: &str,
        rule: &str,
        sev: zensight_common::AlertSeverity,
    ) -> SensorAlert {
        SensorAlert::new(
            source,
            Protocol::Netlink,
            zensight_common::AlertKind::Expectation,
            rule,
            sev,
            "summary",
        )
    }

    #[test]
    fn severity_filter_limits_external_feed() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        // Two alerts on one source, distinct rules (so distinct keys).
        state.ingest_external(ext_alert("crit-rule", AlertSeverity::Critical));
        state.ingest_external(ext_alert("warn-rule", AlertSeverity::Warning));

        // No filter: one group, both alerts.
        let groups = state.external_by_source_at(0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].alerts.len(), 2);

        // Critical only.
        state.external_severity_filter = Some(AlertSeverity::Critical);
        let groups = state.external_by_source_at(0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].alerts.len(), 1);
        assert_eq!(groups[0].alerts[0].severity, AlertSeverity::Critical);

        // Info: nothing matches -> no groups.
        state.external_severity_filter = Some(AlertSeverity::Info);
        assert!(state.external_by_source_at(0).is_empty());
    }

    #[test]
    fn source_filter_limits_external_feed() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.ingest_external(ext_alert_from("host1", "r", AlertSeverity::Warning));
        state.ingest_external(ext_alert_from("host2", "r", AlertSeverity::Warning));

        // Sources pill list is distinct + sorted.
        assert_eq!(state.external_sources(0), vec!["host1", "host2"]);
        assert_eq!(state.external_by_source_at(0).len(), 2);

        // Filter to host2.
        state.external_source_filter = Some("host2".to_string());
        let groups = state.external_by_source_at(0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].source, "host2");
        // Pills stay stable regardless of the active source filter.
        assert_eq!(state.external_sources(0), vec!["host1", "host2"]);
    }

    #[test]
    fn save_filter_preset_captures_current_filters() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();

        // Nothing to save when no filter is active.
        assert!(!state.save_current_filter_preset());
        assert!(state.alert_filter_presets.is_empty());

        // Set a severity+source filter and save it.
        state.external_severity_filter = Some(AlertSeverity::Critical);
        state.external_source_filter = Some("host2".to_string());
        assert!(state.save_current_filter_preset());
        assert_eq!(state.alert_filter_presets.len(), 1);
        let preset = &state.alert_filter_presets[0];
        assert_eq!(preset.severity, Some(AlertSeverity::Critical));
        assert_eq!(preset.source.as_deref(), Some("host2"));
        assert_eq!(preset.name, "Critical · host2");

        // Saving the same combination again is a no-op (deduped).
        assert!(state.current_filter_is_saved());
        assert!(!state.save_current_filter_preset());
        assert_eq!(state.alert_filter_presets.len(), 1);
    }

    #[test]
    fn apply_and_delete_filter_preset() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.external_severity_filter = Some(AlertSeverity::Warning);
        state.save_current_filter_preset();
        assert_eq!(state.alert_filter_presets[0].name, "Warning");

        // Clear the live filters, then re-apply the preset.
        state.external_severity_filter = None;
        state.apply_filter_preset(0);
        assert_eq!(state.external_severity_filter, Some(AlertSeverity::Warning));
        assert_eq!(state.external_source_filter, None);

        // Out-of-range indices are ignored, valid ones remove.
        state.apply_filter_preset(99); // no panic, no change
        state.delete_filter_preset(99); // no panic
        assert_eq!(state.alert_filter_presets.len(), 1);
        state.delete_filter_preset(0);
        assert!(state.alert_filter_presets.is_empty());
    }

    #[test]
    fn timeline_records_firing_resolved_transitions() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let mut a = ext_alert("ssh", AlertSeverity::Warning);
        a.timestamp = 1_000;
        let key = AlertsState::external_key(&a);
        state.ingest_external(a.clone());
        // A repeat firing (update) does NOT add a transition.
        let mut a2 = a.clone();
        a2.timestamp = 1_500;
        state.ingest_external(a2);
        // Resolve adds a Resolved transition.
        let mut r = a.resolved();
        r.timestamp = 2_000;
        state.ingest_external(r);
        // Fires again.
        let mut a3 = ext_alert("ssh", AlertSeverity::Warning);
        a3.timestamp = 3_000;
        state.ingest_external(a3);

        let tl = state.timeline(&key);
        assert_eq!(tl.len(), 3);
        assert_eq!(tl[0].state, SensorAlertState::Firing);
        assert_eq!(tl[0].at, 1_000);
        assert_eq!(tl[1].state, SensorAlertState::Resolved);
        assert_eq!(tl[1].at, 2_000);
        assert_eq!(tl[2].state, SensorAlertState::Firing);
        assert_eq!(tl[2].at, 3_000);
    }

    #[test]
    fn active_external_sorted_by_severity() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.ingest_external(ext_alert("a", AlertSeverity::Info));
        state.ingest_external(ext_alert("b", AlertSeverity::Critical));
        state.ingest_external(ext_alert("c", AlertSeverity::Warning));
        let active = state.active_external();
        assert_eq!(active[0].severity, AlertSeverity::Critical);
        assert_eq!(active[2].severity, AlertSeverity::Info);
    }

    #[test]
    fn external_grouping_and_acknowledge() {
        use zensight_common::{AlertKind, AlertSeverity};
        let mk = |source: &str, rule: &str, sev| {
            SensorAlert::new(
                source,
                Protocol::Netlink,
                AlertKind::Anomaly,
                rule,
                sev,
                "s",
            )
        };
        let mut state = AlertsState::new();
        for (host, rule, sev) in [
            ("hostA", "r1", AlertSeverity::Warning),
            ("hostA", "r2", AlertSeverity::Critical),
            ("hostB", "r3", AlertSeverity::Info),
        ] {
            let origin = if host == "hostA" {
                "h-aaaaaaaaaaaa"
            } else {
                "h-bbbbbbbbbbbb"
            };
            state.ingest_external_from(Some(origin.into()), mk(host, rule, sev));
        }

        // Two source groups; hostA (Critical) sorts first with 2 alerts.
        let groups = state.external_by_source();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].source, "hostA");
        assert_eq!(groups[0].alerts.len(), 2);
        assert_eq!(groups[0].unacked, 2);
        assert_eq!(state.external_count(), 3);

        // Acknowledging hostA drops it from the count and below the un-acked
        // hostB. Since #925 the GUI does not do this itself: it asks the
        // catalog, and renders the documents that come back — which is what
        // `refs_for_source` + `ingest_ack` model here.
        for r in state.refs_for_source("hostA") {
            let fired = state
                .active_external()
                .iter()
                .find(|a| a.source == "hostA")
                .map(|a| a.timestamp)
                .unwrap_or(0);
            state.ingest_ack(ack_doc(&r, fired));
        }
        assert_eq!(state.external_count(), 1);
        let groups = state.external_by_source();
        assert_eq!(groups[0].source, "hostB");
        let host_a = groups.iter().find(|g| g.source == "hostA").unwrap();
        assert_eq!(host_a.unacked, 0);

        for r in state.all_refs() {
            let fired = state
                .active_external()
                .iter()
                .map(|a| a.timestamp)
                .max()
                .unwrap_or(0);
            state.ingest_ack(ack_doc(&r, fired));
        }
        assert_eq!(state.external_count(), 0);
    }
}
