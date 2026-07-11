//! Force-directed layout algorithm for topology graph.
//!
//! Implements a simple force-directed layout where:
//! - Nodes repel each other (like charged particles)
//! - Edges attract connected nodes (like springs)
//! - Damping prevents oscillation

use super::{NodeId, TopologyState};

/// Configuration for the force-directed layout.
#[derive(Debug, Clone)]
pub struct LayoutConfig {
    /// Repulsion force constant (higher = stronger repulsion).
    pub repulsion: f32,
    /// Attraction force constant (higher = stronger spring force).
    pub attraction: f32,
    /// Centering force constant (pulls nodes toward origin).
    pub centering: f32,
    /// Damping factor (0-1, higher = more damping).
    pub damping: f32,
    /// Minimum distance between nodes to prevent extreme forces.
    pub min_distance: f32,
    /// Ideal distance between connected nodes.
    pub ideal_distance: f32,
    /// Maximum velocity to prevent instability.
    pub max_velocity: f32,
    /// Velocity threshold below which nodes are considered stable.
    pub stability_threshold: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            repulsion: 10000.0,       // Strong repulsion to keep nodes well apart
            attraction: 0.015,        // Moderate attraction for faster settling
            centering: 0.002,         // Weak centering - prevents drift
            damping: 0.7,             // Lower damping for faster convergence
            min_distance: 180.0,      // Minimum node separation
            ideal_distance: 350.0,    // Large target distance for spread out graph
            max_velocity: 20.0,       // Higher velocity for faster convergence
            stability_threshold: 1.5, // Stabilizes quickly
        }
    }
}

/// Run one iteration of the force-directed layout algorithm.
///
/// Returns true if the layout is stable (all velocities below threshold).
pub fn layout_step(state: &mut TopologyState, config: &LayoutConfig) -> bool {
    if !state.auto_layout || state.nodes.len() < 2 {
        return true;
    }

    // Collect node IDs for iteration
    let node_ids: Vec<NodeId> = state.nodes.keys().cloned().collect();

    // Calculate forces for each node
    let mut forces: std::collections::HashMap<NodeId, (f32, f32)> =
        std::collections::HashMap::new();

    for id in &node_ids {
        forces.insert(id.clone(), (0.0, 0.0));
    }

    // Repulsion forces between all node pairs
    for (i, id1) in node_ids.iter().enumerate() {
        for id2 in node_ids.iter().skip(i + 1) {
            let node1 = &state.nodes[id1];
            let node2 = &state.nodes[id2];

            let dx = node1.position.0 - node2.position.0;
            let dy = node1.position.1 - node2.position.1;
            let distance = (dx * dx + dy * dy).sqrt().max(config.min_distance);

            // Coulomb's law: F = k / d^2
            let force = config.repulsion / (distance * distance);

            // Normalize direction
            let fx = (dx / distance) * force;
            let fy = (dy / distance) * force;

            // Apply forces (equal and opposite)
            if let Some(f) = forces.get_mut(id1) {
                f.0 += fx;
                f.1 += fy;
            }
            if let Some(f) = forces.get_mut(id2) {
                f.0 -= fx;
                f.1 -= fy;
            }
        }
    }

    // Attraction forces along edges (spring toward ideal distance)
    for edge in &state.edges {
        if let (Some(from_node), Some(to_node)) =
            (state.nodes.get(&edge.from), state.nodes.get(&edge.to))
        {
            let dx = to_node.position.0 - from_node.position.0;
            let dy = to_node.position.1 - from_node.position.1;
            let distance = (dx * dx + dy * dy).sqrt().max(1.0);

            // Spring force toward ideal distance
            let displacement = distance - config.ideal_distance;
            let force = config.attraction * displacement;

            let fx = (dx / distance) * force;
            let fy = (dy / distance) * force;

            if let Some(f) = forces.get_mut(&edge.from) {
                f.0 += fx;
                f.1 += fy;
            }
            if let Some(f) = forces.get_mut(&edge.to) {
                f.0 -= fx;
                f.1 -= fy;
            }
        }
    }

    // Centering force - pull all nodes toward origin
    for id in &node_ids {
        if let Some(node) = state.nodes.get(id) {
            let dx = -node.position.0;
            let dy = -node.position.1;

            if let Some(f) = forces.get_mut(id) {
                f.0 += dx * config.centering;
                f.1 += dy * config.centering;
            }
        }
    }

    // Apply forces to update velocities and positions
    let mut is_stable = true;

    for id in &node_ids {
        if let Some(node) = state.nodes.get_mut(id) {
            // Skip pinned nodes
            if node.pinned {
                node.velocity = (0.0, 0.0);
                continue;
            }

            if let Some(&(fx, fy)) = forces.get(id) {
                // Update velocity with damping
                node.velocity.0 = (node.velocity.0 + fx) * config.damping;
                node.velocity.1 = (node.velocity.1 + fy) * config.damping;

                // Clamp velocity
                let speed =
                    (node.velocity.0 * node.velocity.0 + node.velocity.1 * node.velocity.1).sqrt();
                if speed > config.max_velocity {
                    let scale = config.max_velocity / speed;
                    node.velocity.0 *= scale;
                    node.velocity.1 *= scale;
                }

                // Check stability
                if speed > config.stability_threshold {
                    is_stable = false;
                }

                // Update position
                node.position.0 += node.velocity.0;
                node.position.1 += node.velocity.1;
            }
        }
    }

    // Clear cache if layout changed
    if !is_stable {
        state.cache.clear();
    }

    is_stable
}

/// Deterministic seed position for a newly discovered node (#440): an
/// FNV-1a hash of the id picks an angle (plus a small radial jitter) on a
/// ring around the origin. New nodes never stack at (0,0) — coincident
/// nodes have a zero-direction repulsion force and would never separate —
/// and, unlike the old whole-graph `arrange_in_circle` reseed, nothing else
/// moves when a node appears. Pure.
pub fn seed_position(id: &str) -> (f32, f32) {
    const RING_RADIUS: f32 = 400.0;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let angle = (hash % 3600) as f32 / 3600.0 * std::f32::consts::TAU;
    let radius = RING_RADIUS + ((hash >> 32) % 120) as f32;
    (radius * angle.cos(), radius * angle.sin())
}

/// Center the layout around the origin.
pub fn center_layout(state: &mut TopologyState) {
    if state.nodes.is_empty() {
        return;
    }

    // Calculate centroid
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let count = state.nodes.len() as f32;

    for node in state.nodes.values() {
        sum_x += node.position.0;
        sum_y += node.position.1;
    }

    let center_x = sum_x / count;
    let center_y = sum_y / count;

    // Shift all nodes to center around origin
    for node in state.nodes.values_mut() {
        node.position.0 -= center_x;
        node.position.1 -= center_y;
    }

    state.cache.clear();
}

/// Arrange nodes in a ranked grid (#394): most interesting first — alert
/// severity, then total edge rate, then label — reading left-to-right,
/// top-to-bottom. Pinned nodes keep their manual positions.
pub fn grid_positions(state: &mut TopologyState) {
    use std::collections::HashMap;

    // Total live rate per node id, from the current edge set.
    let mut rates: HashMap<&str, f64> = HashMap::new();
    for edge in &state.edges {
        let rate = edge.rate + edge.reverse_rate;
        *rates.entry(edge.from.as_str()).or_insert(0.0) += rate;
        *rates.entry(edge.to.as_str()).or_insert(0.0) += rate;
    }

    let mut ids: Vec<&NodeId> = state.nodes.keys().collect();
    ids.sort_by(|a, b| {
        let (na, nb) = (&state.nodes[*a], &state.nodes[*b]);
        // Alert severity first (Some > None, higher severity first)...
        let sev = |n: &super::Node| n.alert.map(|s| s as i8).unwrap_or(-1);
        sev(nb)
            .cmp(&sev(na))
            // ...then total live rate...
            .then_with(|| {
                let (ra, rb) = (
                    rates.get(a.as_str()).copied().unwrap_or(0.0),
                    rates.get(b.as_str()).copied().unwrap_or(0.0),
                );
                rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
            })
            // ...then stable by label.
            .then_with(|| na.label.cmp(&nb.label))
    });

    let count = ids.len();
    if count == 0 {
        return;
    }
    let columns = (count as f32).sqrt().ceil() as usize;
    const SPACING: f32 = 260.0;
    let width = (columns.saturating_sub(1)) as f32 * SPACING;
    let rows = count.div_ceil(columns);
    let height = (rows.saturating_sub(1)) as f32 * SPACING;

    let ids: Vec<NodeId> = ids.into_iter().cloned().collect();
    for (i, id) in ids.iter().enumerate() {
        let node = state.nodes.get_mut(id).expect("id from keys");
        if node.pinned {
            continue;
        }
        let (col, row) = (i % columns, i / columns);
        node.position = (
            col as f32 * SPACING - width / 2.0,
            row as f32 * SPACING - height / 2.0,
        );
        node.velocity = (0.0, 0.0);
    }

    state.cache.clear();
}

/// Arrange nodes in a circle (initial layout).
pub fn arrange_circle(state: &mut TopologyState, radius: f32) {
    let count = state.nodes.len();
    if count == 0 {
        return;
    }

    let angle_step = 2.0 * std::f32::consts::PI / count as f32;

    for (i, node) in state.nodes.values_mut().enumerate() {
        let angle = i as f32 * angle_step;
        node.position.0 = radius * angle.cos();
        node.position.1 = radius * angle.sin();
        node.velocity = (0.0, 0.0);
    }

    state.cache.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::topology::Node;

    fn create_test_node(id: &str, x: f32, y: f32) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            position: (x, y),
            ..Default::default()
        }
    }

    #[test]
    fn test_layout_step_repulsion() {
        let mut state = TopologyState::default();

        // Two nodes close together should repel
        // Use a config with no centering to test pure repulsion
        state
            .nodes
            .insert("a".to_string(), create_test_node("a", -20.0, 0.0));
        state
            .nodes
            .insert("b".to_string(), create_test_node("b", 20.0, 0.0));

        let config = LayoutConfig {
            centering: 0.0, // Disable centering for this test
            ..LayoutConfig::default()
        };
        layout_step(&mut state, &config);

        // Node A should move left (negative x) due to repulsion
        assert!(state.nodes["a"].velocity.0 < 0.0, "Node A should move left");
        // Node B should move right (positive x) due to repulsion
        assert!(
            state.nodes["b"].velocity.0 > 0.0,
            "Node B should move right"
        );
    }

    #[test]
    fn test_layout_step_pinned_node() {
        let mut state = TopologyState::default();

        let mut node_a = create_test_node("a", 0.0, 0.0);
        node_a.pinned = true;
        state.nodes.insert("a".to_string(), node_a);
        state
            .nodes
            .insert("b".to_string(), create_test_node("b", 10.0, 0.0));

        let config = LayoutConfig::default();
        layout_step(&mut state, &config);

        // Pinned node should not move
        assert_eq!(state.nodes["a"].position, (0.0, 0.0));
        assert_eq!(state.nodes["a"].velocity, (0.0, 0.0));
    }

    #[test]
    fn test_center_layout() {
        let mut state = TopologyState::default();

        state
            .nodes
            .insert("a".to_string(), create_test_node("a", 100.0, 100.0));
        state
            .nodes
            .insert("b".to_string(), create_test_node("b", 200.0, 100.0));

        center_layout(&mut state);

        // Centroid should be at origin
        let sum_x: f32 = state.nodes.values().map(|n| n.position.0).sum();
        let sum_y: f32 = state.nodes.values().map(|n| n.position.1).sum();
        assert!((sum_x).abs() < 0.001);
        assert!((sum_y).abs() < 0.001);
    }

    #[test]
    fn test_arrange_circle() {
        let mut state = TopologyState::default();

        state
            .nodes
            .insert("a".to_string(), create_test_node("a", 0.0, 0.0));
        state
            .nodes
            .insert("b".to_string(), create_test_node("b", 0.0, 0.0));
        state
            .nodes
            .insert("c".to_string(), create_test_node("c", 0.0, 0.0));
        state
            .nodes
            .insert("d".to_string(), create_test_node("d", 0.0, 0.0));

        arrange_circle(&mut state, 100.0);

        // All nodes should be at radius 100 from origin
        for node in state.nodes.values() {
            let dist =
                (node.position.0 * node.position.0 + node.position.1 * node.position.1).sqrt();
            assert!((dist - 100.0).abs() < 0.001);
        }
    }

    #[test]
    fn test_grid_positions_ranks_and_spaces() {
        use crate::view::topology::Edge;
        use zensight_common::AlertSeverity;

        let mut state = TopologyState::default();
        for id in ["quiet", "loud", "alerting"] {
            state
                .nodes
                .insert(id.to_string(), create_test_node(id, 0.0, 0.0));
        }
        state.nodes.get_mut("alerting").unwrap().alert = Some(AlertSeverity::Critical);
        state.edges.push(Edge {
            from: "loud".to_string(),
            to: "quiet".to_string(),
            rate: 9_000.0,
            ..Default::default()
        });

        grid_positions(&mut state);

        // 3 nodes → 2 columns; rank: alerting, loud, quiet. The alerting node
        // takes the first cell (top-left).
        let pos = |id: &str| state.nodes[id].position;
        assert!(pos("alerting").0 < pos("loud").0 || pos("alerting").1 < pos("loud").1);
        // All distinct positions.
        assert_ne!(pos("alerting"), pos("loud"));
        assert_ne!(pos("loud"), pos("quiet"));

        // Pinned nodes stay put.
        let mut pinned = create_test_node("pinned", 42.0, 43.0);
        pinned.pinned = true;
        state.nodes.insert("pinned".to_string(), pinned);
        grid_positions(&mut state);
        assert_eq!(state.nodes["pinned"].position, (42.0, 43.0));
    }

    #[test]
    fn test_seed_position_deterministic_and_off_origin() {
        let a = seed_position("host-a");
        assert_eq!(a, seed_position("host-a"), "same id ⇒ same seed");
        assert!(
            (a.0 * a.0 + a.1 * a.1).sqrt() > 100.0,
            "seed sits on the ring, not at the origin"
        );
        // Different ids land apart, so stacked spawns can't zero out the
        // repulsion direction.
        assert_ne!(a, seed_position("host-b"));
    }

    #[test]
    fn test_layout_disabled_when_auto_layout_off() {
        let mut state = TopologyState {
            auto_layout: false,
            ..Default::default()
        };

        state
            .nodes
            .insert("a".to_string(), create_test_node("a", 0.0, 0.0));
        state
            .nodes
            .insert("b".to_string(), create_test_node("b", 10.0, 0.0));

        let config = LayoutConfig::default();
        let stable = layout_step(&mut state, &config);

        // Should return stable immediately
        assert!(stable);
        // Velocities should be unchanged (zero)
        assert_eq!(state.nodes["a"].velocity, (0.0, 0.0));
        assert_eq!(state.nodes["b"].velocity, (0.0, 0.0));
    }
}
