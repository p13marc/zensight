//! Physical-host aggregate (#128 → #306) — the keystone re-key of the dashboard.
//!
//! `DeviceId = (protocol, source)`, so one physical host running
//! sysinfo+netlink+logs+netring is four [`DeviceState`]s. A [`Host`] groups those
//! per-protocol **facets** into **one card per host** with merged identity, a
//! composite health score, and per-facet badges.
//!
//! Before the correlator (#305) grouping was by `source` string alone. With an
//! [`EntityStore`] the grouping key becomes **entity-or-source**: devices whose
//! `DeviceId` maps into an entity's `members[]` collapse under one
//! [`HostKey::Entity`]; everything else falls back to [`HostKey::Source`], which
//! reproduces the pre-correlation behavior **bit-for-bit** when the store is
//! empty (the degraded path).

use std::collections::{BTreeMap, HashMap, HashSet};

use zensight_common::{DeviceStatus, HostEntity, Protocol};

use crate::entity::EntityStore;
use crate::view::dashboard::{DeviceState, status_rank};
use crate::view::health::{self, HealthScore};

/// The grouping key for a [`Host`] card: a correlated entity, or a bare source
/// (degraded / uncorrelated).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostKey {
    /// A correlator entity id (`h_<12hex>`).
    Entity(String),
    /// A per-sensor source string (no entity claims this device).
    Source(String),
}

/// A physical host: all per-protocol facet [`DeviceState`]s that share a host,
/// facet-sorted by identity priority.
#[derive(Debug)]
pub struct Host<'a> {
    /// Stable grouping key (entity id or source).
    pub key: HostKey,
    /// Display name: entity hostname/fqdn when correlated, else the source.
    pub display_name: String,
    /// The correlator entity backing this host, when correlated.
    pub entity: Option<&'a HostEntity>,
    /// Facets (one per protocol present), primary-first.
    pub facets: Vec<&'a DeviceState>,
}

/// Identity priority for a host's primary facet (sysinfo > netlink > netring >
/// logs/syslog > everything else). Mirrors topology's `primary_protocol` and
/// extends it across all protocols. Lower sorts first.
pub fn protocol_priority(p: Protocol) -> u8 {
    match p {
        Protocol::Sysinfo => 0,
        Protocol::Netlink => 1,
        Protocol::Netring => 2,
        Protocol::Logs => 3,
        _ => 4,
    }
}

/// Protocols appearing on two or more facets of the same host (e.g. a
/// toolbox-run sysinfo sensor with source "toolbx" correlated into the same
/// host as the host-run one). Callers use this to disambiguate same-protocol
/// facet badges/tabs by appending the facet's `source`. Pure over any protocol
/// iterator so both the dashboard cards and the facet tab strip share it.
pub fn duplicated_protocols(protocols: impl Iterator<Item = Protocol>) -> HashSet<Protocol> {
    let mut counts: HashMap<Protocol, usize> = HashMap::new();
    for p in protocols {
        *counts.entry(p).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(p, n)| (n >= 2).then_some(p))
        .collect()
}

/// Best display name for an entity: hostname > fqdn > short entity id.
fn entity_display_name(e: &HostEntity) -> String {
    e.hostname
        .clone()
        .or_else(|| e.fqdn.clone())
        .unwrap_or_else(|| e.entity_id.clone())
}

impl<'a> Host<'a> {
    /// The primary facet — the card's default open target.
    pub fn primary(&self) -> &'a DeviceState {
        self.facets[0]
    }

    /// Worst effective status across facets (problem-first).
    pub fn effective_status(&self) -> DeviceStatus {
        self.facets
            .iter()
            .map(|f| f.effective_status())
            .min_by_key(|s| status_rank(*s))
            .unwrap_or(DeviceStatus::Unknown)
    }

    /// Total metrics across all facets.
    pub fn metric_count(&self) -> usize {
        self.facets.iter().map(|f| f.metric_count).sum()
    }

    /// Composite health: the worst of the facet scores (#130/#128).
    pub fn health(&self) -> HealthScore {
        health::score_host(self.facets.iter().copied())
    }

    /// Number of merged member sources when correlated (for the "merged from N
    /// sources" caption). `None` when uncorrelated.
    pub fn merged_source_count(&self) -> Option<usize> {
        self.entity.map(|e| e.members.len().max(self.facets.len()))
    }
}

/// Group device refs into facet-sorted, problem-first hosts (#128/#306).
///
/// Grouping key: an entity via [`EntityStore::entity_for_device`] (resolved
/// through aliases), else the bare `source`. An **empty** store reproduces the
/// pre-correlation per-source grouping exactly (degraded parity). Pure +
/// testable — feeds both the host-card grid and the fleet rollups.
pub fn aggregate<'a>(devices: &[&'a DeviceState], entities: &'a EntityStore) -> Vec<Host<'a>> {
    // Preserve deterministic ordering with a BTreeMap keyed by a sortable form.
    let mut by_host: BTreeMap<HostKey, Vec<&DeviceState>> = BTreeMap::new();
    for d in devices {
        let key = match entities.entity_id_for_device(&d.id) {
            Some(eid) => HostKey::Entity(entities.resolve_alias(eid).to_string()),
            None => HostKey::Source(d.id.source.clone()),
        };
        by_host.entry(key).or_default().push(d);
    }

    let mut hosts: Vec<Host<'a>> = by_host
        .into_iter()
        .map(|(key, mut facets)| {
            // Primary-first: identity priority, then protocol for stability.
            facets.sort_by(|a, b| {
                protocol_priority(a.id.protocol)
                    .cmp(&protocol_priority(b.id.protocol))
                    .then(a.id.protocol.cmp(&b.id.protocol))
            });
            let (display_name, entity) = match &key {
                HostKey::Entity(id) => {
                    let e = entities.hosts.get(id);
                    let name = e
                        .map(entity_display_name)
                        .unwrap_or_else(|| facets[0].id.source.clone());
                    (name, e)
                }
                HostKey::Source(source) => (source.clone(), None),
            };
            Host {
                key,
                display_name,
                entity,
                facets,
            }
        })
        .collect();

    // Problem-first ordering, then display name (matches the device grid's #34 sort).
    hosts.sort_by(|a, b| {
        status_rank(a.effective_status())
            .cmp(&status_rank(b.effective_status()))
            .then(a.display_name.cmp(&b.display_name))
    });
    hosts
}

// Ordering for HostKey so the BTreeMap groups deterministically (entities
// before sources, then lexicographic within each).
impl PartialOrd for HostKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HostKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (HostKey::Entity(a), HostKey::Entity(b)) => a.cmp(b),
            (HostKey::Source(a), HostKey::Source(b)) => a.cmp(b),
            (HostKey::Entity(_), HostKey::Source(_)) => Ordering::Less,
            (HostKey::Source(_), HostKey::Entity(_)) => Ordering::Greater,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::MemberClaim;

    fn facet(proto: Protocol, source: &str, status: DeviceStatus) -> DeviceState {
        let mut d = DeviceState::new(DeviceId::fixture(proto, source));
        d.update_from_liveness(status, 0, None);
        d.metric_count = 3;
        d
    }

    fn member(sensor: &str, source: &str) -> MemberClaim {
        MemberClaim {
            sensor: sensor.into(),
            source: source.into(),
            rule: "host_id".into(),
            confidence: 1.0,
            last_seen: 1,
        }
    }

    fn entity(id: &str, members: Vec<MemberClaim>) -> HostEntity {
        HostEntity {
            entity_id: id.into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: Some("web-01".into()),
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            members,
            status: None,
            last_updated: 1_000,
        }
    }

    #[test]
    fn groups_facets_by_source_primary_first_degraded() {
        let a = facet(Protocol::Netlink, "host1", DeviceStatus::Online);
        let b = facet(Protocol::Sysinfo, "host1", DeviceStatus::Online);
        let c = facet(Protocol::Logs, "host2", DeviceStatus::Online);
        let devices = vec![&a, &b, &c];

        // Empty store → per-source grouping, bit-for-bit as before.
        let store = EntityStore::default();
        let hosts = aggregate(&devices, &store);
        assert_eq!(hosts.len(), 2);
        let h1 = hosts.iter().find(|h| h.display_name == "host1").unwrap();
        assert_eq!(h1.facets.len(), 2);
        assert_eq!(h1.primary().id.protocol, Protocol::Sysinfo);
        assert_eq!(h1.metric_count(), 6);
        assert!(h1.entity.is_none());
        assert_eq!(h1.key, HostKey::Source("host1".into()));
    }

    #[test]
    fn merges_two_sources_under_one_entity() {
        // sysinfo on "srvA" and netlink on "srvB" both belong to one entity.
        let a = facet(Protocol::Sysinfo, "srvA", DeviceStatus::Online);
        let b = facet(Protocol::Netlink, "srvB", DeviceStatus::Online);
        let devices = vec![&a, &b];

        let mut store = EntityStore::default();
        store.upsert(entity(
            "h_web01",
            vec![member("sysinfo", "srvA"), member("netlink", "srvB")],
        ));

        let hosts = aggregate(&devices, &store);
        assert_eq!(hosts.len(), 1);
        let h = &hosts[0];
        assert_eq!(h.key, HostKey::Entity("h_web01".into()));
        assert_eq!(h.display_name, "web-01");
        assert_eq!(h.facets.len(), 2);
        assert_eq!(h.merged_source_count(), Some(2));
    }

    #[test]
    fn resolves_alias_to_current_entity() {
        let a = facet(Protocol::Sysinfo, "srvA", DeviceStatus::Online);
        let devices = vec![&a];
        let mut store = EntityStore::default();
        let mut e = entity("h_new", vec![member("sysinfo", "srvA")]);
        e.aliases = vec!["h_old".into()];
        store.upsert(e);
        let hosts = aggregate(&devices, &store);
        assert_eq!(hosts.len(), 1);
        // The device maps to h_new directly (by_device points at the current id).
        assert_eq!(hosts[0].key, HostKey::Entity("h_new".into()));
    }

    #[test]
    fn duplicated_protocols_flags_only_repeats() {
        let protos = [
            Protocol::Sysinfo,
            Protocol::Sysinfo,
            Protocol::Netlink,
            Protocol::Sysinfo,
        ];
        let dups = duplicated_protocols(protos.into_iter());
        assert_eq!(dups.len(), 1);
        assert!(dups.contains(&Protocol::Sysinfo));
        assert!(!dups.contains(&Protocol::Netlink));

        // No repeats (or nothing at all) → empty set.
        assert!(duplicated_protocols([Protocol::Snmp, Protocol::Logs].into_iter()).is_empty());
        assert!(duplicated_protocols(std::iter::empty()).is_empty());
    }

    #[test]
    fn host_status_is_worst_facet_and_sorts_problem_first() {
        let healthy = facet(Protocol::Sysinfo, "good", DeviceStatus::Online);
        let ok = facet(Protocol::Netlink, "bad", DeviceStatus::Online);
        let down = facet(Protocol::Netring, "bad", DeviceStatus::Offline);
        let devices = vec![&healthy, &ok, &down];

        let store = EntityStore::default();
        let hosts = aggregate(&devices, &store);
        assert_eq!(hosts[0].display_name, "bad");
        assert_eq!(hosts[0].effective_status(), DeviceStatus::Offline);
        assert_eq!(hosts[1].display_name, "good");
    }
}
