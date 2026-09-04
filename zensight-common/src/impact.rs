//! Impact attribution (#918): given the graph, what is a cause and what is
//! merely a symptom.
//!
//! When a hypervisor dies, every guest on it goes down, every container on
//! every guest goes down, and every probe run from any of them starts failing.
//! Without the graph an operator gets forty pages and has to work out from
//! their timestamps which one to act on. With it, one page is the cause and
//! thirty-nine are symptoms of it — and saying which is which is arithmetic,
//! not judgement.
//!
//! # Pure, clock-free, deterministic
//!
//! [`attribute`] takes the edge set, the firing alerts and the set of entities
//! the caller has decided are down, and returns the attribution. It reads no
//! clock, holds no state, and performs no I/O — the same three inputs always
//! give byte-identical output. That is deliberate: this decides what an
//! operator is paged about, and a function that answered differently on
//! Tuesday would be impossible to trust or to test.
//!
//! **"Down" is the caller's decision, not this module's.** Every member origin
//! having lost liveliness, or `HostEntity.status == "offline"`, or an operator
//! marking a host in maintenance are all legitimate definitions and they
//! differ per deployment. Encoding one here would bury a policy choice inside a
//! graph walk.
//!
//! # Only containment propagates
//!
//! Impact flows `from` → `to` along containment edges
//! ([`RelationKind::is_containment`]): a guest cannot outlive its hypervisor,
//! a container cannot outlive its host, a probe result cannot outlive the
//! vantage that measures it. [`RelationKind::L2Adjacent`] is inert — sharing a
//! segment says nothing about dependency, and treating it as containment would
//! flood the segment and blame an arbitrary neighbour.
//!
//! # Bounds
//!
//! Depth is capped at [`MAX_DEPTH`] and every walk carries a visited set. The
//! graph is built from evidence published by independent sensors that have no
//! way to agree there is no cycle: two hosts can each claim to be the other's
//! gateway from a stale table. An unguarded walk would hang the caller, which
//! in the GUI is the render thread.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::alert::Alert;
use crate::relation::{Edge, Endpoint};

/// How far impact propagates from a root.
///
/// Four is the deepest real chain the fleet has — hypervisor → guest →
/// container → probe target — and a cap keeps a pathological edge set from
/// turning attribution into a fleet-wide traversal on every render. Beyond it
/// the relationship is too indirect to be worth paging about anyway.
pub const MAX_DEPTH: usize = 4;

/// Where a firing alert lives.
///
/// Carries the entity as well as the bus coordinates because attribution is
/// *about* entities: `(origin, alert_key)` locates the document, `entity_id`
/// says whose problem it is. Resolving an origin to an entity is the catalog's
/// job and the caller has already done it — asking this function to redo it
/// would mean handing it the entity set too, and it would stop being a
/// function of the graph.
///
/// Renamed from `AlertRef` in #922, which is what this doc comment's first
/// line has always called it. [`crate::alert::AlertRef`] is now the *wire*
/// identifier — one slug-safe key chunk, `Display`/`FromStr` — and the two
/// cannot share a name: a type whose `Display` drops a field (`entity_id`) is
/// a round-trip trap, and `entity_id` has no business in a key chunk anyway.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct AlertSite {
    /// The entity this alert is about.
    pub entity_id: String,
    /// The origin that published it.
    pub origin: String,
    /// Its RFC 11 §3.1 key.
    pub alert_key: String,
}

/// What a symptom is a symptom *of*.
///
/// `JsonSchema` since #922: it rides an `Incident`, which is a state-class
/// `@catalog` document, and the #815 gate wants a real schema for one.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The root entity is down, and has no firing alert of its own to point
    /// at — which is the common case for a host that stopped answering
    /// entirely, since a dead machine publishes nothing.
    Entity { entity_id: String },
    /// The root entity is down *and* is firing an alert, so the operator can
    /// be sent straight to the page that describes it.
    Alert(AlertSite),
}

impl Cause {
    /// The entity at the root, whichever form the cause took.
    pub fn entity_id(&self) -> &str {
        match self {
            Cause::Entity { entity_id } => entity_id,
            Cause::Alert(r) => &r.entity_id,
        }
    }
}

/// The attribution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Impact {
    /// Firing alerts that are explained by something upstream, and by what.
    ///
    /// An alert on a root is **absent** — it is a cause, not a symptom. So an
    /// operator's queue is exactly the firing set minus these keys.
    pub symptoms: BTreeMap<AlertSite, Cause>,
    /// Each root and everything downstream of it, root excluded.
    ///
    /// Present even for a root with no firing alerts anywhere beneath it: "the
    /// hypervisor is down and these twelve guests are behind it" is worth
    /// showing before any of the twelve has had time to alert.
    pub roots: BTreeMap<String, BTreeSet<String>>,
}

impl Impact {
    /// Whether this alert is explained by something else.
    pub fn is_symptom(&self, r: &AlertSite) -> bool {
        self.symptoms.contains_key(r)
    }
}

/// Attribute firing alerts to root causes over the containment graph.
///
/// See the module docs for the guarantees. `down` is the caller's decision;
/// entity ids in it that appear nowhere in `edges` are still roots (an
/// isolated host that went down is its own cause), they simply impact nothing.
pub fn attribute(
    edges: &[Edge],
    firing: &[(AlertSite, &Alert)],
    down: &BTreeSet<String>,
) -> Impact {
    // Containment only, both ends resolved. An `External` end cannot be in
    // `down` — the caller decides downness per entity, and an endpoint with no
    // entity has no liveliness to lose — so an edge touching one can neither
    // carry impact nor root it.
    let mut children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut parents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for e in edges {
        if !e.is_containment() {
            continue;
        }
        let (Endpoint::Entity { entity_id: from }, Endpoint::Entity { entity_id: to }) =
            (&e.from, &e.to)
        else {
            continue;
        };
        // A self-edge is not a dependency and would make every entity its own
        // ancestor, so no down entity could ever be a root.
        if from == to {
            continue;
        }
        children.entry(from).or_default().insert(to);
        parents.entry(to).or_default().insert(from);
    }

    // A root is a down entity with no down containment ancestor. Walking up is
    // bounded the same way as walking down: the cycle guard is what stops two
    // hosts each claiming to be the other's gateway from looping forever.
    let roots: Vec<&str> = down
        .iter()
        .map(String::as_str)
        .filter(|id| !has_down_ancestor(id, &parents, down))
        .collect();

    let mut impact = Impact::default();

    // Nearest root wins, ties broken by root id ascending — so an entity under
    // two dead hypervisors is attributed to one of them, always the same one.
    let mut best: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for root in &roots {
        let reach = descendants(root, &children);
        for (id, depth) in &reach {
            let cand = (*depth, *root);
            best.entry(id)
                .and_modify(|cur| {
                    if cand < *cur {
                        *cur = cand;
                    }
                })
                .or_insert(cand);
        }
        impact.roots.insert(
            (*root).to_string(),
            reach.keys().map(|s| (*s).to_string()).collect(),
        );
    }

    for (r, _) in firing {
        let entity = r.entity_id.as_str();
        // An alert on a root is the cause, never a symptom of itself.
        if roots.contains(&entity) {
            continue;
        }
        let Some((_, root)) = best.get(entity) else {
            continue;
        };
        impact.symptoms.insert(r.clone(), cause_for(root, firing));
    }
    impact
}

/// The most useful thing to point an operator at for a given root.
///
/// If the root is firing its own alert, that alert — it carries a summary, a
/// severity and a rule the operator can act on. Otherwise the entity, which is
/// the honest answer for a machine that died so completely it published
/// nothing.
///
/// **A deviation from #918 worth flagging**: the issue says "when the root has
/// a firing *availability* alert". The alert model has no availability
/// classification — `AlertKind` is `Anomaly`/`Expectation`/`SensorHealth` —
/// and inventing one is a larger design decision than this function should
/// make on its own. The most severe alert on the root is chosen instead, ties
/// broken by `alert_key` ascending so the pick is deterministic. If an
/// availability vocabulary is added later, this is the one place that changes.
fn cause_for(root: &str, firing: &[(AlertSite, &Alert)]) -> Cause {
    firing
        .iter()
        .filter(|(r, _)| r.entity_id == root)
        .max_by(|(ra, a), (rb, b)| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| rb.alert_key.cmp(&ra.alert_key))
        })
        .map(|(r, _)| Cause::Alert(r.clone()))
        .unwrap_or_else(|| Cause::Entity {
            entity_id: root.to_string(),
        })
}

/// Whether any containment ancestor within [`MAX_DEPTH`] is itself down.
fn has_down_ancestor(
    id: &str,
    parents: &BTreeMap<&str, BTreeSet<&str>>,
    down: &BTreeSet<String>,
) -> bool {
    let mut seen: BTreeSet<&str> = BTreeSet::from([id]);
    let mut queue: VecDeque<(&str, usize)> = VecDeque::from([(id, 0usize)]);
    while let Some((cur, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
            continue;
        }
        for p in parents.get(cur).into_iter().flatten() {
            if !seen.insert(p) {
                continue;
            }
            if down.contains(*p) {
                return true;
            }
            queue.push_back((p, depth + 1));
        }
    }
    false
}

/// Every entity reachable downstream, with the shortest depth at which it was
/// reached. Root excluded.
fn descendants<'a>(
    root: &'a str,
    children: &BTreeMap<&'a str, BTreeSet<&'a str>>,
) -> BTreeMap<&'a str, usize> {
    let mut out: BTreeMap<&str, usize> = BTreeMap::new();
    let mut seen: BTreeSet<&str> = BTreeSet::from([root]);
    let mut queue: VecDeque<(&str, usize)> = VecDeque::from([(root, 0usize)]);
    while let Some((cur, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
            continue;
        }
        for c in children.get(cur).into_iter().flatten() {
            if !seen.insert(c) {
                continue;
            }
            out.insert(c, depth + 1);
            queue.push_back((c, depth + 1));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protocol;
    use crate::alert::{AlertKind, AlertSeverity, AlertState};
    use crate::relation::RelationKind;

    fn edge(kind: RelationKind, from: &str, to: &str) -> Edge {
        let f = Endpoint::Entity {
            entity_id: from.into(),
        };
        let t = Endpoint::Entity {
            entity_id: to.into(),
        };
        Edge {
            edge_id: Edge::edge_id(kind, &f, &t),
            kind,
            from: f,
            to: t,
            attrs: Default::default(),
            observers: Vec::new(),
            last_updated: 0,
        }
    }

    fn alert(severity: AlertSeverity) -> Alert {
        Alert {
            timestamp: 0,
            source: "s".into(),
            protocol: Protocol::Sysinfo,
            kind: AlertKind::Expectation,
            rule: "r".into(),
            severity,
            state: AlertState::Firing,
            summary: "x".into(),
            labels: Default::default(),
        }
    }

    fn aref(entity: &str, key: &str) -> AlertSite {
        AlertSite {
            entity_id: entity.into(),
            origin: entity.into(),
            alert_key: key.into(),
        }
    }

    fn down(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_dead_hypervisor_makes_every_guests_alert_a_symptom() {
        let edges = vec![
            edge(RelationKind::Hosts, "hv", "g1"),
            edge(RelationKind::Hosts, "hv", "g2"),
        ];
        let a1 = alert(AlertSeverity::Critical);
        let a2 = alert(AlertSeverity::Warning);
        let firing = vec![(aref("g1", "k1"), &a1), (aref("g2", "k2"), &a2)];
        let imp = attribute(&edges, &firing, &down(&["hv", "g1", "g2"]));

        assert_eq!(imp.roots.keys().collect::<Vec<_>>(), vec!["hv"]);
        assert_eq!(
            imp.roots["hv"],
            BTreeSet::from(["g1".to_string(), "g2".to_string()])
        );
        // Both guests' alerts are symptoms of the hypervisor, which has no
        // alert of its own — a machine that died completely publishes nothing.
        assert_eq!(
            imp.symptoms[&aref("g1", "k1")],
            Cause::Entity {
                entity_id: "hv".into()
            }
        );
        assert!(imp.is_symptom(&aref("g2", "k2")));
    }

    #[test]
    fn the_root_points_at_its_own_alert_when_it_has_one() {
        let edges = vec![edge(RelationKind::Hosts, "hv", "g1")];
        let minor = alert(AlertSeverity::Warning);
        let major = alert(AlertSeverity::Critical);
        let guest = alert(AlertSeverity::Warning);
        let firing = vec![
            (aref("hv", "b-minor"), &minor),
            (aref("hv", "a-major"), &major),
            (aref("g1", "k"), &guest),
        ];
        let imp = attribute(&edges, &firing, &down(&["hv", "g1"]));
        // The most severe alert on the root, not the first one seen.
        assert_eq!(
            imp.symptoms[&aref("g1", "k")],
            Cause::Alert(aref("hv", "a-major"))
        );
        // The root's own alerts are causes, never symptoms.
        assert!(!imp.is_symptom(&aref("hv", "a-major")));
        assert!(!imp.is_symptom(&aref("hv", "b-minor")));
    }

    #[test]
    fn equal_severity_ties_break_on_alert_key_ascending() {
        // The doc comment on `cause_for` promises a deterministic pick; without
        // a test the promise is only a comment, and the failure mode is a cause
        // that changes between renders for no reason an operator can see.
        let edges = vec![edge(RelationKind::Hosts, "hv", "g")];
        let a = alert(AlertSeverity::Warning);
        let guest = alert(AlertSeverity::Warning);
        let forward = vec![
            (aref("hv", "zzz"), &a),
            (aref("hv", "aaa"), &a),
            (aref("g", "k"), &guest),
        ];
        let reversed = vec![
            (aref("hv", "aaa"), &a),
            (aref("hv", "zzz"), &a),
            (aref("g", "k"), &guest),
        ];
        let d = down(&["hv", "g"]);
        let want = Cause::Alert(aref("hv", "aaa"));
        assert_eq!(
            attribute(&edges, &forward, &d).symptoms[&aref("g", "k")],
            want
        );
        assert_eq!(
            attribute(&edges, &reversed, &d).symptoms[&aref("g", "k")],
            want
        );
    }

    #[test]
    fn a_chain_attributes_to_the_top_not_the_nearest_parent() {
        // hv -> guest -> container. All down. Only hv is a root.
        let edges = vec![
            edge(RelationKind::Hosts, "hv", "g"),
            edge(RelationKind::Runs, "g", "c"),
        ];
        let a = alert(AlertSeverity::Critical);
        let firing = vec![(aref("c", "k"), &a)];
        let imp = attribute(&edges, &firing, &down(&["hv", "g", "c"]));
        assert_eq!(imp.roots.keys().collect::<Vec<_>>(), vec!["hv"]);
        assert_eq!(
            imp.symptoms[&aref("c", "k")],
            Cause::Entity {
                entity_id: "hv".into()
            }
        );
    }

    #[test]
    fn a_dead_gateway_explains_the_hosts_behind_it() {
        let edges = vec![
            edge(RelationKind::GatewayOf, "gw", "h1"),
            edge(RelationKind::GatewayOf, "gw", "h2"),
        ];
        let a = alert(AlertSeverity::Critical);
        let firing = vec![(aref("h1", "k"), &a)];
        let imp = attribute(&edges, &firing, &down(&["gw", "h1", "h2"]));
        assert_eq!(
            imp.roots["gw"],
            BTreeSet::from(["h1".to_string(), "h2".to_string()])
        );
        assert!(imp.is_symptom(&aref("h1", "k")));
    }

    #[test]
    fn a_dead_vantage_explains_the_targets_it_probes() {
        let edges = vec![edge(RelationKind::Probes, "vantage", "target")];
        let a = alert(AlertSeverity::Warning);
        let firing = vec![(aref("target", "k"), &a)];
        let imp = attribute(&edges, &firing, &down(&["vantage", "target"]));
        assert_eq!(
            imp.symptoms[&aref("target", "k")],
            Cause::Entity {
                entity_id: "vantage".into()
            }
        );
    }

    #[test]
    fn l2_adjacency_propagates_nothing() {
        // Two hosts on one switch are peers, not parent and child. Treating
        // adjacency as containment would blame an arbitrary neighbour.
        let edges = vec![edge(RelationKind::L2Adjacent, "a", "b")];
        let al = alert(AlertSeverity::Critical);
        let firing = vec![(aref("b", "k"), &al)];
        let imp = attribute(&edges, &firing, &down(&["a", "b"]));
        // Both are roots: neither contains the other.
        assert_eq!(imp.roots.keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(imp.roots["a"].is_empty());
        assert!(imp.symptoms.is_empty());
    }

    #[test]
    fn a_cycle_terminates() {
        // Two hosts each claiming to be the other's gateway — which stale
        // neighbour tables really do produce, from sensors that have no way to
        // agree with each other that there is no cycle.
        let edges = vec![
            edge(RelationKind::GatewayOf, "a", "b"),
            edge(RelationKind::GatewayOf, "b", "a"),
        ];
        let al = alert(AlertSeverity::Critical);
        let firing = vec![(aref("a", "k"), &al), (aref("b", "k"), &al)];
        // Both down and each is the other's ancestor: neither is a root, and
        // the walk must still return rather than spin.
        let imp = attribute(&edges, &firing, &down(&["a", "b"]));
        assert!(imp.roots.is_empty());
        assert!(imp.symptoms.is_empty());
    }

    #[test]
    fn depth_is_capped() {
        // A six-link chain: the cap stops attribution four hops down.
        let ids: Vec<String> = (0..7).map(|i| format!("n{i}")).collect();
        let edges: Vec<Edge> = (0..6)
            .map(|i| edge(RelationKind::Runs, &ids[i], &ids[i + 1]))
            .collect();
        let all: Vec<&str> = ids.iter().map(String::as_str).collect();
        let imp = attribute(&edges, &[], &down(&all));
        assert_eq!(imp.roots["n0"].len(), MAX_DEPTH);
        assert!(imp.roots["n0"].contains("n4"));
        assert!(!imp.roots["n0"].contains("n5"));
    }

    #[test]
    fn nothing_down_means_no_impact() {
        let edges = vec![edge(RelationKind::Hosts, "hv", "g")];
        let a = alert(AlertSeverity::Critical);
        let firing = vec![(aref("g", "k"), &a)];
        let imp = attribute(&edges, &firing, &BTreeSet::new());
        assert_eq!(imp, Impact::default());
    }

    #[test]
    fn an_alert_on_a_healthy_entity_under_a_dead_root_is_still_a_symptom() {
        // The guest itself was never marked down — only the hypervisor was —
        // but its alerts are still explained by the hypervisor being gone.
        let edges = vec![edge(RelationKind::Hosts, "hv", "g")];
        let a = alert(AlertSeverity::Critical);
        let firing = vec![(aref("g", "k"), &a)];
        let imp = attribute(&edges, &firing, &down(&["hv"]));
        assert!(imp.is_symptom(&aref("g", "k")));
    }

    #[test]
    fn attribution_does_not_depend_on_edge_order() {
        let mut edges = vec![
            edge(RelationKind::Hosts, "hv", "g1"),
            edge(RelationKind::Runs, "g1", "c1"),
            edge(RelationKind::GatewayOf, "gw", "hv"),
        ];
        let a = alert(AlertSeverity::Critical);
        let firing = vec![(aref("c1", "k"), &a)];
        let d = down(&["gw", "hv", "g1", "c1"]);
        let first = attribute(&edges, &firing, &d);
        edges.reverse();
        assert_eq!(first, attribute(&edges, &firing, &d));
        // gw is above hv, so it is the only root and owns the whole chain.
        assert_eq!(first.roots.keys().collect::<Vec<_>>(), vec!["gw"]);
    }

    #[test]
    fn two_dead_roots_attribute_to_the_nearer_one_deterministically() {
        // c is one hop under b and two under a; b is not down, so a is the
        // only root and owns both.
        let edges = vec![
            edge(RelationKind::Hosts, "a", "b"),
            edge(RelationKind::Runs, "b", "c"),
            edge(RelationKind::GatewayOf, "z", "c"),
        ];
        let al = alert(AlertSeverity::Critical);
        let firing = vec![(aref("c", "k"), &al)];
        // Both a and z are down and neither is the other's ancestor: two
        // roots, and c is 1 hop from z but 2 from a — z wins.
        let imp = attribute(&edges, &firing, &down(&["a", "z"]));
        assert_eq!(
            imp.symptoms[&aref("c", "k")],
            Cause::Entity {
                entity_id: "z".into()
            }
        );
    }

    #[test]
    fn an_external_endpoint_neither_roots_nor_carries_impact() {
        // An upstream router the fleet can see but runs no sensor on: it has
        // no entity id, so it can never be in `down` and must not silently
        // become a path between two hosts.
        let e = Edge {
            edge_id: "e-0".into(),
            kind: RelationKind::GatewayOf,
            from: Endpoint::External {
                ip: Some("10.0.0.1".into()),
                mac: None,
                name: None,
            },
            to: Endpoint::Entity {
                entity_id: "h1".into(),
            },
            attrs: Default::default(),
            observers: Vec::new(),
            last_updated: 0,
        };
        let al = alert(AlertSeverity::Critical);
        let firing = vec![(aref("h1", "k"), &al)];
        let imp = attribute(&[e], &firing, &down(&["h1"]));
        assert_eq!(imp.roots.keys().collect::<Vec<_>>(), vec!["h1"]);
        assert!(imp.symptoms.is_empty());
    }
}
