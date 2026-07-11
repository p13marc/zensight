//! Tiered hierarchy layout (#442): the topology's default arrangement.
//!
//! Force-directed positions carry no meaning — the classic "hairball"
//! comprehension failure. Network tools that people actually read (UniFi,
//! Auvik, conventional network diagrams) draw a deterministic *tiered* map
//! instead, and the model already derives everything that map needs:
//!
//! - **Tier 0 — Internet**: the external aggregate pseudo-node.
//! - **Tier 1 — Infrastructure**: routers / switches / access points
//!   (asset roles, `is_router` neighbors, default-gateway targets),
//!   barycenter-ordered so each gateway sits above the subnet it serves.
//! - **Tier 2 — Hosts**: everything else with an identity, banded by /24
//!   subnet (hosts without an IPv4 land in a trailing band).
//! - **Tier 3 — Discovered**: wire-only passive nodes nothing has
//!   classified — the noise floor, visually subordinate at the bottom.
//!
//! Positions are **deterministic**: same nodes/edges in ⇒ same layout out,
//! with within-band ordering by (role, label, id) — never by rate or alert,
//! so the map doesn't shuffle between refreshes or sessions. Pinned nodes
//! are the caller's business: the layout computes a slot for every node and
//! the state only moves the unpinned ones.
//!
//! Everything here is pure: no `TopologyState`, no canvas, no clock.

use std::collections::HashMap;

use super::model::{Node, NodeId, NodeRole, Provenance, subnet24};

/// Horizontal gap between adjacent nodes inside a band.
const NODE_GAP_X: f32 = 190.0;
/// Horizontal gap between adjacent bands in a tier. Wider than a whole
/// node slot so adjacent bands' padded extents never overlap.
const BAND_GAP_X: f32 = 240.0;
/// Vertical gap between wrapped rows inside a band.
const ROW_GAP_Y: f32 = 150.0;
/// Vertical gap between tiers.
const TIER_GAP_Y: f32 = 300.0;

/// A labeled horizontal band of the tiered layout: one tier, or one subnet
/// bucket within the hosts tier. Feeds the caption/separator drawing.
#[derive(Debug, Clone, PartialEq)]
pub struct TierBand {
    /// Caption ("Internet", "Gateways & infrastructure", "192.168.1.0/24",
    /// "Other hosts", "Discovered").
    pub label: String,
    /// Top edge of the band, graph coordinates.
    pub y: f32,
    /// Horizontal extent of the band, graph coordinates.
    pub x_range: (f32, f32),
    /// Vertical extent of the band's node rows (0 for a single row), so the
    /// caption backdrop can cover wrapped rows (#443).
    pub height: f32,
}

/// The computed layout: a target position per node plus the band metadata.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TieredLayout {
    pub positions: HashMap<NodeId, (f32, f32)>,
    pub bands: Vec<TierBand>,
}

/// Which tier a node belongs to. Ordering = top-to-bottom display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Internet,
    Infrastructure,
    Hosts,
    Discovered,
}

fn tier_of(node: &Node) -> Tier {
    if node.provenance == Provenance::External {
        return Tier::Internet;
    }
    match node.role {
        NodeRole::Router | NodeRole::Switch | NodeRole::AccessPoint => Tier::Infrastructure,
        NodeRole::Unknown if node.provenance == Provenance::Passive => Tier::Discovered,
        _ => Tier::Hosts,
    }
}

/// Within-band ordering rank: infrastructure-ish first, then by kind.
/// Deliberately excludes rates/alerts so positions never shuffle.
fn role_rank(role: NodeRole) -> u8 {
    match role {
        NodeRole::Router => 0,
        NodeRole::Switch => 1,
        NodeRole::AccessPoint => 2,
        NodeRole::Host => 3,
        NodeRole::Phone => 4,
        NodeRole::Iot => 5,
        NodeRole::Internet => 6,
        NodeRole::Unknown => 7,
    }
}

/// One band's worth of sorted member ids, pre-placement.
struct BandPlan<'a> {
    label: String,
    members: Vec<&'a Node>,
}

/// Grid shape for `n` members: columns clamped to [4, 8] so big bands wrap
/// into rows and grow *down*, never into a kilometer-wide strip.
fn band_columns(n: usize) -> usize {
    (n as f32).sqrt().ceil().clamp(4.0, 8.0) as usize
}

/// Lay one tier's bands side by side, centered on x = 0. Returns the band
/// metadata (y still tier-local, caller shifts) and writes positions.
fn place_tier(
    bands: &[BandPlan<'_>],
    tier_top: f32,
    positions: &mut HashMap<NodeId, (f32, f32)>,
    out_bands: &mut Vec<TierBand>,
) -> f32 {
    // First pass: band widths, to center the whole tier.
    let widths: Vec<f32> = bands
        .iter()
        .map(|b| {
            let cols = band_columns(b.members.len()).min(b.members.len().max(1));
            (cols.saturating_sub(1)) as f32 * NODE_GAP_X
        })
        .collect();
    let tier_width: f32 =
        widths.iter().sum::<f32>() + BAND_GAP_X * (bands.len().saturating_sub(1)) as f32;

    let mut cursor = -tier_width / 2.0;
    let mut max_rows = 1usize;
    for (band, width) in bands.iter().zip(&widths) {
        let cols = band_columns(band.members.len()).min(band.members.len().max(1));
        for (i, node) in band.members.iter().enumerate() {
            let (col, row) = (i % cols, i / cols);
            positions.insert(
                node.id.clone(),
                (
                    cursor + col as f32 * NODE_GAP_X,
                    tier_top + row as f32 * ROW_GAP_Y,
                ),
            );
        }
        let rows = band.members.len().div_ceil(cols.max(1)).max(1);
        max_rows = max_rows.max(rows);
        out_bands.push(TierBand {
            label: band.label.clone(),
            y: tier_top,
            x_range: (cursor - NODE_GAP_X / 2.0, cursor + width + NODE_GAP_X / 2.0),
            height: (rows.saturating_sub(1)) as f32 * ROW_GAP_Y,
        });
        cursor += width + BAND_GAP_X;
    }
    (max_rows.saturating_sub(1)) as f32 * ROW_GAP_Y
}

/// Compute the tiered layout. Deterministic and pure: same `nodes`/`edges`
/// in, same positions out, regardless of map iteration order.
pub fn tiered_layout(nodes: &HashMap<NodeId, Node>, edges: &[super::model::Edge]) -> TieredLayout {
    let mut layout = TieredLayout::default();
    if nodes.is_empty() {
        return layout;
    }

    // Classify.
    let mut internet: Vec<&Node> = Vec::new();
    let mut infra: Vec<&Node> = Vec::new();
    let mut host_bands: HashMap<Option<String>, Vec<&Node>> = HashMap::new();
    let mut discovered: Vec<&Node> = Vec::new();
    for node in nodes.values() {
        match tier_of(node) {
            Tier::Internet => internet.push(node),
            Tier::Infrastructure => infra.push(node),
            Tier::Hosts => host_bands.entry(subnet24(node)).or_default().push(node),
            Tier::Discovered => discovered.push(node),
        }
    }

    // Deterministic within-band order: role, then label, then id.
    let by_role_label = |a: &&Node, b: &&Node| {
        role_rank(a.role)
            .cmp(&role_rank(b.role))
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.id.cmp(&b.id))
    };
    internet.sort_by(by_role_label);
    for members in host_bands.values_mut() {
        members.sort_by(by_role_label);
    }
    discovered.sort_by(by_role_label);

    // Hosts tier: subnet bands ordered by subnet string, no-IPv4 last.
    let mut subnet_keys: Vec<Option<String>> = host_bands.keys().cloned().collect();
    subnet_keys.sort_by(|a, b| match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let host_plans: Vec<BandPlan<'_>> = subnet_keys
        .iter()
        .map(|key| BandPlan {
            label: key.clone().unwrap_or_else(|| "Other hosts".to_string()),
            members: host_bands[key].clone(),
        })
        .collect();

    // Place hosts + discovered first (tier-local y = 0): the infrastructure
    // barycenter pass needs their x coordinates.
    let mut host_positions: HashMap<NodeId, (f32, f32)> = HashMap::new();
    let mut host_bands_meta: Vec<TierBand> = Vec::new();
    let hosts_height = place_tier(&host_plans, 0.0, &mut host_positions, &mut host_bands_meta);

    let mut discovered_positions: HashMap<NodeId, (f32, f32)> = HashMap::new();
    let mut discovered_meta: Vec<TierBand> = Vec::new();
    let discovered_height = if discovered.is_empty() {
        0.0
    } else {
        place_tier(
            &[BandPlan {
                label: "Discovered".to_string(),
                members: discovered,
            }],
            0.0,
            &mut discovered_positions,
            &mut discovered_meta,
        )
    };

    // Infrastructure order: one cheap barycenter pass — each gateway keys on
    // the mean x of its edge-connected hosts (the Internet aggregate counts
    // as x = 0), so it lands above the subnet it serves. Label/id tiebreak
    // keeps it deterministic.
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges {
        adjacency.entry(e.from.as_str()).or_default().push(&e.to);
        adjacency.entry(e.to.as_str()).or_default().push(&e.from);
    }
    let barycenter = |node: &Node| -> f32 {
        let neighbors = adjacency.get(node.id.as_str());
        let (mut sum, mut count) = (0.0f32, 0u32);
        for peer in neighbors.into_iter().flatten() {
            if let Some((x, _)) = host_positions.get(*peer) {
                sum += x;
                count += 1;
            } else if nodes.get(*peer).map(tier_of) == Some(Tier::Internet) {
                count += 1; // internet sits at x = 0
            }
        }
        if count > 0 { sum / count as f32 } else { 0.0 }
    };
    infra.sort_by(|a, b| {
        barycenter(a)
            .partial_cmp(&barycenter(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.id.cmp(&b.id))
    });

    // Stack the tiers top to bottom, skipping empty ones, then center the
    // whole thing on the origin.
    let mut y_cursor = 0.0f32;
    let mut place_simple = |members: Vec<&Node>, label: &str, y_cursor: &mut f32| {
        if members.is_empty() {
            return;
        }
        let plan = [BandPlan {
            label: label.to_string(),
            members,
        }];
        let height = place_tier(&plan, *y_cursor, &mut layout.positions, &mut layout.bands);
        *y_cursor += height + TIER_GAP_Y;
    };
    place_simple(internet, "Internet", &mut y_cursor);
    place_simple(infra, "Gateways & infrastructure", &mut y_cursor);
    if !host_positions.is_empty() {
        for (id, (x, y)) in host_positions {
            layout.positions.insert(id, (x, y + y_cursor));
        }
        for mut band in host_bands_meta {
            band.y += y_cursor;
            layout.bands.push(band);
        }
        y_cursor += hosts_height + TIER_GAP_Y;
    }
    if !discovered_positions.is_empty() {
        for (id, (x, y)) in discovered_positions {
            layout.positions.insert(id, (x, y + y_cursor));
        }
        for mut band in discovered_meta {
            band.y += y_cursor;
            layout.bands.push(band);
        }
        y_cursor += discovered_height + TIER_GAP_Y;
    }

    // Center vertically (the last TIER_GAP_Y is trailing slack).
    let total_height = (y_cursor - TIER_GAP_Y).max(0.0);
    let shift = total_height / 2.0;
    for pos in layout.positions.values_mut() {
        pos.1 -= shift;
    }
    for band in &mut layout.bands {
        band.y -= shift;
    }

    layout
}

/// An in-flight animated transition between two position sets (#442): the
/// canvas keeps drawing live node positions, so writing the interpolation
/// into the nodes each frame animates the whole graph without a render
/// rebuild.
#[derive(Debug, Clone)]
pub struct PositionTween {
    pub from: HashMap<NodeId, (f32, f32)>,
    pub to: HashMap<NodeId, (f32, f32)>,
    /// Epoch ms when the tween started.
    pub started_ms: i64,
    pub duration_ms: u32,
}

/// Standard ease-out cubic: fast start, gentle landing.
pub fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Interpolated positions at `now_ms`, plus whether the tween has finished.
/// Nodes present only in `to` (no recorded start) jump straight there. Pure.
pub fn tween_at(tween: &PositionTween, now_ms: i64) -> (HashMap<NodeId, (f32, f32)>, bool) {
    let elapsed = (now_ms - tween.started_ms).max(0) as f32;
    let t = (elapsed / tween.duration_ms.max(1) as f32).min(1.0);
    let k = ease_out_cubic(t);
    let mut out = HashMap::with_capacity(tween.to.len());
    for (id, &(to_x, to_y)) in &tween.to {
        let (from_x, from_y) = tween.from.get(id).copied().unwrap_or((to_x, to_y));
        out.insert(
            id.clone(),
            (from_x + (to_x - from_x) * k, from_y + (to_y - from_y) * k),
        );
    }
    (out, t >= 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::topology::model::Edge;

    fn node(id: &str, role: NodeRole, provenance: Provenance, ips: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            role,
            provenance,
            ips: ips.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            ..Default::default()
        }
    }

    /// Internet above infrastructure above hosts above discovered.
    #[test]
    fn tiers_stack_top_to_bottom() {
        let mut nodes = HashMap::new();
        for n in [
            node("@internet", NodeRole::Internet, Provenance::External, &[]),
            node("gw", NodeRole::Router, Provenance::Passive, &["10.0.0.1"]),
            node(
                "host-a",
                NodeRole::Host,
                Provenance::Monitored,
                &["10.0.0.2"],
            ),
            node("mystery", NodeRole::Unknown, Provenance::Passive, &[]),
        ] {
            nodes.insert(n.id.clone(), n);
        }
        let layout = tiered_layout(&nodes, &[]);

        let y = |id: &str| layout.positions[id].1;
        assert!(y("@internet") < y("gw"), "internet above infrastructure");
        assert!(y("gw") < y("host-a"), "infrastructure above hosts");
        assert!(y("host-a") < y("mystery"), "hosts above discovered");
        let labels: Vec<&str> = layout.bands.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Internet",
                "Gateways & infrastructure",
                "10.0.0.0/24",
                "Discovered"
            ]
        );
    }

    /// Two /24s become two disjoint bands, labeled; no-IPv4 hosts trail.
    #[test]
    fn subnet_banding_disjoint_and_labeled() {
        let mut nodes = HashMap::new();
        for n in [
            node("a1", NodeRole::Host, Provenance::Monitored, &["10.0.1.2"]),
            node("a2", NodeRole::Host, Provenance::Monitored, &["10.0.1.3"]),
            node("b1", NodeRole::Host, Provenance::Monitored, &["10.0.2.2"]),
            node("noip", NodeRole::Host, Provenance::Monitored, &[]),
        ] {
            nodes.insert(n.id.clone(), n);
        }
        let layout = tiered_layout(&nodes, &[]);

        let band = |label: &str| {
            layout
                .bands
                .iter()
                .find(|b| b.label == label)
                .unwrap_or_else(|| panic!("missing band {label}"))
        };
        let (a, b) = (band("10.0.1.0/24"), band("10.0.2.0/24"));
        assert!(
            a.x_range.1 <= b.x_range.0 || b.x_range.1 <= a.x_range.0,
            "subnet bands don't overlap: {:?} vs {:?}",
            a.x_range,
            b.x_range
        );
        // Subnet bands sort before the no-IPv4 catch-all.
        let other = band("Other hosts");
        assert!(other.x_range.0 >= b.x_range.0);
        // Members sit inside their band.
        let (x, _) = layout.positions["a1"];
        assert!(a.x_range.0 <= x && x <= a.x_range.1);
    }

    /// Same inputs ⇒ identical layout, regardless of insertion order.
    #[test]
    fn deterministic_under_insertion_order() {
        let build = |ids: &[&str]| {
            let mut nodes = HashMap::new();
            for id in ids {
                let n = node(id, NodeRole::Host, Provenance::Monitored, &["10.0.0.9"]);
                nodes.insert(n.id.clone(), n);
            }
            tiered_layout(&nodes, &[])
        };
        let forward = build(&["a", "b", "c", "d", "e"]);
        let reverse = build(&["e", "d", "c", "b", "a"]);
        assert_eq!(forward, reverse);
    }

    /// A passive router is infrastructure; a passive unknown is discovered.
    #[test]
    fn provenance_and_role_pick_the_tier() {
        let mut nodes = HashMap::new();
        for n in [
            node("gw", NodeRole::Router, Provenance::Passive, &["10.0.0.1"]),
            node("ghost", NodeRole::Unknown, Provenance::Passive, &[]),
            node("cam", NodeRole::Iot, Provenance::Passive, &["10.0.0.7"]),
        ] {
            nodes.insert(n.id.clone(), n);
        }
        let layout = tiered_layout(&nodes, &[]);
        let y = |id: &str| layout.positions[id].1;
        // Router in the infra tier (top), IoT with an identity in hosts,
        // unclassified passive at the bottom.
        assert!(y("gw") < y("cam"));
        assert!(y("cam") < y("ghost"));
    }

    /// Barycenter: each gateway lands nearer the subnet band it serves.
    #[test]
    fn barycenter_orders_gateways_over_their_subnets() {
        let mut nodes = HashMap::new();
        for n in [
            node("gw-a", NodeRole::Router, Provenance::Passive, &["10.0.1.1"]),
            node("gw-b", NodeRole::Router, Provenance::Passive, &["10.0.2.1"]),
            node("a1", NodeRole::Host, Provenance::Monitored, &["10.0.1.2"]),
            node("a2", NodeRole::Host, Provenance::Monitored, &["10.0.1.3"]),
            node("b1", NodeRole::Host, Provenance::Monitored, &["10.0.2.2"]),
            node("b2", NodeRole::Host, Provenance::Monitored, &["10.0.2.3"]),
        ] {
            nodes.insert(n.id.clone(), n);
        }
        let edges = [
            edge("a1", "gw-a"),
            edge("a2", "gw-a"),
            edge("b1", "gw-b"),
            edge("b2", "gw-b"),
        ];
        let layout = tiered_layout(&nodes, &edges);

        let x = |id: &str| layout.positions[id].0;
        let band_center = |ids: [&str; 2]| (x(ids[0]) + x(ids[1])) / 2.0;
        let (center_a, center_b) = (band_center(["a1", "a2"]), band_center(["b1", "b2"]));
        assert!(
            (x("gw-a") - center_a).abs() < (x("gw-a") - center_b).abs(),
            "gw-a sits nearer subnet A"
        );
        assert!(
            (x("gw-b") - center_b).abs() < (x("gw-b") - center_a).abs(),
            "gw-b sits nearer subnet B"
        );
    }

    /// Large bands wrap into rows: bounded width, distinct positions.
    #[test]
    fn large_band_wraps_into_rows() {
        let mut nodes = HashMap::new();
        for i in 0..30 {
            let n = node(
                &format!("host-{i:02}"),
                NodeRole::Host,
                Provenance::Monitored,
                &[&format!("10.0.0.{}", i + 10)],
            );
            nodes.insert(n.id.clone(), n);
        }
        let layout = tiered_layout(&nodes, &[]);

        let xs: Vec<f32> = layout.positions.values().map(|p| p.0).collect();
        let ys: Vec<f32> = layout.positions.values().map(|p| p.1).collect();
        let width = xs.iter().cloned().fold(f32::MIN, f32::max)
            - xs.iter().cloned().fold(f32::MAX, f32::min);
        assert!(width <= 8.0 * NODE_GAP_X, "width bounded by column clamp");
        let distinct_rows = {
            let mut rows: Vec<i64> = ys.iter().map(|y| *y as i64).collect();
            rows.sort();
            rows.dedup();
            rows.len()
        };
        assert!(distinct_rows >= 4, "30 nodes wrap over several rows");
        // All positions distinct.
        let mut seen = std::collections::HashSet::new();
        for p in layout.positions.values() {
            assert!(
                seen.insert((p.0 as i64, p.1 as i64)),
                "duplicate slot {p:?}"
            );
        }
    }

    /// Tween endpoints and monotone easing.
    #[test]
    fn tween_interpolates_and_finishes() {
        let mut from = HashMap::new();
        from.insert("a".to_string(), (0.0, 0.0));
        let mut to = HashMap::new();
        to.insert("a".to_string(), (100.0, -50.0));
        to.insert("new".to_string(), (10.0, 10.0));
        let tween = PositionTween {
            from,
            to,
            started_ms: 1_000,
            duration_ms: 400,
        };

        let (at_start, done) = tween_at(&tween, 1_000);
        assert_eq!(at_start["a"], (0.0, 0.0));
        assert!(!done);
        // A node with no recorded start jumps straight to its target.
        assert_eq!(at_start["new"], (10.0, 10.0));

        let (mid, done) = tween_at(&tween, 1_200);
        assert!(!done);
        assert!(mid["a"].0 > 0.0 && mid["a"].0 < 100.0);
        // Ease-out: more than halfway at half time.
        assert!(mid["a"].0 > 50.0);

        let (at_end, done) = tween_at(&tween, 1_400);
        assert!(done);
        assert_eq!(at_end["a"], (100.0, -50.0));

        // Past the end stays clamped.
        let (after, done) = tween_at(&tween, 9_999);
        assert!(done);
        assert_eq!(after["a"], (100.0, -50.0));
    }

    /// Empty input, single node, single tier — no panics, sane output.
    #[test]
    fn degenerate_inputs() {
        assert_eq!(tiered_layout(&HashMap::new(), &[]), TieredLayout::default());

        let mut nodes = HashMap::new();
        let n = node("only", NodeRole::Host, Provenance::Monitored, &["10.0.0.5"]);
        nodes.insert(n.id.clone(), n);
        let layout = tiered_layout(&nodes, &[]);
        assert_eq!(layout.positions.len(), 1);
        assert_eq!(layout.bands.len(), 1);
        // A single-tier layout centers on the origin.
        assert_eq!(layout.positions["only"].1, 0.0);
    }
}
