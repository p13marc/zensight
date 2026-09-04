//! Grouping firing alerts into **incidents**, keyed by entity (#922,
//! epic #900).
//!
//! # What changed from the GUI's version
//!
//! `zensight/src/view/incident.rs` has grouped alerts since #129, and it
//! groups by `alert.source` — the payload field, which for a **proxy** sensor
//! is the polled device and for a host sensor is the host (#883). That is one
//! group per *name*, and two different things share a group whenever two hosts
//! reuse a `source`.
//!
//! Here the key is the **entity**: the thing the catalog has already fused out
//! of identity evidence, so a host that publishes under three origins (its own
//! sensors, a hypervisor polling it, a prober checking it) is one incident and
//! not three. An alert whose origin resolves to no entity falls back to
//! `inc-<origin>` — never to `inc-<source>`, because an unresolved origin is a
//! machine we have not fused yet, and a `source` collision between two of them
//! would silently merge two hosts' incidents.
//!
//! # What stayed behind
//!
//! The **timeline** and the **evidence anchors**. The epic is explicit that a
//! timeline is history — the historian's and the GUI's — not LWW state; an
//! `Incident` document that carried every transition would grow without bound
//! on a key with a TTL. Evidence anchors (`metric`, `flow_src`) are pivot
//! targets for one renderer. Both stay in the GUI as a layer on top of this.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::{Alert, AlertRef, AlertSeverity};
use crate::impact::Cause;

/// The `@catalog/state/incident/{incident_id}` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Incident {
    /// `inc-<entity_id>`, or `inc-<origin>` where the origin resolves to no
    /// entity. Also the key chunk.
    pub id: String,
    /// The entity this incident is about, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// Every origin contributing a firing alert.
    ///
    /// Plural because that is the point of keying by entity: a host down can
    /// fire from its own sensors, from the hypervisor polling it, and from a
    /// prober — three origins, one problem.
    pub origins: Vec<String>,
    /// The worst severity across members.
    pub severity: AlertSeverity,
    /// Earliest member `timestamp`.
    pub started: i64,
    /// Latest member `timestamp`.
    pub last_change: i64,
    /// The representative alert's summary — worst severity, then most recent.
    pub summary: String,
    /// Every firing member, by wire ref.
    pub alerts: Vec<AlertRef>,
    /// What this incident is a symptom *of*, when something upstream explains
    /// it (#918's `impact::attribute`, wired in #923).
    ///
    /// Absent on a root: a root is a cause, not a symptom. So an operator's
    /// queue is exactly the incident set minus the ones carrying this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symptom_of: Option<Cause>,
    /// Entities downstream of this one, when it is a root.
    ///
    /// Populated even with no firing alert beneath it: "the hypervisor is down
    /// and these twelve guests are behind it" is worth saying before the
    /// guests notice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impacted: Vec<String>,
    /// How many members are acknowledged (#900's `AlertAck`, projection applied).
    pub acked: usize,
    /// How many members are suppressed by a live silence.
    pub silenced: usize,
    /// When the catalog last rewrote this document.
    pub last_updated: i64,
}

impl Incident {
    /// Members that are neither acknowledged nor silenced — an operator's
    /// actual queue for this incident.
    #[must_use]
    pub fn open(&self) -> usize {
        self.alerts
            .len()
            .saturating_sub(self.acked)
            .saturating_sub(self.silenced)
    }
}

/// Group firing alerts into incidents.
///
/// Pure: the caller supplies the firing set with each alert's wire ref, a
/// resolver from origin to entity, and the two suppression predicates. That is
/// what keeps this testable without a catalog, and what keeps the catalog's
/// entity table out of `zensight-common`.
///
/// `symptom_of` and `impacted` are left empty — they need the relationship
/// graph, which is the incident engine's input (#923), not this function's.
///
/// Output is worst-first: severity, then open members, then most-recent.
pub fn group_incidents(
    firing: &[(AlertRef, &Alert)],
    entity_of: impl Fn(&str) -> Option<String>,
    is_acked: impl Fn(&AlertRef, &Alert) -> bool,
    is_silenced: impl Fn(&AlertRef, &Alert) -> bool,
    now: i64,
) -> Vec<Incident> {
    // Grouped by (id, entity_id) so the fallback and the resolved case cannot
    // collide: `inc-h-3fa9…` and `inc-<entity>` are different strings, and an
    // entity id that happened to equal an origin still lands in its own group
    // because the pair differs.
    /// (incident id, entity id) -> its members.
    type Groups<'a> = BTreeMap<(String, Option<String>), Vec<&'a (AlertRef, &'a Alert)>>;
    let mut groups: Groups<'_> = BTreeMap::new();
    for member in firing {
        let entity = entity_of(&member.0.origin);
        let id = match &entity {
            Some(e) => format!("inc-{e}"),
            None => format!("inc-{}", member.0.origin),
        };
        groups.entry((id, entity)).or_default().push(member);
    }

    let mut out: Vec<Incident> = groups
        .into_iter()
        .map(|((id, entity_id), members)| {
            let severity = members
                .iter()
                .map(|(_, a)| a.severity)
                .max()
                .unwrap_or(AlertSeverity::Info);
            // De-duplicated and ordered: several sensors on one host publish
            // under one origin, and a document that listed it three times
            // would read as three machines.
            let origins: BTreeSet<&str> = members.iter().map(|(r, _)| r.origin.as_str()).collect();
            let started = members.iter().map(|(_, a)| a.timestamp).min().unwrap_or(0);
            let last_change = members.iter().map(|(_, a)| a.timestamp).max().unwrap_or(0);
            // Representative: worst severity, then most recent.
            let top = members
                .iter()
                .max_by(|(_, a), (_, b)| {
                    a.severity
                        .cmp(&b.severity)
                        .then(a.timestamp.cmp(&b.timestamp))
                })
                .expect("a group is non-empty");
            let acked = members.iter().filter(|(r, a)| is_acked(r, a)).count();
            let silenced = members.iter().filter(|(r, a)| is_silenced(r, a)).count();
            // Canonical member order, decided HERE and not left to the caller.
            // `alerts` reaches the incident document's content hash, so an
            // order that follows the caller's iteration is an order that
            // changes across restarts — and a document that changes for a
            // fleet that did not is exactly what the hash gate exists to
            // prevent. "The caller sorted it" is the kind of invariant that
            // gets lost the first time a second caller appears.
            let mut alerts: Vec<AlertRef> = members.iter().map(|(r, _)| r.clone()).collect();
            alerts.sort();

            Incident {
                id,
                entity_id,
                origins: origins.into_iter().map(String::from).collect(),
                severity,
                started,
                last_change,
                summary: top.1.summary.clone(),
                alerts,
                symptom_of: None,
                impacted: Vec::new(),
                acked,
                silenced,
                last_updated: now,
            }
        })
        .collect();

    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.open().cmp(&a.open()))
            .then(b.last_change.cmp(&a.last_change))
            .then(a.id.cmp(&b.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AlertKind, Protocol};

    fn alert(source: &str, rule: &str, sev: AlertSeverity, ts: i64) -> Alert {
        let mut a = Alert::new(
            source,
            Protocol::Netlink,
            AlertKind::Expectation,
            rule,
            sev,
            format!("{rule} on {source}"),
        );
        a.timestamp = ts;
        a
    }

    fn aref(origin: &str, key: &str) -> AlertRef {
        AlertRef::new(origin, "netlink", key)
    }

    fn group(firing: &[(AlertRef, &Alert)], entity: fn(&str) -> Option<String>) -> Vec<Incident> {
        group_incidents(firing, entity, |_, _| false, |_, _| false, 9_000)
    }

    /// **The whole reason this moved.** Two origins that the catalog has fused
    /// into one entity are ONE incident — a host's own sensor and the
    /// hypervisor polling it are the same machine having the same problem.
    /// The GUI's version grouped by `alert.source` and made two.
    #[test]
    fn alerts_from_two_origins_on_one_entity_are_one_incident() {
        let a = alert("web01", "socket:sshd", AlertSeverity::Critical, 1_000);
        let b = alert("vm-101", "guest/down", AlertSeverity::Warning, 1_200);
        let firing = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
        ];
        let out = group(&firing, |_| Some("ent-web01".into()));

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "inc-ent-web01");
        assert_eq!(out[0].entity_id.as_deref(), Some("ent-web01"));
        assert_eq!(out[0].origins, vec!["h-aaaaaaaaaaaa", "h-bbbbbbbbbbbb"]);
        assert_eq!(out[0].alerts.len(), 2);
        // Worst severity wins, and the representative summary comes from it.
        assert_eq!(out[0].severity, AlertSeverity::Critical);
        assert!(out[0].summary.contains("socket:sshd"));
        // The window spans both members.
        assert_eq!(out[0].started, 1_000);
        assert_eq!(out[0].last_change, 1_200);
    }

    /// An origin the catalog has not fused falls back to `inc-<origin>` — not
    /// to `inc-<source>`, which would silently merge two unfused hosts that
    /// happened to share a `source` name.
    #[test]
    fn an_unresolved_origin_falls_back_to_the_origin_not_the_source() {
        let a = alert("web01", "r", AlertSeverity::Warning, 1);
        let b = alert("web01", "r", AlertSeverity::Warning, 2);
        let firing = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
        ];
        let out = group(&firing, |_| None);

        assert_eq!(
            out.len(),
            2,
            "same source, different machines: two incidents"
        );
        let ids: BTreeSet<&str> = out.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains("inc-h-aaaaaaaaaaaa"));
        assert!(ids.contains("inc-h-bbbbbbbbbbbb"));
        assert!(out.iter().all(|i| i.entity_id.is_none()));
    }

    /// Several sensors on one host publish under one origin. The document
    /// lists it once — three entries would read as three machines.
    #[test]
    fn one_origin_is_listed_once_however_many_alerts_it_fires() {
        let a = alert("web01", "a", AlertSeverity::Warning, 1);
        let b = alert("web01", "b", AlertSeverity::Warning, 2);
        let c = alert("web01", "c", AlertSeverity::Warning, 3);
        let firing = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-aaaaaaaaaaaa", "k2"), &b),
            (aref("h-aaaaaaaaaaaa", "k3"), &c),
        ];
        let out = group(&firing, |_| None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].origins, vec!["h-aaaaaaaaaaaa"]);
        assert_eq!(out[0].alerts.len(), 3);
    }

    /// `acked` and `silenced` count members, and `open()` is what is left —
    /// which is what an operator's queue actually is.
    #[test]
    fn acked_and_silenced_members_leave_the_queue() {
        let a = alert("web01", "a", AlertSeverity::Warning, 1);
        let b = alert("web01", "b", AlertSeverity::Warning, 2);
        let c = alert("web01", "c", AlertSeverity::Warning, 3);
        let firing = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-aaaaaaaaaaaa", "k2"), &b),
            (aref("h-aaaaaaaaaaaa", "k3"), &c),
        ];
        let out = group_incidents(
            &firing,
            |_| None,
            |r, _| r.alert_key == "k1",
            |r, _| r.alert_key == "k2",
            9_000,
        );
        assert_eq!(out[0].acked, 1);
        assert_eq!(out[0].silenced, 1);
        assert_eq!(out[0].open(), 1);
    }

    /// Worst-first, and stable: two incidents that tie on everything sort by
    /// id rather than by hash order, so a re-render does not reshuffle a list
    /// an operator is reading.
    #[test]
    fn output_is_worst_first_and_stable() {
        let warn = alert("a", "r", AlertSeverity::Warning, 1);
        let crit = alert("b", "r", AlertSeverity::Critical, 1);
        let info = alert("c", "r", AlertSeverity::Info, 1);
        let firing = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &warn),
            (aref("h-bbbbbbbbbbbb", "k2"), &crit),
            (aref("h-cccccccccccc", "k3"), &info),
        ];
        let out = group(&firing, |_| None);
        assert_eq!(
            out.iter().map(|i| i.severity).collect::<Vec<_>>(),
            vec![
                AlertSeverity::Critical,
                AlertSeverity::Warning,
                AlertSeverity::Info
            ]
        );

        let x = alert("x", "r", AlertSeverity::Warning, 5);
        let y = alert("y", "r", AlertSeverity::Warning, 5);
        let tied = vec![
            (aref("h-bbbbbbbbbbbb", "k1"), &y),
            (aref("h-aaaaaaaaaaaa", "k2"), &x),
        ];
        let out = group(&tied, |_| None);
        assert_eq!(out[0].id, "inc-h-aaaaaaaaaaaa");
    }

    /// The member list is canonical whatever order the caller passed.
    ///
    /// It reaches the incident document's content hash (#923's publish gate),
    /// so an order that followed the caller's iteration would make a restart
    /// look like every incident on the fleet changing at once.
    #[test]
    fn the_member_list_does_not_depend_on_input_order() {
        let a = alert("web01", "a", AlertSeverity::Warning, 1);
        let b = alert("web01", "b", AlertSeverity::Warning, 2);
        let forward = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
        ];
        let backward = vec![
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
        ];
        let one = group(&forward, |_| Some("ent".into()));
        let two = group(&backward, |_| Some("ent".into()));
        assert_eq!(one, two);
    }

    #[test]
    fn no_firing_alerts_is_no_incidents() {
        assert!(group(&[], |_| None).is_empty());
    }
}
