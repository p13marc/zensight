//! The family model (#1257, design §5.3): rows and columns derived from a
//! producer's registry slice, not written by hand.
//!
//! A producer's `introspect` reply declares its telemetry subjects as paths
//! with variables — `{chassis}/thermal/{sensor}/celsius`,
//! `{chassis}/thermal/{sensor}/upper_critical_c`, `cluster/quorate`. Three
//! views this year folded those by hand into rows (`specialized/bmc.rs`'s
//! `fold`, `overview/pve.rs`'s `backup_rows`, `specialized/probe.rs`'s
//! `target_rows`): group the subjects by their variable bindings, read the
//! siblings as columns. This module is that fold, once, for any slice:
//!
//! - a **family** is the longest common prefix of paths that share the same
//!   variables, ending at the last variable — `{chassis}/thermal/{sensor}` —
//!   or, for a path with no variable, everything but its last chunk
//!   (`cluster/quorate` → `cluster`);
//! - its **fields** are the literal tails — `celsius`, `upper_critical_c`;
//! - its **instances** are the distinct variable bindings seen in a device's
//!   live keys — `rack7/inlet`, `rack7/outlet`.
//!
//! Each field carries its `SubjectDecl`: `kind` decides the presentation
//! (a counter is a rate, a gauge a reading, a bool a state, text a label),
//! `unit` the formatter (`By` on a counter is `By/s`), `cardinality` table
//! against top-N, `ttl_s`/`rate` the staleness rule, `description` the
//! tooltip. Documents (`class = state`) are modelled by their schema through
//! the intake (#1256), not here.
//!
//! Nothing in this module touches Iced. The renderers (#1258) read it; the
//! ratchet in `app::system_view_tests` pins it at gate 2.
//!
//! **The binder is private, deliberately temporary.** Binding a live tail to
//! a declared subject is `RegistrySlice::bind(class, tail)` in zenkey #460,
//! which has not landed; this file carries the smallest copy that works
//! (`SubjectPattern::matches` under `best_match` precedence) and nothing
//! else. When #460 ships, [`FamilyModel::bind`] delegates to it and the
//! `patterns` table goes — do not let the two live side by side (#1153's
//! lesson about second copies).

use std::collections::BTreeMap;

use zenkey::Class;
use zenkey::pattern::{PatternChunk, SubjectPattern};
use zenkey::slice::{RateClass, RegistrySlice, SubjectDecl, SubjectKind};
use zensight_common::{TelemetryPoint, TelemetryValue};

/// How a field is presented, decided by its declared `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    /// A counter: the derivative is the reading, in `<unit>/s`.
    Rate,
    /// A gauge: the value as read, in its unit.
    Absolute,
    /// A bool: state text, never a number.
    State,
    /// Text: a label.
    Label,
    /// The slice declared no kind, or one this build does not know.
    Unknown,
}

/// One declared column of a family.
#[derive(Debug, Clone)]
pub struct Field {
    /// The literal tail after the family prefix (`celsius`,
    /// `uplink/rx_bytes`). Unique within its family.
    pub name: String,
    /// The declared path, as the slice spells it.
    pub path: String,
    pub kind: Option<SubjectKind>,
    pub unit: Option<String>,
    pub cardinality: Option<i64>,
    pub ttl_s: Option<i64>,
    pub rate: Option<RateClass>,
    pub description: Option<String>,
}

impl Field {
    fn from_decl(name: String, decl: &SubjectDecl) -> Self {
        Self {
            name,
            path: decl.path.clone(),
            kind: decl.kind.as_ref().and_then(|k| k.known().copied()),
            unit: decl.unit.clone(),
            cardinality: decl.cardinality,
            ttl_s: decl.ttl_s,
            rate: decl.rate.clone(),
            description: decl.description.clone(),
        }
    }

    /// The presentation the declared kind implies.
    pub fn presentation(&self) -> Presentation {
        match self.kind {
            Some(SubjectKind::Counter) => Presentation::Rate,
            Some(SubjectKind::Gauge) => Presentation::Absolute,
            Some(SubjectKind::Bool) => Presentation::State,
            Some(SubjectKind::Text) => Presentation::Label,
            None => Presentation::Unknown,
        }
    }

    /// The unit the *presented* value carries: a counter's declared unit
    /// per second (`By` → `By/s`, an undeclared unit → `/s`), everything
    /// else its declared unit.
    pub fn display_unit(&self) -> Option<String> {
        match (self.presentation(), &self.unit) {
            (Presentation::Rate, Some(u)) => Some(format!("{u}/s")),
            (Presentation::Rate, None) => Some("/s".to_string()),
            (_, unit) => unit.clone(),
        }
    }

    /// Stale after twice the declared `ttl_s`, when one is declared — the
    /// rule the design names instead of a per-view constant. `None` when the
    /// slice declares no TTL: then nothing here can say "stale", and a
    /// renderer must not invent a number.
    pub fn stale_after_secs(&self) -> Option<i64> {
        self.ttl_s.map(|t| t.saturating_mul(2))
    }
}

/// One family: a prefix, the variables in it, and its fields.
#[derive(Debug, Clone)]
pub struct Family {
    /// The family prefix as a pattern (`{chassis}/thermal/{sensor}`,
    /// `cluster`, `{unit}`). Empty for a single-chunk var-less path.
    pub path: String,
    /// The variable names, in path order. Empty for a facts family.
    pub vars: Vec<String>,
    /// In declaration order.
    pub fields: Vec<Field>,
    /// The family ends in a rest variable (`{metric...}`): the field set is
    /// open, and every live tail binds to one field whose name is whatever
    /// the rest bound to. The proxy producers (`snmp`, `modbus`, `gnmi`,
    /// `netflow`) declare their device tree this way, by design (#468).
    pub open: bool,
}

impl Family {
    /// Whether this family has instances (variables) or is a flat set of
    /// facts.
    pub fn is_table(&self) -> bool {
        !self.vars.is_empty()
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// A live tail bound to its family and field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub family: usize,
    pub field: usize,
    /// `(var, value)` in the family's variable order.
    pub bindings: Vec<(String, String)>,
    /// For an open family: what the rest variable bound to, which names the
    /// field. `None` for a closed one.
    pub rest: Option<String>,
}

impl Binding {
    /// The instance id: the bound values joined with `/` (`rack7/inlet`).
    /// Empty for a facts family.
    pub fn instance_id(&self) -> String {
        self.bindings
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// One instance of a family: a binding of its variables, and the latest
/// point per field.
#[derive(Debug, Clone, Default)]
pub struct Instance {
    pub id: String,
    pub bindings: Vec<(String, String)>,
    /// Keyed by field name (the rest binding, for an open family).
    pub values: BTreeMap<String, TelemetryPoint>,
}

impl Instance {
    pub fn point(&self, field: &str) -> Option<&TelemetryPoint> {
        self.values.get(field)
    }

    /// A numeric reading, when the field holds one.
    pub fn number(&self, field: &str) -> Option<f64> {
        self.values.get(field).and_then(|p| numeric(&p.value))
    }

    pub fn state(&self, field: &str) -> Option<bool> {
        self.values.get(field).and_then(|p| match &p.value {
            TelemetryValue::Boolean(b) => Some(*b),
            TelemetryValue::Gauge(v) => Some(*v != 0.0),
            TelemetryValue::Counter(v) => Some(*v != 0),
            _ => None,
        })
    }
}

/// The instances of one family, sorted by instance id.
#[derive(Debug, Clone)]
pub struct FamilyInstances {
    pub family: usize,
    pub instances: Vec<Instance>,
}

/// One declared procedure of a slice (#1261): what the GUI needs to offer
/// a call and to name the reply's type.
#[derive(Debug, Clone, PartialEq)]
pub struct Procedure {
    pub path: String,
    /// `kind = read` (or undeclared, which the grammar reads as read).
    pub read: bool,
    pub reply: Option<String>,
    pub request: Option<String>,
    pub description: Option<String>,
}

/// A producer's telemetry, modelled from its slice.
#[derive(Debug, Clone)]
pub struct FamilyModel {
    pub producer: String,
    pub families: Vec<Family>,
    /// The procedures the slice declares (#1261), so a view can offer a
    /// read call without a message per procedure. Declaration order.
    pub procedures: Vec<Procedure>,
    /// The private binder (see the module doc): one pattern per declared
    /// telemetry subject, with the family and field it belongs to.
    patterns: Vec<(SubjectPattern, usize, usize)>,
}

impl FamilyModel {
    /// Derive the model from a slice. Only `class = telemetry` subjects
    /// take part; state documents are the intake's (#1256).
    pub fn from_slice(slice: &RegistrySlice) -> Self {
        let mut families: Vec<Family> = Vec::new();
        let mut patterns = Vec::new();
        for decl in slice
            .subjects
            .iter()
            .filter(|d| d.class.is(&Class::Telemetry))
        {
            let Ok(pattern) = SubjectPattern::parse(&decl.path) else {
                continue;
            };
            let (prefix, vars, tail, open) = split(pattern.chunks());
            let family = match families.iter().position(|f| f.path == prefix) {
                Some(i) => i,
                None => {
                    families.push(Family {
                        path: prefix,
                        vars,
                        fields: Vec::new(),
                        open,
                    });
                    families.len() - 1
                }
            };
            let fam = &mut families[family];
            if fam.fields.iter().any(|f| f.name == tail) {
                continue;
            }
            fam.fields.push(Field::from_decl(tail, decl));
            patterns.push((pattern, family, fam.fields.len() - 1));
        }
        let procedures = slice
            .procedures
            .iter()
            .map(|p| Procedure {
                path: p.path.clone(),
                read: match &p.kind {
                    Some(zenkey::slice::Declared::Known(k)) => {
                        matches!(k, zenkey::slice::ProcedureKind::Read)
                    }
                    Some(zenkey::slice::Declared::Other(_)) => false,
                    None => true,
                },
                reply: p.reply.clone(),
                request: p.request.clone(),
                description: p.description.clone(),
            })
            .collect();
        Self {
            producer: slice.name.clone(),
            families,
            procedures,
            patterns,
        }
    }

    /// The read procedures a view can call with no request body: every
    /// `kind = read` declaration without a `request` type, the framework's
    /// own (`introspect`, `describe`, `views`, `artifact/*`) excluded — those
    /// the GUI already calls for itself.
    pub fn callable(&self) -> impl Iterator<Item = &Procedure> {
        self.procedures.iter().filter(|p| {
            p.read
                && p.request.is_none()
                && !matches!(p.path.as_str(), "introspect" | "describe" | "views")
                && !p.path.starts_with("artifact/")
        })
    }

    /// The compiled-in slice for a producer this build knows, when it has one.
    pub fn for_producer(producer: &str) -> Option<Self> {
        let toml = zensight_common::registry::registry_toml(producer)?;
        let slice = zenkey::slice::parse_slice(toml).ok()?;
        Some(Self::from_slice(&slice))
    }

    pub fn family(&self, path: &str) -> Option<&Family> {
        self.families.iter().find(|f| f.path == path)
    }

    /// Bind a live subject tail (`rack7/temp/inlet/celsius`) to its family
    /// and field. `None` when the slice declares nothing it matches — the
    /// intake's "not declared" (#1256).
    ///
    /// Precedence is zenkey's: literals before variables before rest
    /// variables, so `targets/total` binds to `targets/total` and not to
    /// `{target}/total` when a slice declares both.
    pub fn bind(&self, tail: &str) -> Option<Binding> {
        let chunks: Vec<&str> = tail.split('/').collect();
        let mut order: Vec<usize> = (0..self.patterns.len()).collect();
        order.sort_by(|&a, &b| self.patterns[a].0.precedence_cmp(&self.patterns[b].0));
        for idx in order {
            let (pattern, family, field) = &self.patterns[idx];
            let Some(binds) = pattern.matches(&chunks) else {
                continue;
            };
            let fam = &self.families[*family];
            let mut bindings = Vec::new();
            let mut rest = None;
            for (name, value) in binds {
                if fam.vars.iter().any(|v| v == name) {
                    bindings.push((name.to_string(), value));
                } else {
                    rest = Some(value);
                }
            }
            return Some(Binding {
                family: *family,
                field: *field,
                bindings,
                rest,
            });
        }
        None
    }

    /// Fold a device's latest points into instances, per family.
    ///
    /// `metrics` is keyed by the point's metric name, which for a host
    /// producer is the subject tail; a proxy producer's tail carries the
    /// device chunk first, and its rest-var family binds it the same way.
    /// A metric the slice does not declare is skipped here and reported by
    /// the intake, not silently absorbed.
    pub fn instances<'a>(
        &self,
        metrics: impl IntoIterator<Item = (&'a String, &'a TelemetryPoint)>,
    ) -> Vec<FamilyInstances> {
        let mut per_family: Vec<BTreeMap<String, Instance>> =
            (0..self.families.len()).map(|_| BTreeMap::new()).collect();
        for (metric, point) in metrics {
            let Some(b) = self.bind(metric) else {
                continue;
            };
            let field_name = match &b.rest {
                Some(rest) => rest.clone(),
                None => self.families[b.family].fields[b.field].name.clone(),
            };
            let id = b.instance_id();
            let instance = per_family[b.family]
                .entry(id.clone())
                .or_insert_with(|| Instance {
                    id,
                    bindings: b.bindings.clone(),
                    values: BTreeMap::new(),
                });
            instance.values.insert(field_name, point.clone());
        }
        per_family
            .into_iter()
            .enumerate()
            .filter(|(_, m)| !m.is_empty())
            .map(|(family, m)| FamilyInstances {
                family,
                instances: m.into_values().collect(),
            })
            .collect()
    }
}

/// Split a pattern into `(family prefix, vars, field tail, open)`.
///
/// The prefix ends at the last variable; with none, at the second-to-last
/// chunk. A trailing rest variable makes the whole path the prefix and the
/// field set open.
fn split(chunks: &[PatternChunk]) -> (String, Vec<String>, String, bool) {
    let vars: Vec<String> = chunks
        .iter()
        .filter_map(|c| match c {
            PatternChunk::Var(v) | PatternChunk::Rest(v) => Some(v.clone()),
            PatternChunk::Literal(_) => None,
        })
        .collect();
    let open = matches!(chunks.last(), Some(PatternChunk::Rest(_)));
    let last_var = chunks
        .iter()
        .rposition(|c| matches!(c, PatternChunk::Var(_) | PatternChunk::Rest(_)));
    let cut = match last_var {
        Some(i) => i + 1,
        None => chunks.len().saturating_sub(1),
    };
    let render = |c: &PatternChunk| match c {
        PatternChunk::Literal(l) => l.clone(),
        PatternChunk::Var(v) => format!("{{{v}}}"),
        PatternChunk::Rest(v) => format!("{{{v}...}}"),
    };
    let prefix = chunks[..cut]
        .iter()
        .map(render)
        .collect::<Vec<_>>()
        .join("/");
    let tail = chunks[cut..]
        .iter()
        .map(render)
        .collect::<Vec<_>>()
        .join("/");
    // A rest variable names no field: the live tail does.
    let vars = if open {
        vars[..vars.len() - 1].to_vec()
    } else {
        vars
    };
    (prefix, vars, tail, open)
}

fn numeric(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        TelemetryValue::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// A counter's rate between two samples, per second. `None` when the clock
/// did not advance or the counter went backwards (a reset: the rate is
/// unknowable from these two points, and a negative rate is a lie).
pub fn rate_between(prev: &TelemetryPoint, cur: &TelemetryPoint) -> Option<f64> {
    rate_between_samples(
        (prev.timestamp, numeric(&prev.value)?),
        (cur.timestamp, numeric(&cur.value)?),
    )
}

/// [`rate_between`] over `(timestamp_ms, value)` pairs — the store's samples.
pub fn rate_between_samples(prev: (i64, f64), cur: (i64, f64)) -> Option<f64> {
    let dt_ms = cur.0 - prev.0;
    if dt_ms <= 0 || cur.1 < prev.1 {
        return None;
    }
    Some((cur.1 - prev.1) / (dt_ms as f64 / 1000.0))
}

/// Which field a family's rows are graded on, and against which siblings —
/// the **default** rule, with no definition loaded (#1258).
///
/// A limit is a sibling the publisher declares, never a number this GUI
/// holds: the rule reads the slice's field *names* and nothing else. A field
/// named `upper_critical_*` / `critical_*` is a critical limit,
/// `upper_warning_*` / `warning_*` a warning limit, and the reading they
/// grade is the family's one remaining absolute field — a gauge, or a field
/// whose slice declares no kind at all (bmc's, today). Two candidate
/// readings, or none, or no limit-named sibling at all — no grading: a
/// definition (`[panel.grade]`, #1259) can say what the names cannot, and
/// until it does a reading with no limit gets no verdict (the #1126–#1128
/// honesty rules, by construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grading {
    /// Index into `family.fields` of the graded reading.
    pub reading: usize,
    pub warning: Option<usize>,
    pub critical: Option<usize>,
}

fn is_limit_name(name: &str, level: &str) -> bool {
    let last = name.rsplit('/').next().unwrap_or(name);
    last.starts_with(&format!("upper_{level}")) || last.starts_with(level)
}

pub fn default_grading(family: &Family) -> Option<Grading> {
    let critical = family
        .fields
        .iter()
        .position(|f| is_limit_name(&f.name, "critical"));
    let warning = family
        .fields
        .iter()
        .position(|f| is_limit_name(&f.name, "warning"));
    if critical.is_none() && warning.is_none() {
        return None;
    }
    let readings: Vec<usize> = family
        .fields
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            Some(*i) != critical
                && Some(*i) != warning
                && matches!(
                    f.presentation(),
                    Presentation::Absolute | Presentation::Unknown
                )
        })
        .map(|(i, _)| i)
        .collect();
    match readings.as_slice() {
        [reading] => Some(Grading {
            reading: *reading,
            warning,
            critical,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::fake_sensor::{PRODUCER, SLICE, samples_of};
    use std::collections::HashMap;

    fn model() -> FamilyModel {
        let slice = zenkey::slice::parse_slice(SLICE).expect("fixture slice parses");
        FamilyModel::from_slice(&slice)
    }

    fn fixture_metrics() -> HashMap<String, TelemetryPoint> {
        let mut out = HashMap::new();
        for (_, payload) in samples_of("telemetry") {
            let p: TelemetryPoint = serde_json::from_slice(&payload).unwrap();
            // Latest wins, as the device map keeps it.
            out.insert(p.metric.clone(), p);
        }
        out
    }

    /// The design table's rule on the fixture: two families, the two-var
    /// temperature one with its sibling limit and the one-var unit family
    /// with the counter.
    #[test]
    fn the_fixture_derives_two_families() {
        let m = model();
        assert_eq!(m.producer, PRODUCER);
        let paths: Vec<&str> = m.families.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["{unit}/temp/{sensor}", "{unit}"]);

        let temp = m.family("{unit}/temp/{sensor}").unwrap();
        assert_eq!(temp.vars, vec!["unit", "sensor"]);
        assert!(temp.is_table());
        let names: Vec<&str> = temp.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["celsius", "upper_critical_c"]);
        let celsius = temp.field("celsius").unwrap();
        assert_eq!(celsius.presentation(), Presentation::Absolute);
        assert_eq!(celsius.display_unit().as_deref(), Some("Cel"));

        let unit = m.family("{unit}").unwrap();
        assert_eq!(unit.vars, vec!["unit"]);
        let rx = unit.field("uplink/rx_bytes").unwrap();
        assert_eq!(rx.kind, Some(SubjectKind::Counter));
        assert_eq!(rx.presentation(), Presentation::Rate, "a counter is a rate");
        assert_eq!(rx.display_unit().as_deref(), Some("By/s"));
    }

    /// The binder: a live tail lands on its family and field with its
    /// variables bound in family order; an undeclared tail binds to nothing.
    #[test]
    fn a_live_tail_binds_to_its_family_and_field() {
        let m = model();
        let b = m.bind("rack7/temp/inlet/upper_critical_c").unwrap();
        assert_eq!(m.families[b.family].path, "{unit}/temp/{sensor}");
        assert_eq!(
            m.families[b.family].fields[b.field].name,
            "upper_critical_c"
        );
        assert_eq!(
            b.bindings,
            vec![
                ("unit".to_string(), "rack7".to_string()),
                ("sensor".to_string(), "inlet".to_string())
            ]
        );
        assert_eq!(b.instance_id(), "rack7/inlet");
        assert_eq!(b.rest, None);
        assert!(m.bind("rack7/humidity/pct").is_none(), "not declared");
        assert!(
            m.bind("rack7/temp/inlet").is_none(),
            "a prefix is not a subject"
        );
    }

    /// Gate 2's fold: the instances the fixture's samples produce.
    #[test]
    fn the_fixture_folds_into_three_sensors_and_one_unit() {
        let m = model();
        let metrics = fixture_metrics();
        let folded = m.instances(metrics.iter());
        assert_eq!(folded.len(), 2, "both families have instances");

        let temp = folded
            .iter()
            .find(|f| m.families[f.family].path == "{unit}/temp/{sensor}")
            .unwrap();
        let ids: Vec<&str> = temp.instances.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["rack7/exhaust", "rack7/inlet", "rack7/outlet"]);
        let inlet = temp
            .instances
            .iter()
            .find(|i| i.id == "rack7/inlet")
            .unwrap();
        assert_eq!(inlet.number("celsius"), Some(41.5));
        assert_eq!(inlet.number("upper_critical_c"), Some(40.0));
        let exhaust = temp
            .instances
            .iter()
            .find(|i| i.id == "rack7/exhaust")
            .unwrap();
        assert_eq!(exhaust.number("celsius"), Some(70.0));
        assert_eq!(
            exhaust.number("upper_critical_c"),
            None,
            "no limit declared for it"
        );

        let unit = folded
            .iter()
            .find(|f| m.families[f.family].path == "{unit}")
            .unwrap();
        assert_eq!(unit.instances.len(), 1);
        assert_eq!(unit.instances[0].id, "rack7");
        assert_eq!(
            unit.instances[0].number("uplink/rx_bytes"),
            Some(2_000_000.0)
        );
    }

    /// The counter-as-rate on the fixture's two samples ten seconds apart.
    #[test]
    fn a_counter_presents_as_a_rate() {
        let samples: Vec<TelemetryPoint> = samples_of("telemetry")
            .into_iter()
            .map(|(_, p)| serde_json::from_slice(&p).unwrap())
            .filter(|p: &TelemetryPoint| p.metric == "rack7/uplink/rx_bytes")
            .collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(rate_between(&samples[0], &samples[1]), Some(100_000.0));
        // Backwards is a reset, not a negative rate; no time is no rate.
        assert_eq!(rate_between(&samples[1], &samples[0]), None);
        assert_eq!(rate_between(&samples[0], &samples[0]), None);
    }

    /// The default grading rule: the fixture's temperature family grades
    /// `celsius` against `upper_critical_c`; bmc's thermal family against
    /// both limits; bmc's PSU family, with no limit-named sibling, is not
    /// graded — `capacity_watts` is a limit only when a definition says so.
    #[test]
    fn the_default_grading_reads_only_limit_named_siblings() {
        let m = model();
        let temp = m.family("{unit}/temp/{sensor}").unwrap();
        let g = default_grading(temp).expect("graded");
        assert_eq!(temp.fields[g.reading].name, "celsius");
        assert_eq!(
            g.critical.map(|i| temp.fields[i].name.as_str()),
            Some("upper_critical_c")
        );
        assert_eq!(g.warning, None);
        assert_eq!(
            default_grading(m.family("{unit}").unwrap()),
            None,
            "a counter is not graded"
        );

        let bmc = FamilyModel::for_producer("bmc").unwrap();
        let thermal = bmc.family("{chassis}/thermal/{sensor}").unwrap();
        let g = default_grading(thermal).unwrap();
        assert_eq!(thermal.fields[g.reading].name, "celsius");
        assert!(g.warning.is_some() && g.critical.is_some());
        assert_eq!(
            default_grading(bmc.family("{chassis}/psu/{psu}").unwrap()),
            None
        );
        assert_eq!(
            default_grading(bmc.family("{chassis}/fan/{fan}").unwrap()),
            None
        );
    }

    /// The var-less rule and the rest-var rule, on real registries.
    #[test]
    fn facts_and_open_families() {
        let pve = FamilyModel::for_producer("pve").expect("pve is compiled in");
        let cluster = pve
            .family("cluster")
            .expect("var-less paths group by their prefix");
        assert!(!cluster.is_table());
        assert!(cluster.field("quorate").is_some());
        assert!(cluster.field("nodes_online").is_some());
        // Same variable name, different prefix: two families, not one.
        assert!(pve.family("guest/{vmid}").is_some());
        assert!(pve.family("backup/{vmid}").is_some());
        assert!(pve.family("backup/job/{node}").is_some());
        assert!(pve.family("node/{node}").is_some());
        let b = pve.bind("cluster/quorate").unwrap();
        assert_eq!(b.instance_id(), "");

        // snmp declares typed families beside its rest-var catch-all: the
        // typed one wins where it matches (zenkey's precedence), the
        // catch-all takes the rest of the device's tree — an open family
        // whose field is named by the live tail.
        let snmp = FamilyModel::for_producer("snmp").expect("snmp is compiled in");
        let open = snmp
            .families
            .iter()
            .find(|f| f.open)
            .expect("snmp's device tree is a rest-var family by design");
        let typed = snmp.bind("sw1/if/1/in_octets").unwrap();
        assert_eq!(snmp.families[typed.family].path, "{device}/if/{index}");
        assert_eq!(typed.instance_id(), "sw1/1");
        assert_eq!(typed.rest, None);
        let tail = "sw1/vendor/foo/bar".to_string();
        let b = snmp.bind(&tail).expect("the rest var binds any other tail");
        assert_eq!(snmp.families[b.family].path, open.path);
        assert_eq!(b.instance_id(), "sw1");
        assert_eq!(b.rest.as_deref(), Some("vendor/foo/bar"));
        let metrics: HashMap<String, TelemetryPoint> = [(
            tail.clone(),
            TelemetryPoint::new("sw1", tail.clone(), TelemetryValue::Counter(7)),
        )]
        .into_iter()
        .collect();
        let folded = snmp.instances(metrics.iter());
        let inst = &folded
            .iter()
            .find(|f| f.family == b.family)
            .unwrap()
            .instances[0];
        assert_eq!(inst.number("vendor/foo/bar"), Some(7.0));
    }

    /// Every compiled-in producer derives without a panic, and every one of
    /// its declared telemetry subjects binds to itself when its variables
    /// are given values — the binder covers the registry it was built from.
    #[test]
    fn every_registry_binds_its_own_subjects() {
        for (name, toml) in zensight_common::registry::REGISTRIES {
            if name.starts_with('@') {
                continue;
            }
            let slice = zenkey::slice::parse_slice(toml).expect("compiled-in slice parses");
            let m = FamilyModel::from_slice(&slice);
            for decl in slice
                .subjects
                .iter()
                .filter(|d| d.class.is(&Class::Telemetry))
            {
                let tail = decl
                    .path
                    .split('/')
                    .map(|c| {
                        if c.starts_with('{') && c.ends_with("...}") {
                            "x/y".to_string()
                        } else if c.starts_with('{') {
                            "x".to_string()
                        } else {
                            c.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                let b = m
                    .bind(&tail)
                    .unwrap_or_else(|| panic!("{name}: {} does not bind ({tail})", decl.path));
                assert_eq!(
                    m.families[b.family].fields[b.field].path, decl.path,
                    "{name}: {tail} bound to the wrong subject"
                );
            }
        }
    }
}
