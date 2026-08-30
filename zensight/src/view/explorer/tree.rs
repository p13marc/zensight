//! Flattening the monitor's key tree into renderable rows (#748).
//!
//! Pure: `KeyTreeSnapshot` + the user's expansion overrides in,
//! `Vec<TreeRow>` out. The flatten runs in `update` when a tick or a toggle
//! arrives — never per redraw.

use std::collections::BTreeMap;
use std::time::Instant;

use zenkey_fleet::{KeyTreeSnapshot, TreeNode};

/// Chunks shallower than this render expanded unless the user collapsed
/// them; deeper ones collapsed unless expanded. Two chunks shows
/// `v1/<origin>` — the fleet at a glance — without unfolding every subject.
pub const DEFAULT_EXPAND_DEPTH: usize = 2;

/// One renderable row of the key tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    /// Full chunk path from the root (`v1/h-…/telemetry`), the toggle and
    /// selection id.
    pub path: String,
    /// The last chunk, what the row prints.
    pub label: String,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
    /// A leaf that has seen samples itself (a real key, selectable for the
    /// inspector) — inner nodes aggregate.
    pub is_key: bool,
    /// Own-count for a key; subtree aggregate for an inner node.
    pub count: u64,
    pub bytes: u64,
    pub rate_hz: f64,
    pub last_seen: Option<Instant>,
    /// Keys under this node (1 for a leaf).
    pub keys: usize,
}

/// The user's explicit expand/collapse choices, overriding the depth rule.
pub type Expansion = BTreeMap<String, bool>;

pub fn is_expanded(overrides: &Expansion, path: &str, depth: usize) -> bool {
    overrides
        .get(path)
        .copied()
        .unwrap_or(depth < DEFAULT_EXPAND_DEPTH)
}

/// Flatten the snapshot under the current expansion state.
pub fn tree_rows(snapshot: &KeyTreeSnapshot, overrides: &Expansion) -> Vec<TreeRow> {
    let mut rows = Vec::with_capacity(snapshot.keys.min(4096));
    walk(&snapshot.root, String::new(), 0, overrides, &mut rows);
    rows
}

fn walk(
    node: &TreeNode,
    prefix: String,
    depth: usize,
    overrides: &Expansion,
    rows: &mut Vec<TreeRow>,
) {
    for (chunk, child) in &node.children {
        let path = if prefix.is_empty() {
            chunk.clone()
        } else {
            format!("{prefix}/{chunk}")
        };
        let has_children = !child.children.is_empty();
        let expanded = has_children && is_expanded(overrides, &path, depth);
        let is_key = child.count > 0;
        rows.push(TreeRow {
            label: chunk.clone(),
            depth,
            has_children,
            expanded,
            is_key,
            // A key that also has children (a prefix of deeper keys) shows
            // its own numbers; a pure inner node shows the subtree's.
            count: if is_key {
                child.count
            } else {
                child.subtree_count
            },
            bytes: if is_key {
                child.bytes
            } else {
                child.subtree_bytes
            },
            rate_hz: if is_key {
                child.rate_hz
            } else {
                child.subtree_rate_hz
            },
            last_seen: if is_key {
                child.last_seen
            } else {
                child.subtree_last_seen
            },
            keys: child.subtree_keys.max(usize::from(is_key)),
            path: path.clone(),
        });
        if expanded {
            walk(child, path, depth + 1, overrides, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zenkey_fleet::{MonitorCore, StatsTable};

    fn snapshot_of(keys: &[(&str, u64)]) -> KeyTreeSnapshot {
        let mut stats = StatsTable::default();
        for (key, n) in keys {
            for _ in 0..*n {
                stats.record(key, 8, None, std::time::Instant::now(), None, None);
            }
        }
        KeyTreeSnapshot::build(&stats)
    }

    /// Depth rule: two chunks unfold by default, deeper stays folded until
    /// toggled — and a toggle overrides in both directions.
    #[test]
    fn expansion_defaults_and_overrides() {
        let snap = snapshot_of(&[
            ("v1/h-1111aaaa2222/telemetry/sysinfo/cpu", 3),
            ("v1/h-1111aaaa2222/telemetry/sysinfo/disk", 2),
        ]);
        let rows = tree_rows(&snap, &Expansion::new());
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"v1"));
        assert!(paths.contains(&"v1/h-1111aaaa2222"));
        assert!(paths.contains(&"v1/h-1111aaaa2222/telemetry"));
        // Depth 2 ("telemetry") is collapsed by default: no producer row.
        assert!(!paths.iter().any(|p| p.ends_with("/sysinfo")));

        let mut overrides = Expansion::new();
        overrides.insert("v1/h-1111aaaa2222/telemetry".into(), true);
        overrides.insert("v1/h-1111aaaa2222/telemetry/sysinfo".into(), true);
        let rows = tree_rows(&snap, &overrides);
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"v1/h-1111aaaa2222/telemetry/sysinfo/cpu"));

        // Collapse the root: one row remains.
        let mut overrides = Expansion::new();
        overrides.insert("v1".into(), false);
        let rows = tree_rows(&snap, &overrides);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].expanded);
    }

    /// Inner nodes aggregate; keys carry their own numbers.
    #[test]
    fn aggregates_vs_own_counts() {
        let snap = snapshot_of(&[
            ("v1/h-1111aaaa2222/telemetry/sysinfo/cpu", 3),
            ("v1/h-1111aaaa2222/telemetry/sysinfo/disk", 2),
        ]);
        let mut overrides = Expansion::new();
        overrides.insert("v1/h-1111aaaa2222/telemetry".into(), true);
        overrides.insert("v1/h-1111aaaa2222/telemetry/sysinfo".into(), true);
        let rows = tree_rows(&snap, &overrides);
        let by_path = |p: &str| rows.iter().find(|r| r.path == p).unwrap();
        assert_eq!(by_path("v1").count, 5);
        assert_eq!(by_path("v1").keys, 2);
        let cpu = by_path("v1/h-1111aaaa2222/telemetry/sysinfo/cpu");
        assert!(cpu.is_key);
        assert_eq!(cpu.count, 3);
        assert_eq!(cpu.keys, 1);
    }

    /// The determinism pin (#747's contract): the same ingest at the same
    /// injected clocks yields byte-identical rows across two independent
    /// folds — and eviction increments the ledger when keys exceed the
    /// bound, while the key count holds at it.
    #[test]
    fn deterministic_fold_and_honest_eviction() {
        let run = || {
            let mcore = MonitorCore::bounded(64, 3);
            let epoch = std::time::Instant::now();
            let wall = std::time::SystemTime::UNIX_EPOCH;
            for (i, key) in [
                "v1/h-1111aaaa2222/telemetry/sysinfo/cpu",
                "v1/h-1111aaaa2222/telemetry/sysinfo/disk",
                "v1/h-1111aaaa2222/telemetry/sysinfo/memory",
                "v1/h-1111aaaa2222/telemetry/sysinfo/network",
            ]
            .iter()
            .enumerate()
            {
                let row = zenkey_fleet::IngestRow {
                    key: key.to_string(),
                    payload: b"{}".to_vec(),
                    encoding: None,
                    qos: Some("sampled".into()),
                    delete: false,
                    attachment: None,
                };
                let at = epoch + std::time::Duration::from_millis(i as u64 * 10);
                mcore.ingest_at(
                    std::sync::Arc::new(crate::replay::sample_view(&row, at)),
                    None,
                    at,
                    wall + std::time::Duration::from_millis(i as u64 * 10),
                );
            }
            mcore.tick();
            let snap = mcore.tree();
            (tree_rows(&snap, &Expansion::new()), snap.keys, snap.evicted)
        };
        let (rows_a, keys_a, evicted_a) = run();
        let (rows_b, keys_b, evicted_b) = run();
        // `Instant`s differ across runs; compare everything but the clock.
        let strip = |rows: &[TreeRow]| {
            rows.iter()
                .map(|r| {
                    (
                        r.path.clone(),
                        r.depth,
                        r.count,
                        r.bytes,
                        r.keys,
                        r.expanded,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&rows_a), strip(&rows_b));
        assert_eq!((keys_a, evicted_a), (keys_b, evicted_b));
        assert_eq!(keys_a, 3, "the key bound holds");
        assert_eq!(evicted_a, 1, "what the bound refused is on the ledger");
    }
}
