//! The view definition — `views.toml` v1 (#1259, design §6).
//!
//! A producer can say how its family model (design §5.3) is best shown: which
//! families are panels and in what order, titles, which field is graded
//! against which sibling, the row label, what is hidden. The vocabulary is
//! **closed and structural — no expressions**: anything conditional is a
//! `{ rhai = "…" }` slot returning a value (a string, a bool, a sort key),
//! evaluated by the GUI under the engine limits of §6.4. So the format never
//! grows an operator, and a script cannot name a colour, a size or a widget.
//!
//! **`grade` accepts field names only.** `reading`, `warning`, `critical` and
//! `absent` name sibling fields of the panel's family — the producer's own
//! limits — and there is no script slot for them. The one admitted exception
//! is a literal *with provenance* (`{ const = 172800, declared_by = "gui" }`):
//! a threshold the definition confesses is its own, which the renderer labels
//! as such. The doctrine *the limit is the publisher's* is enforced by the
//! absence of an API, not by review.
//!
//! **Where a definition comes from.** Bundled beside the registry
//! (`zensight-common/registry/views/<producer>.toml`), compiled into this
//! crate as [`VIEWS`], and served by the producer at `@rpc/<producer>/views`
//! (a `views` read procedure declared in its registry like `introspect`,
//! reply type `ViewSet`). The GUI prefers what the producer serves, falls
//! back to the bundled copy, and to the default renderer with neither.
//!
//! **What this crate checks.** That every bundled document parses as this
//! vocabulary and names the producer whose file it is
//! ([`bundled_documents_parse`](crate::views) test). The deeper lint — every
//! `scope` is a family of the producer's slice, every field a declared one,
//! every script compiles and names only declared fields — needs the family
//! derivation and the script engine, both of which live in the GUI
//! (`zensight::view::definition::lint`), and runs there over these same
//! documents; a sensor never links the engine.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

include!(concat!(env!("OUT_DIR"), "/zensight_views.rs"));

/// A presentation slot: a literal, or a Rhai expression returning the value
/// the slot demands (design §6.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Slot {
    /// A literal. `$var` inside it is the bound variable of the panel's
    /// family (`label = "$sensor"`).
    Literal(String),
    /// A script. The GUI evaluates it with `row`, the bound vars and `decl`
    /// in scope, and the pure host functions of §6.4.
    Rhai { rhai: String },
}

impl Slot {
    /// The script, when the slot is one.
    pub fn script(&self) -> Option<&str> {
        match self {
            Slot::Rhai { rhai } => Some(rhai),
            Slot::Literal(_) => None,
        }
    }
}

/// A limit in `[panel.grade]`: a sibling field of the panel's family — the
/// publisher's own number — or a literal that admits whose it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Limit {
    /// A sibling field name (`upper_critical_c`, `backup.age_secs`).
    Field(String),
    /// A literal with provenance: `{ const = 172800, declared_by = "gui" }`.
    /// The renderer labels a verdict from it as the GUI's, never the
    /// producer's.
    Const { r#const: f64, declared_by: String },
}

/// `[panel.grade]` — the `LimitRow` semantics (#1127): absent ≠ 0,
/// unmetered ≠ 0, no limit → no verdict. Field names only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Grade {
    /// The graded reading.
    pub reading: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<Limit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical: Option<Limit>,
    /// The field whose `false` means "no such thing in the bay" — rendered
    /// "absent", never `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent: Option<String>,
}

/// `[panel.sort]` as a field: `{ by = "celsius", dir = "desc" }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Sort {
    /// A script returning the sort key (a number or a string), ascending.
    Rhai { rhai: String },
    /// A field, with an optional direction (`asc` default).
    Field {
        by: String,
        #[serde(default)]
        dir: Option<String>,
    },
}

/// `[panel.stale]` — overrides the `ttl_s`/`rate` default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stale {
    pub after_s: i64,
}

/// `[[panel.link]]` — a jump to another view, by a bound var.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub field: String,
    pub view: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filter: BTreeMap<String, String>,
}

/// `[[panel.action]]` — a write procedure on a row, gated and audited as
/// today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub procedure: String,
    pub label: String,
}

/// One panel of a view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    /// `table` | `facts` | `document` | `chart` | `reply` | `custom`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The family (a subject pattern up to its last variable), possibly
    /// extended by literal chunks that select a field subtree
    /// (`{unit}/uplink` on the `{unit}` family's `uplink/*` fields).
    pub scope: String,
    /// A second family joined on the shared variable (left join: a row of
    /// the scope with no partner keeps its place, with the partner's fields
    /// absent). Fields of the partner are named `<partner head>.<field>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<String>,
    /// The fields shown, in order. Default: every field of the family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<String>,
    /// The row label; default the bound vars joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Slot>,
    /// A bool: hide the row when false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<Slot>,
    /// A string or `()`: a note beside the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Slot>,
    /// A `facts` panel over *every* instance of the scope at once (#1260): a
    /// script that sees `rows` — the array of row maps — and returns a map
    /// of fact → value. The fleet aggregates the overview tabs show (nodes
    /// online, "any node lost quorum") are this, not a per-row slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Slot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Sort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparkline: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grade: Option<Grade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<Stale>,
    /// Per-field display value, still typed by the declaration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub format: BTreeMap<String, Slot>,
    /// `document`: the type name in `describe`; default the subject's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// `reply`: the read procedure whose reply schema renders the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link: Vec<Link>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action: Vec<Action>,
}

/// `[view]` — the document's header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewHeader {
    /// Of this format. `"1"`.
    pub version: String,
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// One card per binding of this var, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

/// A producer's view definition — the `views` reply (design §6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewSet {
    pub view: ViewHeader,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panel: Vec<Panel>,
}

impl ViewSet {
    /// Parse a `views.toml` document. The one version this build reads is
    /// `"1"`; anything else is refused by name rather than half-read.
    pub fn parse_toml(text: &str) -> Result<Self, String> {
        let set: ViewSet = toml::from_str(text).map_err(|e| e.to_string())?;
        if set.view.version != "1" {
            return Err(format!(
                "views.toml version {:?} is not one this build reads (\"1\")",
                set.view.version
            ));
        }
        Ok(set)
    }

    /// The bundled definition for a producer, when this build ships one.
    pub fn bundled(producer: &str) -> Option<Self> {
        let (_, text) = VIEWS.iter().find(|(name, _)| *name == producer)?;
        Self::parse_toml(text).ok()
    }

    /// Every slot script in the document, with the path that names it in a
    /// message (`panel[1].label`, `panel[2].format.age_secs`).
    pub fn scripts(&self) -> Vec<(String, &str)> {
        let mut out = Vec::new();
        for (i, p) in self.panel.iter().enumerate() {
            for (name, slot) in [
                ("label", &p.label),
                ("show", &p.show),
                ("note", &p.note),
                ("aggregate", &p.aggregate),
            ] {
                if let Some(s) = slot.as_ref().and_then(Slot::script) {
                    out.push((format!("panel[{i}].{name}"), s));
                }
            }
            if let Some(Sort::Rhai { rhai }) = &p.sort {
                out.push((format!("panel[{i}].sort"), rhai.as_str()));
            }
            for (field, slot) in &p.format {
                if let Some(s) = slot.script() {
                    out.push((format!("panel[{i}].format.{field}"), s));
                }
            }
        }
        out
    }
}

/// The bundled definitions as the JSON the `views` procedure serves, built
/// once. A document that does not parse is *absent* here, not served
/// broken; the parse test below is what keeps that from ever being the case
/// in a shipped build.
static VIEWS_JSON: LazyLock<Vec<(&'static str, String)>> = LazyLock::new(|| {
    VIEWS
        .iter()
        .filter_map(|(name, text)| {
            let set = ViewSet::parse_toml(text).ok()?;
            Some((*name, serde_json::to_string(&set).ok()?))
        })
        .collect()
});

/// The `views` reply for a producer this build bundles a document for.
pub fn views_json(producer: &str) -> Option<&'static str> {
    VIEWS_JSON
        .iter()
        .find(|(name, _)| *name == producer)
        .map(|(_, json)| json.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled document parses as the vocabulary, names the producer
    /// whose file it is, and that producer's registry declares the `views`
    /// procedure that serves it — three things that can each drift alone.
    #[test]
    fn bundled_documents_parse() {
        assert!(!VIEWS.is_empty(), "no bundled views.toml found");
        for (name, text) in VIEWS {
            let set = ViewSet::parse_toml(text)
                .unwrap_or_else(|e| panic!("registry/views/{name}.toml: {e}"));
            assert_eq!(
                set.view.producer, *name,
                "registry/views/{name}.toml names producer {:?}",
                set.view.producer
            );
            assert!(
                !set.panel.is_empty(),
                "registry/views/{name}.toml declares no panel"
            );
            let slice = crate::registry::registry_toml(name)
                .unwrap_or_else(|| panic!("registry/views/{name}.toml: no registry for {name}"));
            assert!(
                slice.contains("path = \"views\""),
                "{name}'s registry does not declare the `views` procedure its bundled document is served by"
            );
            assert!(views_json(name).is_some());
        }
    }

    /// The §6.1 vocabulary round-trips: slots as literal or script, a limit
    /// as a field or a literal with provenance, and an unknown key refused.
    #[test]
    fn vocabulary_round_trips_and_refuses_unknown_keys() {
        let doc = r#"
[view]
version = "1"
producer = "x"
[[panel]]
kind = "table"
scope = "{a}/b/{c}"
label = "$c"
note = { rhai = "if row.v == () { \"none\" }" }
sort = { by = "v", dir = "desc" }
[panel.grade]
reading = "v"
critical = { const = 3.5, declared_by = "gui" }
[panel.format]
v = { rhai = "fmt_unit(row.v, \"W\")" }
"#;
        let set = ViewSet::parse_toml(doc).unwrap();
        let p = &set.panel[0];
        assert_eq!(p.label, Some(Slot::Literal("$c".into())));
        assert!(matches!(p.note, Some(Slot::Rhai { .. })));
        assert!(matches!(p.sort, Some(Sort::Field { .. })));
        assert!(matches!(
            p.grade.as_ref().unwrap().critical,
            Some(Limit::Const { declared_by: ref d, .. }) if d == "gui"
        ));
        assert_eq!(set.scripts().len(), 2);
        let json = serde_json::to_string(&set).unwrap();
        let back: ViewSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set);

        let bad = doc.replace("kind = \"table\"", "kind = \"table\"\ncolour = \"red\"");
        let err = ViewSet::parse_toml(&bad).unwrap_err();
        assert!(err.contains("colour"), "the refusal names the key: {err}");
        let v2 = doc.replace("version = \"1\"", "version = \"2\"");
        assert!(ViewSet::parse_toml(&v2).unwrap_err().contains("version"));
    }
}
