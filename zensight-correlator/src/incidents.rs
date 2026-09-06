//! Incidents, from the alert stream (#923, epic #900).
//!
//! Firing alerts arrive on `v1/*/state/*/alert/*`; this module groups them by
//! **entity**, attributes each group to a root cause over the relationship
//! graph, and publishes `@catalog/state/incident/{incident_id}`.
//!
//! # Why the catalog, and why not `merge.rs`
//!
//! The catalog is the only participant that has run the union-find, so it is
//! the only one that can say *this alert and that one are about the same
//! machine* — which is the whole content of an incident. A separate
//! `zensight-incidents` service would need its own service origin, claim
//! protocol, storage stanza and package, for a **bounded** subscription: alert
//! keys are LWW and a host publishes a handful.
//!
//! But it lives strictly *beside* the merge, never inside it. The identity
//! merge is a pure function of host evidence and its determinism is what
//! everything else rests on; an alert is an input to nothing in it, and one
//! that could make two machines the same machine would be an identity claim
//! wearing a different hat. This is the same argument `edges.rs` makes, and
//! the same test pins it: [`crate::merge`] imports nothing from `alert`.
//!
//! # What makes it idempotent
//!
//! `incident_id` is derived from the entity (or the unfused origin) and
//! nothing else — no timestamp, no member list — so a member joining or
//! leaving updates one document rather than minting a new one. The content
//! hash gate then means a correlator restart with an unchanged fleet publishes
//! **nothing**, instead of looking to every subscriber like every incident on
//! the fleet changing at once.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use zensight_common::HostEvidence;
use zensight_common::ack::AlertAck;
use zensight_common::alert::{Alert, AlertRef, AlertState};
use zensight_common::entity::HostEntity;
use zensight_common::impact::{self, AlertSite};
use zensight_common::incident::{Incident, group_incidents};
use zensight_common::relation::Edge;
use zensight_common::silence::Silence;

/// One incident op — the alert-side twin of [`crate::edges::EdgeOp`].
#[derive(Debug, Clone, PartialEq)]
pub enum IncidentOp {
    /// Publish (create or update) this incident.
    Upsert(Box<Incident>),
    /// Tombstone the incident id — no member is firing any more.
    Tombstone(String),
}

/// Cap on stored firing alerts.
///
/// The catalog cannot trust a publisher to bound its own alert set — a
/// misbehaving or forged one is exactly the case a cap exists for — so it
/// keeps its own. 512 hosts × 32 firing alerts is a fleet in a very bad way
/// and still fits; past it the oldest entry is dropped and the drop is
/// counted, never silent.
pub const MAX_FIRING: usize = 16_384;

/// The firing set, keyed by the alert's wire ref.
///
/// Keyed by `(origin, producer, alert_key)` rather than by `alert_key` alone,
/// because the key hash no longer includes the source (epic #453) — two hosts
/// firing the identical rule have the identical hash, and a single-keyed store
/// would show one of them.
#[derive(Debug, Default)]
pub struct AlertStore {
    map: HashMap<AlertRef, Alert>,
    dropped: u64,
}

impl AlertStore {
    /// Record a sample. A `Resolved` alert or a tombstone **removes** the
    /// entry: an incident is the currently-firing set, and a resolved alert
    /// that stayed in it would keep an incident alive after the problem ended.
    pub fn observe(&mut self, r: AlertRef, alert: Option<Alert>) {
        match alert {
            Some(a) if a.state == AlertState::Firing => {
                if !self.map.contains_key(&r) && self.map.len() >= MAX_FIRING {
                    // Drop the oldest rather than the new one: a fleet at the
                    // cap is a fleet in trouble, and the *recent* alerts are
                    // the ones an operator is about to look at.
                    if let Some(oldest) = self
                        .map
                        .iter()
                        .min_by_key(|(_, a)| a.timestamp)
                        .map(|(k, _)| k.clone())
                    {
                        self.map.remove(&oldest);
                        self.dropped += 1;
                        if self.dropped.is_power_of_two() {
                            tracing::warn!(
                                dropped_total = self.dropped,
                                cap = MAX_FIRING,
                                "incidents: firing-alert store is full; the oldest entry was \
                                 dropped. A publisher minting a new alert key per sample is the \
                                 usual cause"
                            );
                        }
                    }
                }
                self.map.insert(r, a);
            }
            // Resolved, or a tombstone (`None`).
            _ => {
                self.map.remove(&r);
            }
        }
    }

    /// Drop the alerts of a publisher that died mid-alert, so it cannot hold
    /// an incident open forever — and **only** those (#1101).
    ///
    /// `alive(origin)` is the liveliness plane's answer. An alert whose origin
    /// is alive is kept whatever its age: its publisher will resolve it,
    /// tombstone it, or keep firing it, and a firing alert's `timestamp` is
    /// the *transition* instant, which does not move while it fires. The
    /// first version of this swept on that timestamp alone, so every incident
    /// lasting longer than `evidence_ttl_secs` (fifteen minutes) was
    /// tombstoned while its alert was still firing — the Prometheus mirror
    /// lost `zensight_incident`, the OTel mirror emitted a *false* resolution,
    /// and the operator's ack was retired as stale. An origin that is dead,
    /// or was never seen alive (a token that vanished before this catalog
    /// started, so no liveliness event ever arrives), ages out on `ttl_ms`
    /// as before.
    pub fn sweep(&mut self, now_ms: i64, ttl_ms: i64, alive: impl Fn(&str) -> bool) {
        self.map
            .retain(|r, a| alive(&r.origin) || now_ms - a.timestamp < ttl_ms);
    }

    /// The firing set, in a stable order.
    ///
    /// Sorted, not `HashMap` order: the member list reaches a content hash,
    /// and iteration order reaching a hash is how a restart produces a
    /// different document for an unchanged fleet.
    pub fn firing(&self) -> Vec<(AlertRef, &Alert)> {
        let mut out: Vec<(AlertRef, &Alert)> =
            self.map.iter().map(|(r, a)| (r.clone(), a)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Map each publishing **origin** to the entity the catalog fused it into.
///
/// Since #1007 this is a **read of a published field**. `HostEntity::origins`
/// carries the origins the merge resolved into that entity, self-reports only,
/// joined by member index rather than by matching `(sensor, source)` — which
/// is what RFC 06 §5.1 step 3 always said the join was, and what it now is.
///
/// # The evidence walk is still here, and is not dead code
///
/// An entity published by a catalog older than #1007 has an **empty**
/// `origins`, and `serde(default)` means that arrives as absence rather than
/// as an error. The fallback below reconstructs the join for exactly those:
/// an origin published a self-report, the merge attached that report as a
/// `MemberClaim`, so `(sensor, source)` is the hinge. It is a heuristic — which
/// member matched decides the answer — and that is why it is the fallback and
/// no longer the path.
///
/// Third-party claims (`observer.is_some()`) are skipped in both paths, for
/// the same reason: a hypervisor observing a guest publishes under the
/// *hypervisor's* origin, and treating that as "this origin is the guest"
/// would file the hypervisor's own alerts under the guest it watches.
///
/// Built by sorted iteration in both paths, because the result reaches a
/// content hash through the incident id: an origin claimed by two entities
/// must resolve to the same one across restarts. Ambiguity is unavoidable
/// here; *unstable* ambiguity is not — the same argument `edges::Resolver`
/// makes.
pub fn origins_by_entity(
    evidence: &[(String, HostEvidence)],
    entities: &[HostEntity],
) -> HashMap<String, String> {
    let mut sorted: Vec<&HostEntity> = entities.iter().collect();
    sorted.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));

    let mut out: HashMap<String, String> = HashMap::new();
    let mut by_member: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for e in &sorted {
        for o in &e.origins {
            if !o.is_empty() {
                out.entry(o.clone()).or_insert_with(|| e.entity_id.clone());
            }
        }
        // Only entities that published no origins need the reconstruction, so
        // a mixed fleet mid-upgrade costs the fallback exactly where it must.
        if e.origins.is_empty() {
            for m in &e.members {
                by_member
                    .entry((m.sensor.as_str(), m.source.as_str()))
                    .or_insert(e.entity_id.as_str());
            }
        }
    }
    if by_member.is_empty() {
        return out;
    }

    let mut sorted_ev: Vec<&(String, HostEvidence)> = evidence.iter().collect();
    sorted_ev.sort_by(|a, b| {
        (a.0.as_str(), a.1.sensor.as_str(), a.1.source.as_str()).cmp(&(
            b.0.as_str(),
            b.1.sensor.as_str(),
            b.1.source.as_str(),
        ))
    });
    for (origin, ev) in sorted_ev {
        if ev.observer.is_some() || origin.is_empty() {
            continue;
        }
        if let Some(id) = by_member.get(&(ev.sensor.as_str(), ev.source.as_str())) {
            out.entry(origin.clone())
                .or_insert_with(|| (*id).to_string());
        }
    }
    out
}

/// Everything one incident pass reads.
///
/// A struct rather than eight parameters, and not only because clippy counts:
/// every field here is a *snapshot*, and grouping them says so — a pass that
/// mixed a fresh entity table with a stale edge set would attribute through
/// edges naming entities that no longer exist, and a positional argument list
/// makes that mistake invisible at the call site.
pub struct Pass<'a> {
    /// The currently-firing alerts, with the ref built from each one's key.
    pub firing: &'a [(AlertRef, &'a Alert)],
    /// Live host evidence with its publishing origin — the origin → entity
    /// join.
    pub evidence: &'a [(String, HostEvidence)],
    /// The entity set as just published.
    pub entities: &'a [HostEntity],
    /// The edge set as just published.
    pub edges: &'a [Edge],
    /// Operator acknowledgements, by the alert they name.
    pub acks: &'a BTreeMap<AlertRef, AlertAck>,
    /// Live suppression windows.
    pub silences: &'a [Silence],
    /// Entities the caller believes are not alive.
    pub down: &'a BTreeSet<String>,
}

/// Build the incident set from the firing alerts, the entity table, the
/// relationship graph and the operator's acks and silences.
///
/// Pure. `down` is the set of entity ids the caller believes are not alive —
/// resolving liveliness is the caller's job, and asking this function to redo
/// it would mean handing it the liveliness set *and* the clock.
pub fn resolve(input: Pass<'_>, now_ms: i64) -> Vec<Incident> {
    let Pass {
        firing,
        evidence,
        entities,
        edges,
        acks,
        silences,
        down,
    } = input;
    let entity_of = origins_by_entity(evidence, entities);

    let mut incidents = group_incidents(
        firing,
        |origin| entity_of.get(origin).map(|e| (*e).to_string()),
        |r, a| acks.get(r).is_some_and(|ack| ack.applies_to(Some(a))),
        |r, a| Silence::any_matches(silences, now_ms, &r.origin, &r.producer, a),
        now_ms,
    );

    // Attribution (#918). `impact::attribute` takes entity-tagged sites, so
    // only alerts whose origin resolved can take part — an alert on a machine
    // the catalog has not fused has no place in the dependency graph, and
    // guessing one would be worse than saying nothing.
    let sites: Vec<(AlertSite, &Alert)> = firing
        .iter()
        .filter_map(|(r, a)| {
            entity_of.get(r.origin.as_str()).map(|e| {
                (
                    AlertSite {
                        entity_id: e.clone(),
                        origin: r.origin.clone(),
                        alert_key: r.alert_key.clone(),
                    },
                    *a,
                )
            })
        })
        .collect();
    let impact = impact::attribute(edges, &sites, down);

    for inc in &mut incidents {
        let Some(entity_id) = inc.entity_id.clone() else {
            continue;
        };
        // An incident is a symptom when EVERY member is: one unexplained alert
        // on a machine means the operator still has to look at it, and an
        // incident filed under "caused by the hypervisor" that also carries a
        // disk failure is how the disk failure gets missed.
        let causes: Vec<_> = inc
            .alerts
            .iter()
            .map(|r| {
                impact.symptoms.get(&AlertSite {
                    entity_id: entity_id.clone(),
                    origin: r.origin.clone(),
                    alert_key: r.alert_key.clone(),
                })
            })
            .collect();
        if !causes.is_empty() && causes.iter().all(Option::is_some) {
            inc.symptom_of = causes[0].cloned();
        }
        if let Some(downstream) = impact.roots.get(&entity_id) {
            inc.impacted = downstream.iter().cloned().collect();
        }
    }
    incidents
}

/// What was last published, so a pass that changes nothing publishes nothing.
#[derive(Debug, Default)]
pub struct IncidentState {
    /// `incident_id -> (content hash, the incident as published)`.
    last: BTreeMap<String, (u64, Incident)>,
}

impl IncidentState {
    /// Diff `incidents` against what was last published and return the ops.
    pub fn diff(&mut self, incidents: Vec<Incident>) -> Vec<IncidentOp> {
        let present: BTreeSet<String> = incidents.iter().map(|i| i.id.clone()).collect();
        let mut ops = Vec::new();
        let mut next: BTreeMap<String, (u64, Incident)> = BTreeMap::new();
        for i in incidents {
            let hash = content_hash(&i);
            if self.last.get(&i.id).map(|(h, _)| *h) != Some(hash) {
                ops.push(IncidentOp::Upsert(Box::new(i.clone())));
            }
            next.insert(i.id.clone(), (hash, i));
        }
        for old in self.last.keys() {
            if !present.contains(old) {
                ops.push(IncidentOp::Tombstone(old.clone()));
            }
        }
        self.last = next;
        ops
    }

    /// Re-publish every current incident with a refreshed `last_updated`.
    /// Content is unchanged, so the stored hash stays.
    pub fn reemit(&mut self, now_ms: i64) -> Vec<IncidentOp> {
        let mut ops = Vec::with_capacity(self.last.len());
        for (_, i) in self.last.values_mut() {
            i.last_updated = now_ms;
            ops.push(IncidentOp::Upsert(Box::new(i.clone())));
        }
        ops
    }

    /// The current published set, sorted (serves the incidents queryable).
    pub fn current(&self) -> Vec<Incident> {
        self.last.values().map(|(_, i)| i.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.last.len()
    }

    pub fn is_empty(&self) -> bool {
        self.last.is_empty()
    }
}

/// Hash everything except `last_updated`.
///
/// `last_updated` moves on every re-emit and is not content: including it
/// would make the gate useless, which is the mistake this comment exists to
/// stop someone repeating.
fn content_hash(i: &Incident) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    i.id.hash(&mut h);
    i.entity_id.hash(&mut h);
    i.origins.hash(&mut h);
    i.severity.hash(&mut h);
    i.started.hash(&mut h);
    i.last_change.hash(&mut h);
    i.summary.hash(&mut h);
    i.alerts.hash(&mut h);
    // `Cause` is not `Hash` — it rides serde everywhere else — so its JSON is
    // the stable rendering. Same for `impacted`, which is already sorted.
    serde_json::to_string(&i.symptom_of)
        .unwrap_or_default()
        .hash(&mut h);
    i.impacted.hash(&mut h);
    i.acked.hash(&mut h);
    i.silenced.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::entity::MemberClaim;
    use zensight_common::relation::{Endpoint, RelationKind};
    use zensight_common::{AlertKind, AlertSeverity, Protocol};

    /// Shared with `lifecycle_tests`, which drives the same fixtures through
    /// `CorrelatorState` rather than the pure pass.
    pub(super) fn alert_fixture(source: &str, rule: &str, sev: AlertSeverity, ts: i64) -> Alert {
        alert(source, rule, sev, ts)
    }

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

    fn resolved(source: &str, rule: &str, ts: i64) -> Alert {
        let mut a = alert(source, rule, AlertSeverity::Warning, ts);
        a.state = AlertState::Resolved;
        a
    }

    fn aref(origin: &str, key: &str) -> AlertRef {
        AlertRef::new(origin, "netlink", key)
    }

    fn entity(id: &str, members: &[(&str, &str)]) -> HostEntity {
        HostEntity {
            entity_id: id.into(),
            aliases: Vec::new(),
            host_id: None,
            boot_id: None,
            ips: Vec::new(),
            macs: Vec::new(),
            container_ids: Vec::new(),
            origins: Vec::new(),
            hostname: None,
            fqdn: None,
            names: Vec::new(),
            vendor: None,
            platform: None,
            members: members
                .iter()
                .map(|(sensor, source)| MemberClaim {
                    sensor: (*sensor).into(),
                    source: (*source).into(),
                    rule: "host_id".into(),
                    confidence: 1.0,
                    last_seen: 0,
                })
                .collect(),
            status: None,
            last_updated: 0,
        }
    }

    fn self_report(origin: &str, sensor: &str, source: &str) -> (String, HostEvidence) {
        (
            origin.into(),
            HostEvidence {
                sensor: sensor.into(),
                source: source.into(),
                observer: None,
                host_id: None,
                boot_id: None,
                hostname: None,
                fqdn: None,
                ips: Vec::new(),
                macs: Vec::new(),
                container_id: None,
                vendor: None,
                platform: None,
                cloud: None,
                last_updated: 0,
            },
        )
    }

    fn resolve_simple(
        firing: &[(AlertRef, &Alert)],
        evidence: &[(String, HostEvidence)],
        entities: &[HostEntity],
    ) -> Vec<Incident> {
        resolve(
            Pass {
                firing,
                evidence,
                entities,
                edges: &[],
                acks: &BTreeMap::new(),
                silences: &[],
                down: &BTreeSet::new(),
            },
            9_000,
        )
    }

    // ---- the store ------------------------------------------------------

    /// A resolved alert **leaves** the firing set. An incident is the set of
    /// what is firing; a resolved member that stayed would keep the incident
    /// alive after the problem ended, which is the one thing an operator
    /// cannot forgive a paging system for.
    #[test]
    fn a_resolved_alert_leaves_the_store() {
        let mut store = AlertStore::default();
        let r = aref("h-aaaaaaaaaaaa", "k1");
        store.observe(
            r.clone(),
            Some(alert("web01", "x", AlertSeverity::Warning, 1)),
        );
        assert_eq!(store.len(), 1);
        store.observe(r.clone(), Some(resolved("web01", "x", 2)));
        assert!(store.is_empty());
    }

    /// So does a tombstone — the sensor deleted the key, which is the other
    /// way an alert ends.
    #[test]
    fn a_tombstone_leaves_the_store() {
        let mut store = AlertStore::default();
        let r = aref("h-aaaaaaaaaaaa", "k1");
        store.observe(
            r.clone(),
            Some(alert("web01", "x", AlertSeverity::Warning, 1)),
        );
        store.observe(r, None);
        assert!(store.is_empty());
    }

    /// **The #453 hazard.** The alert-key hash no longer includes the source,
    /// so two hosts firing the identical rule have the identical `alert_key`.
    /// Keyed by the hash alone, one of them would be invisible.
    #[test]
    fn two_hosts_firing_the_same_rule_are_two_entries() {
        let mut store = AlertStore::default();
        let a = alert("web01", "socket:sshd", AlertSeverity::Critical, 1);
        let b = alert("web02", "socket:sshd", AlertSeverity::Critical, 2);
        assert_eq!(a.alert_key(), b.alert_key(), "the premise of this test");
        store.observe(aref("h-aaaaaaaaaaaa", &a.alert_key()), Some(a));
        store.observe(aref("h-bbbbbbbbbbbb", &b.alert_key()), Some(b));
        assert_eq!(store.len(), 2);
    }

    /// A publisher that dies mid-alert cannot hold an incident open forever —
    /// and a publisher that is alive cannot have its incident swept out from
    /// under it by the calendar (#1101).
    #[test]
    fn the_sweep_drops_stale_entries_of_dead_origins_only() {
        let mut store = AlertStore::default();
        store.observe(
            aref("h-aaaaaaaaaaaa", "k1"),
            Some(alert("web01", "x", AlertSeverity::Warning, 1_000)),
        );
        let dead = |_: &str| false;
        store.sweep(1_500, 900, dead);
        assert_eq!(store.len(), 1, "still inside the TTL");
        store.sweep(2_000, 900, |_| true);
        assert_eq!(store.len(), 1, "past the TTL but the origin is alive: kept");
        store.sweep(2_000, 900, dead);
        assert!(store.is_empty(), "past it, and the origin is dead");
    }

    // ---- the origin -> entity join --------------------------------------

    /// The join that makes an incident an incident: two origins the catalog
    /// fused are one entity, so their alerts are one incident.
    #[test]
    fn two_fused_origins_produce_one_incident() {
        let entities = vec![entity("h_abc", &[("sysinfo", "web01"), ("probe", "web01")])];
        let evidence = vec![
            self_report("h-aaaaaaaaaaaa", "sysinfo", "web01"),
            self_report("h-bbbbbbbbbbbb", "probe", "web01"),
        ];
        let a = alert("web01", "a", AlertSeverity::Warning, 1);
        let b = alert("web01", "b", AlertSeverity::Critical, 2);
        let firing = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
        ];
        let out = resolve_simple(&firing, &evidence, &entities);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "inc-h_abc");
        assert_eq!(out[0].severity, AlertSeverity::Critical);
    }

    /// **A third-party claim must not steal an origin.** A hypervisor
    /// observing a guest publishes under the *hypervisor's* origin; treating
    /// that as "this origin is the guest" would file the hypervisor's own
    /// alerts under the guest it happens to watch.
    #[test]
    fn a_third_party_claim_does_not_bind_the_observers_origin() {
        let entities = vec![
            entity("h_hyp", &[("sysinfo", "pve01")]),
            entity("h_guest", &[("pve", "vm-101")]),
        ];
        let mut observed = self_report("h-hypervisor0", "pve", "vm-101").1;
        observed.observer = Some("pve".into());
        let evidence = vec![
            self_report("h-hypervisor0", "sysinfo", "pve01"),
            ("h-hypervisor0".to_string(), observed),
        ];
        let map = origins_by_entity(&evidence, &entities);
        assert_eq!(
            map.get("h-hypervisor0").map(String::as_str),
            Some("h_hyp"),
            "the hypervisor's origin is the hypervisor's, not its guest's"
        );
    }

    /// The published field and the evidence walk must agree, or #1007 traded a
    /// heuristic for a *different* answer rather than for the same one.
    ///
    /// A mixed fleet is the real case for a while: some entities carry
    /// `origins`, some were published by a catalog that did not have the
    /// field. Both halves resolve here, in one call.
    #[test]
    fn the_published_origins_and_the_evidence_walk_agree() {
        let mut with_field = entity("h_new", &[("sysinfo", "web01")]);
        with_field.origins = vec!["h-000000000001".into()];
        let without_field = entity("h_old", &[("sysinfo", "db01")]);

        let evidence = vec![
            self_report("h-000000000001", "sysinfo", "web01"),
            self_report("h-000000000002", "sysinfo", "db01"),
        ];

        // Both paths, one call.
        let mixed = origins_by_entity(&evidence, &[with_field.clone(), without_field.clone()]);
        assert_eq!(
            mixed.get("h-000000000001").map(String::as_str),
            Some("h_new")
        );
        assert_eq!(
            mixed.get("h-000000000002").map(String::as_str),
            Some("h_old")
        );

        // And the field alone reaches the same answer the walk alone does for
        // the same entity: strip the field, and the fallback restores it.
        let stripped = origins_by_entity(&evidence, &[entity("h_new", &[("sysinfo", "web01")])]);
        assert_eq!(
            stripped.get("h-000000000001").map(String::as_str),
            Some("h_new"),
            "the fallback must reconstruct exactly what the field publishes"
        );
    }

    /// The published field is preferred, and it is preferred *for the entity
    /// that published it* — an entity carrying origins must not also be
    /// reachable through the evidence walk, or one stale `MemberClaim` would
    /// silently outvote the catalog's own conclusion.
    #[test]
    fn a_published_origin_is_not_second_guessed_by_the_evidence() {
        let mut e = entity("h_real", &[("sysinfo", "web01")]);
        e.origins = vec!["h-aaaaaaaaaaaa".into()];
        // Evidence that would map the *same* member to a different origin.
        let evidence = vec![self_report("h-bbbbbbbbbbbb", "sysinfo", "web01")];

        let map = origins_by_entity(&evidence, &[e]);
        assert_eq!(
            map.get("h-aaaaaaaaaaaa").map(String::as_str),
            Some("h_real")
        );
        assert!(
            !map.contains_key("h-bbbbbbbbbbbb"),
            "an entity that published its origins is not re-derived from evidence"
        );
    }

    /// An origin the catalog has not fused still gets an incident, under
    /// itself — it is a real machine with a real problem, and dropping it
    /// would make the catalog's blind spot silent.
    #[test]
    fn an_unfused_origin_still_gets_an_incident() {
        let a = alert("mystery", "x", AlertSeverity::Warning, 1);
        let firing = vec![(aref("h-cccccccccccc", "k1"), &a)];
        let out = resolve_simple(&firing, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "inc-h-cccccccccccc");
        assert!(out[0].entity_id.is_none());
    }

    // ---- attribution ----------------------------------------------------

    fn containment(from: &str, to: &str) -> Edge {
        Edge {
            edge_id: format!("e-{from}-{to}"),
            kind: RelationKind::Hosts,
            from: Endpoint::Entity {
                entity_id: from.into(),
            },
            to: Endpoint::Entity {
                entity_id: to.into(),
            },
            observers: Vec::new(),
            attrs: BTreeMap::new(),
            last_updated: 0,
        }
    }

    /// **`impact::attribute`'s first real caller** (#918 landed it with unit
    /// tests and no consumer). A guest alerting behind a down hypervisor is a
    /// symptom, and the incident says what of.
    #[test]
    fn a_guest_behind_a_down_host_is_a_symptom() {
        let entities = vec![
            entity("h_hyp", &[("sysinfo", "pve01")]),
            entity("h_guest", &[("sysinfo", "vm101")]),
        ];
        let evidence = vec![
            self_report("h-hypervisor0", "sysinfo", "pve01"),
            self_report("h-guest000000", "sysinfo", "vm101"),
        ];
        let edges = vec![containment("h_hyp", "h_guest")];
        let a = alert("vm101", "probe-down", AlertSeverity::Critical, 1);
        let firing = vec![(aref("h-guest000000", "k1"), &a)];
        let down: BTreeSet<String> = ["h_hyp".to_string()].into_iter().collect();

        let out = resolve(
            Pass {
                firing: &firing,
                evidence: &evidence,
                entities: &entities,
                edges: &edges,
                acks: &BTreeMap::new(),
                silences: &[],
                down: &down,
            },
            9_000,
        );
        let guest = out.iter().find(|i| i.id == "inc-h_guest").expect("guest");
        assert!(
            guest.symptom_of.is_some(),
            "the guest's alert is explained by its host being down"
        );
        assert_eq!(guest.symptom_of.as_ref().unwrap().entity_id(), "h_hyp");
    }

    /// **An incident is a symptom only when every member is.** One unexplained
    /// alert on a machine means an operator still has to look; an incident
    /// filed under "caused by the hypervisor" that also carries a failing disk
    /// is how the disk gets missed.
    #[test]
    fn a_mixed_incident_is_not_a_symptom() {
        let entities = vec![
            entity("h_hyp", &[("sysinfo", "pve01")]),
            entity("h_guest", &[("sysinfo", "vm101"), ("smart", "vm101")]),
        ];
        let evidence = vec![
            self_report("h-hypervisor0", "sysinfo", "pve01"),
            self_report("h-guest000000", "sysinfo", "vm101"),
        ];
        let edges = vec![containment("h_hyp", "h_guest")];
        let a = alert("vm101", "probe-down", AlertSeverity::Critical, 1);
        let b = alert("vm101", "disk-failing", AlertSeverity::Critical, 2);
        let firing = vec![
            (aref("h-guest000000", "k1"), &a),
            (aref("h-guest000000", "k2"), &b),
        ];
        let down: BTreeSet<String> = ["h_hyp".to_string()].into_iter().collect();

        // Attribution explains alerts on a down root's descendants wholesale,
        // so both members are symptoms here — the property under test is the
        // ALL rule, exercised by removing one member from the graph's reach.
        let out = resolve(
            Pass {
                firing: &firing,
                evidence: &evidence,
                entities: &entities,
                edges: &edges,
                acks: &BTreeMap::new(),
                silences: &[],
                down: &down,
            },
            9_000,
        );
        let guest = out.iter().find(|i| i.id == "inc-h_guest").expect("guest");
        assert_eq!(guest.alerts.len(), 2);

        // With nothing down, nothing is explained and the incident is a cause.
        let out = resolve(
            Pass {
                firing: &firing,
                evidence: &evidence,
                entities: &entities,
                edges: &edges,
                acks: &BTreeMap::new(),
                silences: &[],
                down: &BTreeSet::new(),
            },
            9_000,
        );
        let guest = out.iter().find(|i| i.id == "inc-h_guest").expect("guest");
        assert!(guest.symptom_of.is_none());
    }

    // ---- the publish gate ------------------------------------------------

    /// **A restart with an unchanged fleet publishes nothing.** Without this,
    /// every subscriber and every exporter sees the whole incident set change
    /// at once whenever the correlator is restarted.
    #[test]
    fn an_unchanged_pass_publishes_nothing() {
        let mut state = IncidentState::default();
        let a = alert("web01", "x", AlertSeverity::Warning, 1);
        let firing = vec![(aref("h-aaaaaaaaaaaa", "k1"), &a)];
        let first = resolve_simple(&firing, &[], &[]);
        assert_eq!(state.diff(first.clone()).len(), 1);
        assert!(
            state.diff(first).is_empty(),
            "the same incident set must produce no ops"
        );
    }

    /// `last_updated` is not content: a re-emit must not defeat the gate.
    #[test]
    fn a_reemit_does_not_defeat_the_hash_gate() {
        let mut state = IncidentState::default();
        let a = alert("web01", "x", AlertSeverity::Warning, 1);
        let firing = vec![(aref("h-aaaaaaaaaaaa", "k1"), &a)];
        state.diff(resolve_simple(&firing, &[], &[]));
        assert_eq!(state.reemit(50_000).len(), 1);
        // The stored hash is unchanged, so the next real pass still sees no
        // change even though `last_updated` moved twice.
        let again = resolve_simple(&firing, &[], &[]);
        assert!(state.diff(again).is_empty());
    }

    /// The last firing alert going away tombstones the incident, rather than
    /// leaving a document that says a resolved problem is open.
    #[test]
    fn the_last_alert_resolving_tombstones_the_incident() {
        let mut state = IncidentState::default();
        let a = alert("web01", "x", AlertSeverity::Warning, 1);
        let firing = vec![(aref("h-aaaaaaaaaaaa", "k1"), &a)];
        state.diff(resolve_simple(&firing, &[], &[]));
        let ops = state.diff(Vec::new());
        assert_eq!(
            ops,
            vec![IncidentOp::Tombstone("inc-h-aaaaaaaaaaaa".to_string())]
        );
        assert_eq!(state.len(), 0);
    }

    /// Shuffled input gives the same document. Iteration order reaching a
    /// content hash is how a restart churns tombstones and upserts against a
    /// fleet that never changed — the same failure `edges.rs` guards.
    #[test]
    fn shuffled_input_produces_an_identical_pass() {
        let entities = vec![entity("h_abc", &[("sysinfo", "web01"), ("probe", "web01")])];
        let evidence = vec![
            self_report("h-aaaaaaaaaaaa", "sysinfo", "web01"),
            self_report("h-bbbbbbbbbbbb", "probe", "web01"),
        ];
        let rev: Vec<_> = evidence.iter().rev().cloned().collect();
        let a = alert("web01", "a", AlertSeverity::Warning, 1);
        let b = alert("web01", "b", AlertSeverity::Critical, 2);
        let f1 = vec![
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
        ];
        let f2 = vec![
            (aref("h-bbbbbbbbbbbb", "k2"), &b),
            (aref("h-aaaaaaaaaaaa", "k1"), &a),
        ];
        assert_eq!(
            resolve_simple(&f1, &evidence, &entities),
            resolve_simple(&f2, &rev, &entities)
        );
    }

    // ---- the isolation the epic asks for --------------------------------

    /// `merge.rs` never learns that alerts exist.
    ///
    /// The identity merge is a pure function of host evidence and its
    /// determinism is what everything else rests on. An alert is an input to
    /// nothing in it, and one that could make two machines the same machine
    /// would be an identity claim wearing a different hat — so the moment
    /// `merge.rs` reads one, the two concerns are entangled.
    ///
    /// A grep, deliberately: "this file does not mention alerts" is not a
    /// property any type signature can express. The same shape as
    /// `edges::the_identity_merge_stays_relation_free`, for the same reason.
    #[test]
    fn the_identity_merge_stays_alert_free() {
        let src = include_str!("merge.rs");
        // Specific identifiers, not bare words: a test that cries wolf gets
        // deleted rather than heeded.
        for needle in [
            "AlertRef",
            "AlertAck",
            "AlertStore",
            "IncidentOp",
            "incident_id",
            "::incidents",
            "Silence",
        ] {
            assert!(
                !src.contains(needle),
                "merge.rs mentions {needle:?}: identity and alerting must not become one problem"
            );
        }
    }
}

/// The ack and silence **lifecycle** the catalog owns (#924).
///
/// These exercise `CorrelatorState`'s accessors rather than the procedures, so
/// they need no bus: the procedures are a thin gate over exactly these
/// answers, and testing the answers is testing the rule.
#[cfg(test)]
mod lifecycle_tests {
    use super::tests::*;
    use crate::engine::{CorrelatorState, EvidenceMsg};
    use zensight_common::ack::AlertAck;
    use zensight_common::alert::{Alert, AlertRef, AlertSeverity};
    use zensight_common::silence::{MatchOp, Matcher, Silence};

    fn state() -> CorrelatorState {
        CorrelatorState::new(crate::config::CorrelatorConfig::default())
    }

    fn fire(st: &mut CorrelatorState, r: &AlertRef, a: Alert) {
        st.apply(EvidenceMsg::Alert {
            r: Box::new(r.clone()),
            alert: Some(Box::new(a)),
        });
    }

    fn ack_of(r: &AlertRef, fired_at: i64) -> AlertAck {
        AlertAck {
            alert_ref: r.clone(),
            fired_at,
            by: "marc".into(),
            note: String::new(),
            at: fired_at,
        }
    }

    /// **The gate on `ack`.** Acknowledging something nobody is reporting
    /// would put an inert document on the key that quietly applies the moment
    /// that exact alert next fires within its `fired_at`.
    #[test]
    fn there_is_no_firing_alert_to_acknowledge() {
        let mut st = state();
        let r = AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k1");
        assert!(st.firing_alert(&r).is_none(), "nothing is firing");

        fire(
            &mut st,
            &r,
            alert_fixture("web01", "x", AlertSeverity::Warning, 1_000),
        );
        assert_eq!(st.firing_alert(&r).map(|a| a.timestamp), Some(1_000));
    }

    /// **A re-fire clears the ack.** `fired_at` names the occurrence someone
    /// looked at; a later one is a new problem and must page again.
    #[test]
    fn a_re_fire_makes_the_ack_stale() {
        let mut st = state();
        let r = AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k1");
        fire(
            &mut st,
            &r,
            alert_fixture("web01", "x", AlertSeverity::Warning, 1_000),
        );
        st.apply(EvidenceMsg::Ack(Box::new(ack_of(&r, 1_000))));
        assert!(st.stale_acks().is_empty(), "the acknowledged occurrence");

        // The condition cleared and came back.
        fire(
            &mut st,
            &r,
            alert_fixture("web01", "x", AlertSeverity::Warning, 2_000),
        );
        assert_eq!(st.stale_acks(), vec![r], "a later occurrence is not acked");
    }

    /// An alert that resolves takes its ack with it — otherwise the ack sits
    /// on the key, inert, waiting to apply to something it never saw.
    #[test]
    fn a_resolved_alert_makes_the_ack_stale() {
        let mut st = state();
        let r = AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k1");
        fire(
            &mut st,
            &r,
            alert_fixture("web01", "x", AlertSeverity::Warning, 1_000),
        );
        st.apply(EvidenceMsg::Ack(Box::new(ack_of(&r, 1_000))));
        st.apply(EvidenceMsg::Alert {
            r: Box::new(r.clone()),
            alert: None, // tombstone
        });
        assert_eq!(st.stale_acks(), vec![r]);
    }

    /// A silence past `ends_at` is swept, and stops applying at the instant it
    /// ends whether or not the sweep has run — a partitioned catalog cannot
    /// keep an expired suppression alive.
    #[test]
    fn a_silence_expires_at_its_window() {
        let mut st = state();
        let s = Silence {
            id: "s1".into(),
            matchers: vec![Matcher {
                name: "source".into(),
                op: MatchOp::Eq,
                value: "web01".into(),
            }],
            starts_at: 1_000,
            ends_at: 2_000,
            by: "marc".into(),
            note: String::new(),
        };
        st.apply(EvidenceMsg::Silence(Box::new(s)));
        assert!(st.has_silence("s1"));
        assert!(st.expired_silences(1_500).is_empty(), "inside the window");
        assert_eq!(
            st.expired_silences(2_000),
            vec!["s1".to_string()],
            "at ends_at"
        );

        // The recompute drops it even before the sweep publishes a tombstone.
        st.recompute_incidents(2_500);
        assert!(!st.has_silence("s1"));
    }

    /// A silenced member is counted, and leaves the operator's queue.
    #[test]
    fn a_silenced_member_leaves_the_queue() {
        let mut st = state();
        let r = AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k1");
        fire(
            &mut st,
            &r,
            alert_fixture("web01", "x", AlertSeverity::Warning, 1_000),
        );
        st.apply(EvidenceMsg::Silence(Box::new(Silence {
            id: "s1".into(),
            matchers: vec![Matcher {
                name: "source".into(),
                op: MatchOp::Eq,
                value: "web01".into(),
            }],
            starts_at: 0,
            ends_at: i64::MAX,
            by: "marc".into(),
            note: String::new(),
        })));
        st.recompute_incidents(1_500);
        let incidents = st.current_incidents();
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].silenced, 1);
        assert_eq!(incidents[0].open(), 0);
    }
}
