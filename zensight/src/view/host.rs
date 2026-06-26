//! Physical-host aggregate (#128) — the keystone re-key of the dashboard.
//!
//! `DeviceId = (protocol, source)`, so one physical host running
//! sysinfo+netlink+logs+netring is four [`DeviceState`]s. A [`Host`] groups those
//! per-protocol **facets** by `source` so the dashboard renders **one card per
//! host** with merged identity, a composite health score, and per-facet badges.
//! `DeviceId` stays the facet key internally and on the wire — this is a pure
//! view-model over the existing device map (the topology `update_from_devices`
//! merge is the template).

use std::collections::BTreeMap;

use zensight_common::{DeviceStatus, Protocol};

use crate::view::dashboard::{DeviceState, status_rank};
use crate::view::health::{self, HealthScore};

/// A physical host: all per-protocol facet [`DeviceState`]s that share a
/// `source`, facet-sorted by identity priority.
#[derive(Debug)]
pub struct Host<'a> {
    pub source: &'a str,
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
        Protocol::Syslog => 3,
        _ => 4,
    }
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
}

/// Group device refs by `source` into facet-sorted, problem-first hosts (#128).
/// Pure + testable — feeds both the host-card grid and the fleet rollups.
pub fn aggregate<'a>(devices: &[&'a DeviceState]) -> Vec<Host<'a>> {
    let mut by_source: BTreeMap<&str, Vec<&DeviceState>> = BTreeMap::new();
    for d in devices {
        by_source.entry(d.id.source.as_str()).or_default().push(d);
    }

    let mut hosts: Vec<Host<'a>> = by_source
        .into_iter()
        .map(|(source, mut facets)| {
            // Primary-first: identity priority, then protocol for stability.
            facets.sort_by(|a, b| {
                protocol_priority(a.id.protocol)
                    .cmp(&protocol_priority(b.id.protocol))
                    .then(a.id.protocol.cmp(&b.id.protocol))
            });
            Host { source, facets }
        })
        .collect();

    // Problem-first ordering, then host name (matches the device grid's #34 sort).
    hosts.sort_by(|a, b| {
        status_rank(a.effective_status())
            .cmp(&status_rank(b.effective_status()))
            .then(a.source.cmp(b.source))
    });
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;

    fn facet(proto: Protocol, source: &str, status: DeviceStatus) -> DeviceState {
        let mut d = DeviceState::new(DeviceId::new(proto, source));
        d.update_from_liveness(status, 0, None);
        d.metric_count = 3;
        d
    }

    #[test]
    fn groups_facets_by_source_primary_first() {
        let a = facet(Protocol::Netlink, "host1", DeviceStatus::Online);
        let b = facet(Protocol::Sysinfo, "host1", DeviceStatus::Online);
        let c = facet(Protocol::Syslog, "host2", DeviceStatus::Online);
        let devices = vec![&a, &b, &c];

        let hosts = aggregate(&devices);
        assert_eq!(hosts.len(), 2);
        let h1 = &hosts.iter().find(|h| h.source == "host1").unwrap();
        // Two facets, sysinfo primary (priority 0 < netlink 1).
        assert_eq!(h1.facets.len(), 2);
        assert_eq!(h1.primary().id.protocol, Protocol::Sysinfo);
        assert_eq!(h1.metric_count(), 6); // 3 + 3
    }

    #[test]
    fn host_status_is_worst_facet_and_sorts_problem_first() {
        let healthy = facet(Protocol::Sysinfo, "good", DeviceStatus::Online);
        let ok = facet(Protocol::Netlink, "bad", DeviceStatus::Online);
        let down = facet(Protocol::Netring, "bad", DeviceStatus::Offline);
        let devices = vec![&healthy, &ok, &down];

        let hosts = aggregate(&devices);
        // "bad" host has an offline facet → Offline, sorts before the online "good".
        assert_eq!(hosts[0].source, "bad");
        assert_eq!(hosts[0].effective_status(), DeviceStatus::Offline);
        assert_eq!(hosts[1].source, "good");
    }
}
