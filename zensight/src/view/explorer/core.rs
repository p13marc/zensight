//! The bus explorer's pure fold (#748).
//!
//! Session-free and Iced-free by design: [`ExplorerCore`] consumes
//! `zenkey_fleet::StreamItem`s and answers with an [`ExplorerSnapshot`] — the
//! same shape whether the items came from a live [`zenkey_fleet::Monitor`],
//! the demo generator, or a `.zrec` replay through
//! `crate::replay::sample_view` + `MonitorCore::ingest_at` (#747). That is
//! what makes the view's logic deterministically testable: nothing in this
//! module can tell live traffic from a fixture.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use zenkey_fleet::{
    FleetEvent, KeyTreeSnapshot, MonitorCore, RetentionStats, SampleView, StreamItem, WatchId,
};
use zensight_common::keyexpr::{correlator_alive_key, refine_key};

use super::inspector::InspectedSample;

/// Key-tree bound for the explorer (not upstream's 50k default): this GUI
/// runs beside the fleet it watches, and the eviction ledger is what makes
/// the smaller bound honest — a fleet larger than this is reported as
/// truncated, never silently clipped.
pub const EXPLORER_MAX_KEYS: usize = 10_000;

/// Broadcast capacity between the monitor's network callback and the pump.
/// What the bound sheds arrives as an explicit `StreamItem::Dropped(n)`.
pub const EXPLORER_CAPACITY: usize = 1024;

/// Retention budget: 16 MiB / 60 s — smaller than upstream's 64 MiB / 2 min
/// default, because this observer shares a host with the GUI. The banner
/// states it (the budget is part of the view, not a hidden constant).
pub const RETAIN_BYTES: usize = 16 * 1024 * 1024;
pub const RETAIN_SECS: u64 = 60;

/// Bound on the QoS-mismatch and unregistered-key ledgers. What the bound
/// refuses is counted — never silently forgotten.
pub const LEDGER_KEYS: usize = 256;

/// One key's observed-vs-declared QoS disagreement (all four axes compared —
/// `SampleView::qos_matches`). This is `QosObservedMismatch`, live, in the
/// UI instead of a doctor report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QosMismatch {
    /// The registry's declared profile name for this subject.
    pub declared: &'static str,
    /// The wire's actual axes, as an axes token (`priority/cc/reliability`).
    pub observed: String,
    /// Samples seen with these mismatching axes.
    pub count: u64,
}

/// The bounded QoS/registration ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QosLedger {
    /// Mismatching keys only — a clean fleet keeps this empty.
    pub mismatches: BTreeMap<String, QosMismatch>,
    /// Mismatching keys the [`LEDGER_KEYS`] bound refused to track.
    pub refused: u64,
    /// Keys the registry does not know. Distinct from a mismatch: there is
    /// no declared profile to disagree with, and rendering it as one would
    /// invent a verdict (RFC 09 §5.1 O4).
    pub unregistered: BTreeSet<String>,
    /// Unregistered keys the bound refused to track.
    pub unregistered_refused: u64,
}

/// A liveliness participant: `(origin chunk, producer name)`.
pub type Presence = BTreeMap<(String, String), bool>;

/// What one stats tick hands the GUI. `Arc`-carried and cheap to clone; the
/// four loss ledgers are **distinct facts** and are never summed (RFC 13):
/// `tree.evicted` (key bound), `tree.unwatched` (released watches),
/// `stream_dropped` (broadcast shed), `retention.evicted`/`.expired` (byte /
/// age budget).
#[derive(Debug, Clone)]
pub struct ExplorerSnapshot {
    pub tree: Arc<KeyTreeSnapshot>,
    /// Active watches, sorted by id.
    pub watches: Vec<(WatchId, String)>,
    pub presence: Presence,
    pub qos: QosLedger,
    /// Cumulative broadcast-lag drops (`MonitorCore::dropped`).
    pub stream_dropped: u64,
    pub retention: RetentionStats,
    /// The selected key's latest retained sample, when one is selected and
    /// retained. Retention covers watched keys only — the pane says so.
    pub inspected: Option<InspectedSample>,
}

/// The per-sample fold the pump runs off the GUI thread. Holds only what a
/// `KeyTreeSnapshot` does not already carry: presence and the QoS ledger.
#[derive(Debug, Default)]
pub struct ExplorerCore {
    presence: Presence,
    qos: QosLedger,
    /// Declared profile per key, memoized — `refine_key` is a parse and a
    /// registry walk, and a hot key repeats thousands of times.
    declared: HashMap<String, Option<(zenkey::qos::QosProfile, &'static str)>>,
}

/// The registry's declared QoS profile and payload type for a subject.
///
/// `AnySubject` deliberately has no delegating accessor for these — the
/// exhaustive match here is the compile error that reminds whoever adds a
/// producer that the explorer judges its QoS too.
pub fn declared_of(
    subject: &zensight_common::registry::AnySubject,
) -> (zenkey::qos::QosProfile, &'static str) {
    use zensight_common::registry::AnySubject as S;
    match subject {
        S::Catalog(s) => (s.qos(), s.payload_type()),
        S::Gnmi(s) => (s.qos(), s.payload_type()),
        S::Hostspec(s) => (s.qos(), s.payload_type()),
        S::Desired(s) => (s.qos(), s.payload_type()),
        S::Logs(s) => (s.qos(), s.payload_type()),
        S::Modbus(s) => (s.qos(), s.payload_type()),
        S::Netflow(s) => (s.qos(), s.payload_type()),
        S::Netlink(s) => (s.qos(), s.payload_type()),
        S::Netring(s) => (s.qos(), s.payload_type()),
        S::Parallax(s) => (s.qos(), s.payload_type()),
        S::Container(s) => (s.qos(), s.payload_type()),
        S::Pve(s) => (s.qos(), s.payload_type()),
        S::Snmp(s) => (s.qos(), s.payload_type()),
        S::Sysinfo(s) => (s.qos(), s.payload_type()),
        S::Systemd(s) => (s.qos(), s.payload_type()),
    }
}

/// The registry's declared QoS profile for a key, un-memoized (the
/// inspector's once-per-tick lookup; the hot path uses the memo above).
pub fn declared_profile(key: &str) -> Option<zenkey::qos::QosProfile> {
    refine_key(key).map(|(_, _, subject)| declared_of(&subject).0)
}

/// The wire's axes as the report dialect's token.
pub fn axes_token(view: &SampleView) -> String {
    zenkey_fleet::report::qos_axes_token(
        view.priority,
        view.congestion_control,
        view.reliability,
        view.express,
    )
}

/// Parse a liveliness token key into a presence entry.
///
/// Two shapes exist because two sweeps exist (grammar D4 — `*` in the origin
/// position can never match the verbatim `@catalog`): the fleet's
/// `v1/<origin>/state/<producer>/alive` and the catalog's own
/// `v1/@catalog/state/alive`, which carries no producer chunk.
fn presence_entry(key: &str) -> Option<(String, String)> {
    if key == correlator_alive_key() {
        return Some(("@catalog".to_string(), "correlator".to_string()));
    }
    let parsed = zensight_common::keyexpr::parse_key(key)?;
    if parsed.subject.as_slice() != ["alive"] {
        return None;
    }
    Some((
        parsed.origin.chunk().to_string(),
        parsed.producer()?.name().to_string(),
    ))
}

impl ExplorerCore {
    /// Fold one stream item. Cheap by construction — this runs per sample.
    pub fn apply(&mut self, item: &StreamItem) {
        match item {
            StreamItem::Event(FleetEvent::Sample(view)) => self.fold_sample(view),
            StreamItem::Event(FleetEvent::NodeUp(key)) => {
                if let Some(entry) = presence_entry(key) {
                    self.presence.insert(entry, true);
                }
            }
            StreamItem::Event(FleetEvent::NodeDown(key)) => {
                if let Some(entry) = presence_entry(key) {
                    self.presence.insert(entry, false);
                }
            }
            // Broadcast lag is already folded into `MonitorCore::dropped()`
            // by the stream itself; ticks and watch changes carry no state
            // this fold keeps.
            StreamItem::Event(
                FleetEvent::StatsTick | FleetEvent::WatchChanged | FleetEvent::WatchSeeded { .. },
            )
            | StreamItem::Dropped(_) => {}
        }
    }

    fn fold_sample(&mut self, view: &SampleView) {
        let declared = match self.declared.get(view.key.as_str()) {
            Some(d) => *d,
            None => {
                let d = refine_key(&view.key).map(|(_, _, subject)| {
                    let (qos, ty) = declared_of(&subject);
                    (qos, ty)
                });
                // The memo shares the ledger bound: an unbounded memo over an
                // unbounded keyspace would be the leak the ledgers exist to
                // prevent. Past the bound we re-derive per sample (correct,
                // just slower) rather than grow.
                if self.declared.len() < EXPLORER_MAX_KEYS {
                    self.declared.insert(view.key.clone(), d);
                }
                d
            }
        };
        match declared {
            None => {
                if self.qos.unregistered.contains(view.key.as_str()) {
                } else if self.qos.unregistered.len() < LEDGER_KEYS {
                    self.qos.unregistered.insert(view.key.clone());
                } else {
                    self.qos.unregistered_refused += 1;
                }
            }
            Some((profile, _)) => {
                if !view.qos_matches(profile) {
                    if let Some(m) = self.qos.mismatches.get_mut(view.key.as_str()) {
                        m.count += 1;
                        m.observed = axes_token(view);
                    } else if self.qos.mismatches.len() < LEDGER_KEYS {
                        self.qos.mismatches.insert(
                            view.key.clone(),
                            QosMismatch {
                                declared: profile.name(),
                                observed: axes_token(view),
                                count: 1,
                            },
                        );
                    } else {
                        self.qos.refused += 1;
                    }
                }
            }
        }
    }

    /// The declared payload type for a key, from the same memo the QoS fold
    /// keeps (the inspector's "declared type" line).
    pub fn declared_type(&mut self, key: &str) -> Option<&'static str> {
        if let Some(d) = self.declared.get(key) {
            return d.map(|(_, ty)| ty);
        }
        refine_key(key).map(|(_, _, subject)| declared_of(&subject).1)
    }

    /// Assemble the tick snapshot from this fold plus the monitor core's own
    /// ledgers. `watches` and the inspected sample are the pump's to supply —
    /// they live behind async calls this pure fold must not make.
    pub fn snapshot(
        &self,
        core: &MonitorCore,
        watches: Vec<(WatchId, String)>,
        inspected: Option<InspectedSample>,
    ) -> ExplorerSnapshot {
        ExplorerSnapshot {
            tree: core.tree(),
            watches,
            presence: self.presence.clone(),
            qos: self.qos.clone(),
            stream_dropped: core.dropped(),
            retention: core.retention(),
            inspected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use zenkey_fleet::IngestRow;

    fn view(key: &str, qos: Option<&str>) -> Arc<SampleView> {
        let row = IngestRow {
            key: key.to_string(),
            payload: b"{}".to_vec(),
            encoding: Some("application/json".into()),
            qos: qos.map(String::from),
            delete: false,
            attachment: None,
        };
        Arc::new(crate::replay::sample_view(&row, Instant::now()))
    }

    fn sample(key: &str, qos: Option<&str>) -> StreamItem {
        StreamItem::Event(FleetEvent::Sample(view(key, qos)))
    }

    /// Both liveliness sweeps fold into presence — and the catalog's token,
    /// which no `*`-origin selector can match (D4), is named explicitly so
    /// "catalog dead" and "no entities" render differently.
    #[test]
    fn presence_folds_both_sweeps() {
        let mut core = ExplorerCore::default();
        core.apply(&StreamItem::Event(FleetEvent::NodeUp(
            "v1/h-3fa9c2d41b7e/state/sysinfo/alive".into(),
        )));
        core.apply(&StreamItem::Event(FleetEvent::NodeUp(
            correlator_alive_key(),
        )));
        core.apply(&StreamItem::Event(FleetEvent::NodeDown(
            "v1/h-3fa9c2d41b7e/state/sysinfo/alive".into(),
        )));
        assert_eq!(
            core.presence
                .get(&("h-3fa9c2d41b7e".into(), "sysinfo".into())),
            Some(&false)
        );
        assert_eq!(
            core.presence.get(&("@catalog".into(), "correlator".into())),
            Some(&true)
        );
    }

    /// A sample whose axes disagree with the registry's declared profile is
    /// recorded; a conforming one is not — the ledger holds problems only.
    #[test]
    fn qos_mismatch_is_recorded_conformance_is_not() {
        let mut core = ExplorerCore::default();
        let key = "v1/h-3fa9c2d41b7e/state/sysinfo/health";
        // health declares `refreshed`; publish it with `alert` axes.
        core.apply(&sample(key, Some("alert")));
        let m = core.qos.mismatches.get(key).expect("mismatch recorded");
        assert_eq!(m.declared, "refreshed");
        assert_eq!(m.count, 1);
        // And conforming axes leave no trace.
        let mut clean = ExplorerCore::default();
        clean.apply(&sample(key, Some("refreshed")));
        assert!(clean.qos.mismatches.is_empty());
    }

    /// An unregistered key is a distinct fact, never a mismatch: there is no
    /// declared profile to disagree with.
    #[test]
    fn unregistered_is_not_a_mismatch() {
        let mut core = ExplorerCore::default();
        let key = "v1/h-3fa9c2d41b7e/telemetry/sysinfo/bogus/nonexistent";
        core.apply(&sample(key, Some("alert")));
        assert!(core.qos.mismatches.is_empty());
        assert!(core.qos.unregistered.contains(key));
    }

    /// The ledger bound refuses with a counter, never silently.
    #[test]
    fn the_ledger_bound_counts_what_it_refuses() {
        let mut core = ExplorerCore::default();
        for i in 0..(LEDGER_KEYS + 5) {
            core.apply(&sample(
                &format!("v1/h-3fa9c2d41b7e/telemetry/sysinfo/unreg/m{i}"),
                None,
            ));
        }
        assert_eq!(core.qos.unregistered.len(), LEDGER_KEYS);
        assert_eq!(core.qos.unregistered_refused, 5);
    }

    /// The four loss ledgers stay four distinct facts in the snapshot — no
    /// field of [`ExplorerSnapshot`] sums them (RFC 13).
    #[test]
    fn snapshot_keeps_the_loss_ledgers_apart() {
        let mcore = MonitorCore::bounded(8, 4);
        let core = ExplorerCore::default();
        let snap = core.snapshot(&mcore, Vec::new(), None);
        // Fresh monitor: every ledger present, every ledger zero — rendered
        // honestly rather than only appearing when bad.
        assert_eq!(snap.stream_dropped, 0);
        assert_eq!(snap.tree.evicted, 0);
        assert_eq!(snap.tree.unwatched, 0);
        assert_eq!(snap.retention.evicted, 0);
    }
}
