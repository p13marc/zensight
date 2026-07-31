//! Rerun-free host-topology model (docs/plans/rerun/09-topology.md).
//!
//! Folds the inputs the adapter already consumes — `HostEntity` docs and
//! normalized link/peer events — into a node+edge graph that `rerun_sink.rs`
//! renders with `GraphNodes`/`GraphEdges`. NOT a replacement for the Iced
//! topology view (epic #395): this is the time-scrubbable replay supplement.

use std::collections::BTreeMap;

use zensight_common::entity::HostEntity;

use crate::events::{EventKind, NormalizedEvent};

/// What a node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A correlated host (from a `HostEntity`).
    Host,
    /// A remote peer observed in link events (never correlated).
    Peer,
}

/// Node health for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeStatus {
    #[default]
    Unknown,
    Up,
    Degraded,
    Down,
}

/// One graph node.
#[derive(Debug, Clone, PartialEq)]
pub struct TopoNode {
    /// Stable graph id (entity id for hosts, `peer:<addr>` for peers).
    pub id: String,
    /// Display label (hostname / peer address).
    pub label: String,
    pub kind: NodeKind,
    pub status: NodeStatus,
}

/// One graph edge (host → peer link).
#[derive(Debug, Clone, PartialEq)]
pub struct TopoEdge {
    pub from: String,
    pub to: String,
    /// Link state (peer_up/peer_down events flip this).
    pub up: bool,
}

/// The current graph state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Topology {
    pub nodes: Vec<TopoNode>,
    pub edges: Vec<TopoEdge>,
}

/// Folds entities + events into a [`Topology`] and reports changes.
#[derive(Debug, Default)]
pub struct TopologyBuilder {
    /// entity_id → host node state.
    hosts: BTreeMap<String, TopoNode>,
    /// (host node id, peer id) → link up?
    links: BTreeMap<(String, String), bool>,
}

impl TopologyBuilder {
    /// Ingest one entity doc. Returns true when the graph changed.
    pub fn upsert_entity(&mut self, entity: &HostEntity) -> bool {
        let status = match entity.status.as_deref() {
            Some("online") => NodeStatus::Up,
            Some("degraded") => NodeStatus::Degraded,
            Some("offline") => NodeStatus::Down,
            _ => NodeStatus::Unknown,
        };
        let node = TopoNode {
            id: entity.entity_id.clone(),
            label: entity
                .hostname
                .clone()
                .or_else(|| entity.fqdn.clone())
                .unwrap_or_else(|| entity.entity_id.clone()),
            kind: NodeKind::Host,
            status,
        };
        let changed = self.hosts.get(&entity.entity_id) != Some(&node);
        self.hosts.insert(entity.entity_id.clone(), node);
        changed
    }

    /// Ingest one normalized event. Peer up/down (and link degradation)
    /// events with a `target` create/flip the host→peer link. Returns true
    /// when the graph changed.
    pub fn apply_event(&mut self, event: &NormalizedEvent) -> bool {
        let up = match event.kind {
            EventKind::PeerUp => true,
            EventKind::PeerDown | EventKind::LinkDegraded => false,
            _ => return false,
        };
        let Some(target) = event.target.as_deref() else {
            return false;
        };
        // The local endpoint: the correlated host when known, else a
        // source-shaped standalone node id.
        let from = event
            .entity_id
            .clone()
            .unwrap_or_else(|| format!("src:{}", event.source));
        let key = (from, target.to_string());
        let changed = self.links.get(&key) != Some(&up);
        self.links.insert(key, up);
        changed
    }

    /// Materialize the current graph (hosts + peers + links, sorted stably).
    pub fn build(&self) -> Topology {
        let mut nodes: Vec<TopoNode> = self.hosts.values().cloned().collect();

        // Ensure every link endpoint exists as a node.
        let mut extras: BTreeMap<String, TopoNode> = BTreeMap::new();
        for ((from, to), up) in &self.links {
            for (id, kind) in [(from, NodeKind::Host), (to, NodeKind::Peer)] {
                if self.hosts.contains_key(id) || extras.contains_key(id) {
                    continue;
                }
                extras.insert(
                    id.clone(),
                    TopoNode {
                        id: id.clone(),
                        label: id
                            .strip_prefix("peer:")
                            .or_else(|| id.strip_prefix("src:"))
                            .unwrap_or(id)
                            .to_string(),
                        kind,
                        status: if *up {
                            NodeStatus::Up
                        } else {
                            NodeStatus::Down
                        },
                    },
                );
            }
        }
        nodes.extend(extras.into_values());

        let edges = self
            .links
            .iter()
            .map(|((from, to), up)| TopoEdge {
                from: from.clone(),
                to: to.clone(),
                up: *up,
            })
            .collect();

        Topology { nodes, edges }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventSeverity;

    fn entity(id: &str, hostname: &str, status: &str) -> HostEntity {
        HostEntity {
            entity_id: id.into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: Some(hostname.into()),
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            members: vec![],
            status: Some(status.into()),
            last_updated: 1,
        }
    }

    fn peer_event(kind: EventKind, target: &str, entity_id: Option<&str>) -> NormalizedEvent {
        let mut e = NormalizedEvent::new(1_000, kind, EventSeverity::Info, "host1", "link event");
        e.target = Some(target.into());
        e.entity_id = entity_id.map(String::from);
        e
    }

    #[test]
    fn entities_become_host_nodes_and_report_change() {
        let mut b = TopologyBuilder::default();
        assert!(b.upsert_entity(&entity("h_aa", "alpha", "online")));
        // Same doc again: no change.
        assert!(!b.upsert_entity(&entity("h_aa", "alpha", "online")));
        // Status flip: change.
        assert!(b.upsert_entity(&entity("h_aa", "alpha", "degraded")));
        let topo = b.build();
        assert_eq!(topo.nodes.len(), 1);
        assert_eq!(topo.nodes[0].status, NodeStatus::Degraded);
        assert_eq!(topo.nodes[0].label, "alpha");
    }

    #[test]
    fn peer_events_create_and_flip_links() {
        let mut b = TopologyBuilder::default();
        b.upsert_entity(&entity("h_aa", "alpha", "online"));

        assert!(b.apply_event(&peer_event(EventKind::PeerUp, "10.0.0.7", Some("h_aa"))));
        let topo = b.build();
        assert_eq!(topo.nodes.len(), 2); // host + auto-created peer node
        assert_eq!(topo.edges.len(), 1);
        assert!(topo.edges[0].up);

        // Down flips the same edge (change), repeat is a no-op.
        assert!(b.apply_event(&peer_event(EventKind::PeerDown, "10.0.0.7", Some("h_aa"))));
        assert!(!b.apply_event(&peer_event(EventKind::PeerDown, "10.0.0.7", Some("h_aa"))));
        let topo = b.build();
        assert_eq!(topo.edges.len(), 1);
        assert!(!topo.edges[0].up);

        // Non-link events never touch the graph.
        assert!(!b.apply_event(&peer_event(EventKind::RouteChange, "x", None)));
    }

    #[test]
    fn uncorrelated_sources_get_standalone_nodes() {
        let mut b = TopologyBuilder::default();
        assert!(b.apply_event(&peer_event(EventKind::PeerUp, "10.0.0.9", None)));
        let topo = b.build();
        let ids: Vec<_> = topo.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"src:host1"));
        assert!(ids.contains(&"10.0.0.9"));
    }
}
