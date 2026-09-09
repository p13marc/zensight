//! Every shipped producer config declares a memory budget (#1091).
//!
//! `docs/ops/SIZING.md` tells the operator to set `budget_rss_mb` on every
//! sensor. Before #1091 only two of sixteen had a field to set it in, and the
//! other fourteen accepted the key and discarded it — nothing in this tree
//! sets `deny_unknown_fields`, so the omission was silent in both directions.
//!
//! This is a raw-tree assertion on purpose. A typed parse would deserialise a
//! file that had lost the key back into `None` and pass, which is exactly the
//! failure it is here to catch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Configs that are NOT a `SensorConfig` and so have no budget to declare.
///
/// The five daemons here run no `SensorRunner`: they publish no health
/// document, collect no `self_stats`, and construct no `MemoryGovernor`, so a
/// `resources` block would parse and then be read by nothing. Giving them a
/// budget means giving them the health plane first — filed separately rather
/// than faked here. The `router-*` files are zenohd's own configuration and
/// are not ZenSight configs at all.
const NOT_A_SENSOR: &[&str] = &[
    "correlator.json5",
    "desired.json5",
    "otel-exporter.json5",
    "prometheus-exporter.json5",
    "rerun.json5",
    "router-blob-storage.json5",
    "router-events-storage.json5",
    "router-evidence-storage.json5",
    "router-pdns-influxdb-storage.json5",
];

fn configs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("configs")
}

/// `resources.budget_rss_mb`, read off the raw tree.
fn declared_budget(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).expect("readable config");
    let v: serde_json::Value = json5::from_str(&raw).expect("parses as JSON5");
    v.get("resources")?.get("budget_rss_mb")?.as_u64()
}

#[test]
fn every_shipped_sensor_config_declares_a_budget() {
    let mut missing = Vec::new();
    let mut found: BTreeMap<String, u64> = BTreeMap::new();

    for entry in std::fs::read_dir(configs_dir()).expect("configs/ exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json5") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 filename")
            .to_string();
        if NOT_A_SENSOR.contains(&name.as_str()) {
            continue;
        }
        match declared_budget(&path) {
            Some(mb) => {
                found.insert(name, mb);
            }
            None => missing.push(name),
        }
    }

    assert!(
        missing.is_empty(),
        "these shipped sensor configs declare no resources.budget_rss_mb, so they arm no \
         sensor-budget alert and no shed ladder — the state that ends in an OOM kill: {missing:?}"
    );

    // Not an incidental count: if a sensor is added and its config is not,
    // this is what notices. Sixteen `SensorConfig` implementors, plus
    // syslog.json5 — the logs sensor's second, network-listener config.
    assert_eq!(
        found.len(),
        17,
        "expected 17 shipped sensor configs with a budget, found {}: {found:?}",
        found.len()
    );

    // A budget of zero is not a declaration, it is a permanently saturated
    // ladder. The governor's futility guard would latch at boot.
    for (name, mb) in &found {
        assert!(*mb > 0, "{name} declares budget_rss_mb: 0");
    }
}
