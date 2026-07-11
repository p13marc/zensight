//! Force-directed layout algorithm for topology graph.
//!
//! Implements a simple force-directed layout where:
//! - Nodes repel each other (like charged particles)
//! - Edges attract connected nodes (like springs)
//! - Damping prevents oscillation
//! - A d3-style cooling `alpha` scales the applied motion and decays every
//!   iteration (#441), so the simulation eases out and is *guaranteed* to
//!   converge even if velocities micro-oscillate above the threshold.
//!
//! The all-pairs repulsion is O(n²) with no spatial index. At the target
//! scale (LAN: tens of nodes, worst case a few hundred passive assets) a
//! step is tens of microseconds; revisit Barnes–Hut only if profiling shows
//! a step above ~1 ms.

use super::{NodeId, TopologyState};

/// Alpha below which the simulation counts as fully cooled (#441).
pub const ALPHA_MIN: f32 = 0.02;
/// Per-iteration alpha decay (#441): ~150 iterations from 1.0 to
/// [`ALPHA_MIN`], i.e. a couple of seconds of smooth motion at the frame
/// cadence [`super::TopologyState::run_layout_step`] drives.
pub const ALPHA_DECAY: f32 = 0.975;

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
/// Returns true if the layout is stable: every velocity below the threshold,
/// or the cooling alpha fully decayed (#441).
pub fn layout_step(state: &mut TopologyState, config: &LayoutConfig) -> bool {
    if !state.auto_layout || state.nodes.len() < 2 {
        return true;
    }
    let alpha = state.layout_alpha;
    if alpha < ALPHA_MIN {
        return true;
    }

    // Snapshot positions into index-addressable buffers (#441): the O(n²)
    // pair loop below runs on plain Vecs — no String clones and no HashMap
    // lookups in the hot path.
    let n = state.nodes.len();
    let mut pos: Vec<(f32, f32)> = Vec::with_capacity(n);
    let mut index: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(n);
    for (i, (id, node)) in state.nodes.iter().enumerate() {
        index.insert(id.as_str(), i);
        pos.push(node.position);
    }
    let mut force: Vec<(f32, f32)> = vec![(0.0, 0.0); n];

    // Repulsion forces between all node pairs (Coulomb's law: F = k / d²).
    for i in 0..n {
        for j in (i + 1)..n {
            let dx = pos[i].0 - pos[j].0;
            let dy = pos[i].1 - pos[j].1;
            let distance = (dx * dx + dy * dy).sqrt().max(config.min_distance);
            let f = config.repulsion / (distance * distance);
            let fx = (dx / distance) * f;
            let fy = (dy / distance) * f;
            force[i].0 += fx;
            force[i].1 += fy;
            force[j].0 -= fx;
            force[j].1 -= fy;
        }
    }

    // Attraction forces along edges (spring toward ideal distance).
    for edge in &state.edges {
        let (Some(&i), Some(&j)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            continue;
        };
        let dx = pos[j].0 - pos[i].0;
        let dy = pos[j].1 - pos[i].1;
        let distance = (dx * dx + dy * dy).sqrt().max(1.0);
        let displacement = distance - config.ideal_distance;
        let f = config.attraction * displacement;
        let fx = (dx / distance) * f;
        let fy = (dy / distance) * f;
        force[i].0 += fx;
        force[i].1 += fy;
        force[j].0 -= fx;
        force[j].1 -= fy;
    }

    // Centering force — pull all nodes toward origin.
    for i in 0..n {
        force[i].0 -= pos[i].0 * config.centering;
        force[i].1 -= pos[i].1 * config.centering;
    }

    drop(index); // release the borrow of the node keys before writing back

    // Integrate: damped velocities, applied motion scaled by the cooling
    // alpha. Write-back iterates the same (unmodified) map as the snapshot,
    // so the enumeration order matches.
    let mut is_stable = true;
    for (i, node) in state.nodes.values_mut().enumerate() {
        if node.pinned {
            node.velocity = (0.0, 0.0);
            continue;
        }
        let (fx, fy) = force[i];
        node.velocity.0 = (node.velocity.0 + fx) * config.damping;
        node.velocity.1 = (node.velocity.1 + fy) * config.damping;

        let speed = (node.velocity.0 * node.velocity.0 + node.velocity.1 * node.velocity.1).sqrt();
        if speed > config.max_velocity {
            let scale = config.max_velocity / speed;
            node.velocity.0 *= scale;
            node.velocity.1 *= scale;
        }
        if speed > config.stability_threshold {
            is_stable = false;
        }

        node.position.0 += node.velocity.0 * alpha;
        node.position.1 += node.velocity.1 * alpha;
    }

    // Cool down (#441): the decay guarantees convergence and gives the
    // settle a natural ease-out.
    state.layout_alpha = alpha * ALPHA_DECAY;

    // Positions moved: redraw.
    state.cache.clear();

    is_stable || state.layout_alpha < ALPHA_MIN
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
    fn test_layout_step_converges_within_alpha_budget() {
        use crate::view::topology::Edge;

        // A small connected graph settles: either velocities drop below the
        // threshold or the cooling alpha fully decays — never an endless
        // micro-oscillation (#441).
        let mut state = TopologyState::default();
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            state
                .nodes
                .insert(id.to_string(), create_test_node(id, i as f32 * 10.0, 0.0));
        }
        state.edges.push(Edge {
            from: "a".to_string(),
            to: "b".to_string(),
            ..Default::default()
        });
        state.edges.push(Edge {
            from: "c".to_string(),
            to: "d".to_string(),
            ..Default::default()
        });

        let config = LayoutConfig::default();
        let max_iterations = 400; // alpha decays past ALPHA_MIN well before this
        let converged = (0..max_iterations).any(|_| layout_step(&mut state, &config));
        assert!(converged, "layout must converge within the alpha budget");
        // Once stable, further steps stay stable (alpha floor).
        assert!(layout_step(&mut state, &config));
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
