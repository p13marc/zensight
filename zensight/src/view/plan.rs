//! The subscription follows the definition (#1262, design §5.7).
//!
//! The GUI used to subscribe to the whole telemetry class — `v1/*/telemetry/**`
//! — unless an operator configured a scope or focus mode narrowed it to one
//! origin. Once a producer's `views.toml` says which subjects each panel
//! reads, the GUI knows its data needs *before* the first sample: a monitor
//! that fetches everything to show a tenth of it scales with the fleet, not
//! with the screen.
//!
//! [`derived_scope`] is a **pure function** — the loaded definitions, the
//! fleet's slices, what is visible, who is alive → key expressions — and it
//! feeds `LinkConfig.scope`, whose change already restarts the stream. **No
//! script runs to decide a subscription**: the fields a panel needs are its
//! `fields` plus whatever its scripts name, which the #1259 lint extracts
//! lexically, so the set is known at build and a definition cannot starve
//! its own view.
//!
//! **Rules.** Focus mode still wins (one origin, everything); an
//! operator-configured scope still wins (an explicit decision) — both are the
//! caller's, [`crate::app::ZenSight::link_for_stream`]. The derivation
//! replaces only the empty-scope firehose default:
//!
//! - the **overview** is the union over every known producer of what its
//!   definition needs — each panel's `scope` with variables as `*`, joined to
//!   each field's declared path; a `document` panel's subject on the state
//!   class — or, for a producer with a slice and no definition, that
//!   producer's `telemetry/<producer>/**`, and the same for a producer known
//!   only from its liveliness token;
//! - the **device detail** widens to that origin's `telemetry/<producer>/**`
//!   on top of the overview's set, so a subject the slice does not declare is
//!   seen there and rendered as the #1256 finding.
//!
//! The common families — alerts, entities, incidents, health — keep their
//! own wildcards in `subscription.rs`; they are nobody's producer.
//!
//! **What narrowing costs, stated:** an undeclared subject from a *defined*
//! producer is not fetched on the overview. The GUI's honesty rule is "never
//! drop what arrives", not "fetch everything"; what a producer publishes
//! beyond its slice is the bus explorer's and the conformance judges' job.

use std::collections::{BTreeSet, HashMap};

use zenkey_fleet::SliceSet;
use zensight_common::views::{Slot, Sort, ViewSet};

use crate::view::definition::{identifiers, join_head, resolve_scope, scoped_fields};
use crate::view::family::FamilyModel;

/// What is on screen, as far as the subscription cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visible {
    /// The dashboard, the overview tabs, any fleet-wide view.
    Overview,
    /// One device's detail: its origin and producer.
    Device { origin: String, producer: String },
}

/// The inputs of the derivation.
pub struct PlanInput<'a> {
    pub visible: Visible,
    /// Definitions the fleet served, by producer. The bundled ones are
    /// consulted for every producer without a served one.
    pub served_views: &'a HashMap<String, ViewSet>,
    /// The fleet's slices; the compiled-in registry stands in per producer.
    pub slices: &'a SliceSet,
    /// Producers known from their liveliness tokens or health documents —
    /// the ones with neither slice nor definition still get `<producer>/**`.
    pub alive_producers: &'a [String],
}

/// This build's registries that are host producers — not the service
/// origins (`catalog`, `desired`), which publish no telemetry under a
/// producer chunk. Parsed once: the plan is recomputed after every update.
static HOST_REGISTRIES: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    zensight_common::registry::REGISTRIES
        .iter()
        .filter(|(name, toml)| {
            !name.starts_with('@')
                && zenkey::slice::parse_slice(toml).is_ok_and(|s| s.service_origin.is_none())
        })
        .map(|(name, _)| *name)
        .collect()
});

/// The key expressions the visible view needs, sorted and deduplicated.
pub fn derived_scope(input: &PlanInput<'_>) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    // Every producer this GUI can name: the fleet's slices, this build's
    // registries, whatever is alive.
    let mut producers: BTreeSet<String> = input
        .slices
        .slices()
        .iter()
        .filter(|s| s.service_origin.is_none())
        .map(|s| s.name.clone())
        .collect();
    producers.extend(HOST_REGISTRIES.iter().map(|n| n.to_string()));
    producers.extend(input.alive_producers.iter().cloned());

    for producer in &producers {
        let model = input
            .slices
            .get(producer)
            .map(FamilyModel::from_slice)
            .or_else(|| FamilyModel::for_producer(producer));
        let definition = input
            .served_views
            .get(producer)
            .cloned()
            .or_else(|| ViewSet::bundled(producer));
        match (model, definition) {
            (Some(model), Some(def)) => out.extend(definition_needs(&model, &def)),
            _ => {
                out.insert(format!("v1/*/telemetry/{producer}/**"));
            }
        }
    }
    if let Visible::Device { origin, producer } = &input.visible {
        out.insert(format!("v1/{origin}/telemetry/{producer}/**"));
    }
    out.into_iter().collect()
}

/// The key expressions one definition needs: per panel, the declared path of
/// every field it shows or its scripts name, variables as `*`; a document
/// panel's subject on the state class. A panel whose scope resolves to
/// nothing contributes the producer's whole tree — a definition that names a
/// family the slice lacks must not starve the view it fails to describe.
pub fn definition_needs(model: &FamilyModel, def: &ViewSet) -> BTreeSet<String> {
    let producer = &model.producer;
    let mut out = BTreeSet::new();
    for p in &def.panel {
        if p.kind == "document" {
            out.insert(format!("v1/*/state/{producer}/{}", wildcard(&p.scope)));
            continue;
        }
        if !matches!(p.kind.as_str(), "table" | "facts" | "chart") {
            continue;
        }
        let Some(resolved) = resolve_scope(model, &p.scope) else {
            out.insert(format!("v1/*/telemetry/{producer}/**"));
            continue;
        };
        let family = &model.families[resolved.family];
        let scoped = scoped_fields(family, &resolved.prefix);
        if family.open {
            // An open family's fields are the wire's; the whole subtree.
            out.insert(format!(
                "v1/*/telemetry/{producer}/{}",
                wildcard(&family.path)
            ));
            continue;
        }
        // The fields the panel shows …
        let mut wanted: BTreeSet<String> = match &p.fields {
            Some(f) => f.iter().cloned().collect(),
            None => scoped.iter().map(|(s, _)| s.clone()).collect(),
        };
        // … plus every field its scripts and its grade name.
        for (_, script) in scripts_of(p) {
            for ident in identifiers(script) {
                if let Some(field) = ident.strip_prefix("row.") {
                    wanted.insert(field.to_string());
                }
            }
        }
        if let Some(g) = &p.grade {
            wanted.insert(g.reading.clone());
            for l in [&g.warning, &g.critical] {
                if let Some(zensight_common::views::Limit::Field(f)) = l {
                    wanted.insert(f.clone());
                }
            }
            if let Some(a) = &g.absent {
                wanted.insert(a.clone());
            }
        }
        // The join partner's fields are named `head.field`.
        let join = p
            .join
            .as_ref()
            .and_then(|j| model.family(j).map(|f| (join_head(j), f)));
        for name in wanted {
            if let Some((head, partner)) = &join
                && let Some(rest) = name.strip_prefix(&format!("{head}."))
            {
                if let Some(f) = partner.field(rest) {
                    out.insert(format!("v1/*/telemetry/{producer}/{}", wildcard(&f.path)));
                } else if rest.is_empty() {
                    // `row.backup` alone: the partner's presence, any field.
                    out.insert(format!(
                        "v1/*/telemetry/{producer}/{}",
                        wildcard(&partner.path)
                    ));
                }
                continue;
            }
            if let Some((_, full)) = scoped.iter().find(|(s, _)| *s == name)
                && let Some(f) = family.field(full)
            {
                out.insert(format!("v1/*/telemetry/{producer}/{}", wildcard(&f.path)));
            }
        }
    }
    out
}

fn scripts_of(p: &zensight_common::views::Panel) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    for (name, slot) in [("label", &p.label), ("show", &p.show), ("note", &p.note)] {
        if let Some(s) = slot.as_ref().and_then(Slot::script) {
            out.push((name.to_string(), s));
        }
    }
    if let Some(Sort::Rhai { rhai }) = &p.sort {
        out.push(("sort".to_string(), rhai.as_str()));
    }
    for (field, slot) in &p.format {
        if let Some(s) = slot.script() {
            out.push((format!("format.{field}"), s));
        }
    }
    out
}

/// A subject pattern as a key expression: `{var}` → `*`, `{rest...}` → `**`.
pub fn wildcard(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|c| {
            if c.starts_with('{') && c.ends_with("...}") {
                "**"
            } else if c.starts_with('{') {
                "*"
            } else {
                c
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The overview's plan from what this build carries alone — its registries
/// and bundled definitions, nobody alive — as the `--print-subscription`
/// flag reports it (one expression per line).
pub fn bundled_plan() -> Vec<String> {
    derived_scope(&PlanInput {
        visible: Visible::Overview,
        served_views: &HashMap::new(),
        slices: &SliceSet::default(),
        alive_producers: &[],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::fake_sensor::{ORIGIN, PRODUCER, SLICE, VIEWS};

    fn fixture() -> (SliceSet, HashMap<String, ViewSet>) {
        let slices = SliceSet::from_slices(vec![zenkey::slice::parse_slice(SLICE).unwrap()]);
        let mut views = HashMap::new();
        views.insert(PRODUCER.to_string(), ViewSet::parse_toml(VIEWS).unwrap());
        (slices, views)
    }

    /// Gate 6's substance: the fixture definition needs exactly four key
    /// expressions — two temperature fields, the uplink counter, the status
    /// document — and not the firehose.
    #[test]
    fn the_fixture_definition_needs_exactly_four_expressions() {
        let (slices, views) = fixture();
        let model = FamilyModel::from_slice(slices.get(PRODUCER).unwrap());
        let needs: Vec<String> = definition_needs(&model, &views[PRODUCER])
            .into_iter()
            .collect();
        assert_eq!(
            needs,
            vec![
                "v1/*/state/fake-sensor/*/status",
                "v1/*/telemetry/fake-sensor/*/temp/*/celsius",
                "v1/*/telemetry/fake-sensor/*/temp/*/upper_critical_c",
                "v1/*/telemetry/fake-sensor/*/uplink/rx_bytes",
            ]
        );
    }

    /// The overview plan over the fixture alone, and how the device detail
    /// widens it; a producer with a slice and no definition gets its tree.
    #[test]
    fn overview_and_device_plans() {
        let (slices, views) = fixture();
        let no_registry: Vec<String> = Vec::new();
        let overview = derived_scope(&PlanInput {
            visible: Visible::Overview,
            served_views: &views,
            slices: &slices,
            alive_producers: &no_registry,
        });
        assert!(
            !overview.iter().any(|k| k == "v1/*/telemetry/**"),
            "never the firehose: {overview:?}"
        );
        assert!(overview.contains(&"v1/*/telemetry/fake-sensor/*/temp/*/celsius".to_string()));
        assert!(
            !overview
                .iter()
                .any(|k| k.starts_with("v1/*/telemetry/fake-sensor/**")),
            "a defined producer is not fetched whole on the overview"
        );
        // This build's registries ride along, each as its own tree or its
        // bundled definition's needs.
        assert!(overview.contains(&"v1/*/telemetry/sysinfo/**".to_string()));

        let device = derived_scope(&PlanInput {
            visible: Visible::Device {
                origin: ORIGIN.into(),
                producer: PRODUCER.into(),
            },
            served_views: &views,
            slices: &slices,
            alive_producers: &no_registry,
        });
        assert!(device.contains(&format!("v1/{ORIGIN}/telemetry/{PRODUCER}/**")));
        assert!(device.len() > overview.len());

        // No definition: the producer's whole tree, fleet-wide.
        let none = HashMap::new();
        let undefined = derived_scope(&PlanInput {
            visible: Visible::Overview,
            served_views: &none,
            slices: &slices,
            alive_producers: &no_registry,
        });
        assert!(undefined.contains(&"v1/*/telemetry/fake-sensor/**".to_string()));
        assert!(!undefined.iter().any(|k| k.contains("fake-sensor/*/temp")));
    }

    /// A producer known only from its liveliness token gets its tree too.
    #[test]
    fn an_alive_producer_with_nothing_else_gets_its_tree() {
        let plan = derived_scope(&PlanInput {
            visible: Visible::Overview,
            served_views: &HashMap::new(),
            slices: &SliceSet::default(),
            alive_producers: &["mystery".to_string()],
        });
        assert!(plan.contains(&"v1/*/telemetry/mystery/**".to_string()));
    }

    /// The bundled documents' needs: pve's join reaches the backup family,
    /// the grade's reading is fetched even when hidden from `fields`, and
    /// an open family is fetched whole.
    #[test]
    fn bundled_definitions_derive_their_needs() {
        let pve = FamilyModel::for_producer("pve").unwrap();
        let needs = definition_needs(&pve, &ViewSet::bundled("pve").unwrap());
        assert!(needs.contains("v1/*/telemetry/pve/backup/*/age_secs"));
        assert!(needs.contains("v1/*/telemetry/pve/guest/*/running"));
        assert!(needs.contains("v1/*/telemetry/pve/cluster/quorate"));
        let snmp = FamilyModel::for_producer("snmp").unwrap();
        let needs = definition_needs(&snmp, &ViewSet::bundled("snmp").unwrap());
        assert!(needs.iter().any(|k| k.ends_with("/**")), "{needs:?}");
        let plan = bundled_plan();
        assert!(plan.len() > 1 && !plan.contains(&"v1/*/telemetry/**".to_string()));
        // A service origin is nobody's producer.
        assert!(
            !plan
                .iter()
                .any(|k| k.contains("/catalog/") || k.contains("/desired/")),
            "{plan:?}"
        );
    }
}
