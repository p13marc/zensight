//! View definitions (#1259, design §6): a producer's `views.toml`, its
//! presentation scripts under limits, the lint, and the panels it renders to.
//!
//! The vocabulary is `zensight_common::views` — closed, structural, no
//! expressions. Everything conditional is a `{ rhai = "…" }` slot that
//! returns a value: `label`/`note`/`format.*` a string or `()`, `show` a
//! bool, `sort` a number or a string. This module evaluates those and
//! nothing more; the renderer draws. A script cannot name a colour, a size
//! or a widget, so the design-system guard holds by construction.
//!
//! **Limits, from Rhai's Safety chapter** (§6.4): an operation budget, an
//! `on_progress` wall-clock budget that terminates a runaway script with
//! `ErrorTerminated` rather than stalling a frame, a call-level cap, string /
//! array / map size caps, and the `unchecked` feature off. The engine
//! registers three pure host functions — `fmt_age`, `fmt_bytes`,
//! `fmt_unit` — and no clock: a script must not know the time; staleness is
//! the renderer's, from `ttl_s`.
//!
//! **A broken view looks broken.** A script that fails to compile or to run
//! renders the slot's fallback (the field's default display, the instance id
//! as the label, no note) *plus* a visible "view script failed: …" line under
//! the panel — never a silently empty cell.
//!
//! **Grading is by field names only.** `[panel.grade]` names sibling fields;
//! a `{ const, declared_by }` literal is admitted and its verdict is labelled
//! with whose number it is. There is no script slot for a limit, so the
//! renderer never colours a computed number as the producer's threshold.
//!
//! **The lint.** [`lint`] checks a document against the producer's family
//! model: every `scope` (and `join`) is a family, every field it names is
//! declared, every script compiles, and every identifier a script uses is a
//! declared field, a bound variable, `row`/`decl`, or a host function. It is
//! lexical over the script text (Rhai's AST is not walkable without its
//! `internals` feature), which is exact for what a view script is — a
//! one-line expression over `row` — and it names the file, the panel and the
//! field in every message. It runs as a test in this crate over the bundled
//! documents and the system-view fixture; a sensor never links the engine.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rhai::{AST, Dynamic, Engine, Map, Scope};
use zensight_common::views::{Limit, Slot, Sort, ViewSet};

use crate::view::components::limit_table::LimitRow;
use crate::view::device::{DeviceDetailState, FamilyCell, FamilyPanel, FamilyRow};
use crate::view::family::{Family, FamilyModel, Instance};

/// Operations a single slot evaluation may spend. A row label is a few
/// dozen; a few thousand is generous, and a script that needs more is not a
/// label.
pub const OPERATION_BUDGET: u64 = 10_000;
/// Wall-clock budget per evaluation. Past it `on_progress` terminates the
/// script — this is what turns `loop {}` into a visible failure rather than
/// a stalled frame.
pub const WALL_BUDGET: Duration = Duration::from_millis(20);
pub const MAX_CALL_LEVELS: usize = 8;
pub const MAX_STRING_SIZE: usize = 4_096;
pub const MAX_ARRAY_SIZE: usize = 1_024;
pub const MAX_MAP_SIZE: usize = 256;

/// The exact words every script failure starts with — one widget, one
/// string, so a test can find it and a reader can grep it.
pub const FAILURE_MARKER: &str = "view script failed";

/// The host functions a script may call — pure, and the whole list. `now_ms`
/// is deliberately not here.
pub const HOST_FUNCTIONS: [&str; 3] = ["fmt_age", "fmt_bytes", "fmt_unit"];

/// The engine under the §6.4 limits, with its wall-clock start handle: reset
/// it before every evaluation.
fn engine(started: Arc<Mutex<Instant>>) -> Engine {
    let mut e = Engine::new();
    e.set_max_operations(OPERATION_BUDGET);
    e.set_max_call_levels(MAX_CALL_LEVELS);
    e.set_max_string_size(MAX_STRING_SIZE);
    e.set_max_array_size(MAX_ARRAY_SIZE);
    e.set_max_map_size(MAX_MAP_SIZE);
    e.on_progress(move |_| {
        let start = *started.lock().unwrap_or_else(|p| p.into_inner());
        (start.elapsed() > WALL_BUDGET).then_some(Dynamic::from("wall-clock budget exceeded"))
    });
    e.register_fn("fmt_age", |secs: f64| fmt_age(secs));
    e.register_fn("fmt_bytes", |bytes: f64| {
        crate::view::formatting::format_bytes(bytes)
    });
    e.register_fn("fmt_unit", |v: f64, unit: &str| {
        format!("{} {unit}", fmt_num(v))
    });
    e
}

/// Seconds as an age: `3.2d`, `4.0h`, `12m`, `40s`.
pub fn fmt_age(secs: f64) -> String {
    if secs >= 86_400.0 {
        format!("{:.1}d", secs / 86_400.0)
    } else if secs >= 3_600.0 {
        format!("{:.1}h", secs / 3_600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

/// A number as a reading: integers without decimals, the rest to two.
pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A compiled definition: the document, the engine, and one compiled AST
/// (or its compile error, kept to render) per script slot.
pub struct Definition {
    pub set: ViewSet,
    engine: Engine,
    started: Arc<Mutex<Instant>>,
    asts: HashMap<String, Result<AST, String>>,
}

impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definition")
            .field("producer", &self.set.view.producer)
            .field("panels", &self.set.panel.len())
            .field("scripts", &self.asts.len())
            .finish()
    }
}

impl Definition {
    /// Compile every script once. A script that does not compile is kept as
    /// its error, so the panel renders its fallback and says so.
    pub fn compile(set: ViewSet) -> Self {
        let started = Arc::new(Mutex::new(Instant::now()));
        let engine = engine(started.clone());
        let asts = set
            .scripts()
            .into_iter()
            .map(|(path, script)| {
                let compiled = engine.compile(script).map_err(|e| e.to_string());
                (path, compiled)
            })
            .collect();
        Self {
            set,
            engine,
            started,
            asts,
        }
    }

    /// The bundled definition for a producer, compiled.
    pub fn bundled(producer: &str) -> Option<Self> {
        ViewSet::bundled(producer).map(Self::compile)
    }

    /// Evaluate one slot. `Err` names the slot and the failure.
    fn eval(&self, path: &str, scope: &mut Scope) -> Result<Dynamic, String> {
        let ast = match self.asts.get(path) {
            Some(Ok(ast)) => ast,
            Some(Err(e)) => return Err(format!("{path}: does not compile: {e}")),
            None => return Err(format!("{path}: no script")),
        };
        *self.started.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        self.engine
            .eval_ast_with_scope::<Dynamic>(scope, ast)
            .map_err(|e| format!("{path}: {e}"))
    }
}

/// How a panel's `scope` resolves against the model: the family, and the
/// literal chunks past the family prefix that select a field subtree
/// (`{unit}/uplink` → family `{unit}`, fields under `uplink/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub family: usize,
    pub prefix: String,
}

/// Resolve a scope: the longest family path that is the scope or a
/// `/`-prefix of it, the remainder being literal chunks only. An open family
/// (`{device}/{metric...}`) is also reached by its path without the rest
/// variable (`{device}`), which is how a proxy producer's document names its
/// device table.
pub fn resolve_scope(model: &FamilyModel, scope: &str) -> Option<Resolved> {
    let mut best: Option<(usize, usize)> = None;
    for (i, f) in model.families.iter().enumerate() {
        let open_head = f.open.then(|| {
            f.path
                .rsplit_once('/')
                .map(|(head, _)| head.to_string())
                .unwrap_or_default()
        });
        let hit = if f.path == scope || open_head.as_deref() == Some(scope) {
            Some(f.path.len())
        } else if f.path.is_empty() {
            (!scope.contains('{')).then_some(0)
        } else if let Some(rest) = scope.strip_prefix(&f.path)
            && rest.starts_with('/')
            && !rest.contains('{')
        {
            Some(f.path.len())
        } else {
            None
        };
        if let Some(len) = hit
            && best.is_none_or(|(_, l)| len > l)
        {
            best = Some((i, len));
        }
    }
    let (family, len) = best?;
    let prefix = scope
        .get(len..)
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    Some(Resolved { family, prefix })
}

/// The field names a resolved scope exposes to a panel: the family's fields
/// under the prefix, with the prefix stripped.
fn scoped_fields<'a>(family: &'a Family, prefix: &str) -> Vec<(String, &'a str)> {
    family
        .fields
        .iter()
        .filter_map(|f| {
            if prefix.is_empty() {
                Some((f.name.clone(), f.name.as_str()))
            } else {
                f.name
                    .strip_prefix(prefix)
                    .and_then(|r| r.strip_prefix('/'))
                    .map(|short| (short.to_string(), f.name.as_str()))
            }
        })
        .collect()
}

/// The head of a join family: `backup/{vmid}` → `backup`; the name the
/// partner's fields are reached under (`row.backup.age_secs`,
/// `"backup.age_secs"`).
fn join_head(path: &str) -> String {
    path.split('/')
        .take_while(|c| !c.starts_with('{'))
        .collect::<Vec<_>>()
        .join("/")
        .replace('/', "_")
}

/// Lint a document against its producer's model. Every message names the
/// file and the panel; a bad field names the field.
pub fn lint(set: &ViewSet, model: &FamilyModel, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut say = |msg: String| out.push(format!("{file}: {msg}"));
    if set.view.producer != model.producer {
        say(format!(
            "view.producer is {:?}, the slice's is {:?}",
            set.view.producer, model.producer
        ));
    }
    let families: Vec<&str> = model.families.iter().map(|f| f.path.as_str()).collect();
    for (i, p) in set.panel.iter().enumerate() {
        let panel = format!("panel[{i}] ({})", p.title.as_deref().unwrap_or(&p.scope));
        if !matches!(
            p.kind.as_str(),
            "table" | "facts" | "document" | "chart" | "reply" | "custom"
        ) {
            say(format!(
                "{panel}: kind {:?} is not one of the vocabulary",
                p.kind
            ));
        }
        if p.kind == "document" {
            // A document scope is a state subject, judged by the intake
            // against the slice; the family model does not carry it.
            continue;
        }
        let Some(resolved) = resolve_scope(model, &p.scope) else {
            say(format!(
                "{panel}: scope {:?} is not a family of {}; families: {families:?}",
                p.scope, model.producer
            ));
            continue;
        };
        let family = &model.families[resolved.family];
        let mut declared: BTreeSet<String> = scoped_fields(family, &resolved.prefix)
            .into_iter()
            .map(|(short, _)| short)
            .collect();
        let mut vars: BTreeSet<String> = family.vars.iter().cloned().collect();
        if let Some(join) = &p.join {
            match model.family(join) {
                Some(partner) => {
                    if !partner.vars.iter().any(|v| family.vars.contains(v)) {
                        say(format!(
                            "{panel}: join {join:?} shares no variable with {:?}",
                            family.path
                        ));
                    }
                    let head = join_head(join);
                    for f in &partner.fields {
                        declared.insert(format!("{head}.{}", f.name));
                    }
                    declared.insert(head.clone());
                    vars.extend(partner.vars.iter().cloned());
                }
                None => say(format!(
                    "{panel}: join {join:?} is not a family of {}",
                    model.producer
                )),
            }
        }
        if family.open {
            // An open family's fields are whatever the tails are; nothing to
            // check them against.
            continue;
        }
        let mut check = |what: &str, name: &str| {
            if !declared.contains(name) {
                say(format!(
                    "{panel}: {what} {name:?} is not a field of {:?} (declared: {})",
                    p.scope,
                    declared.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        };
        for f in p.fields.iter().flatten() {
            check("field", f);
        }
        for f in &p.hide {
            check("hide", f);
        }
        for f in &p.sparkline {
            check("sparkline", f);
        }
        for f in p.format.keys() {
            check("format", f);
        }
        if let Some(Sort::Field { by, .. }) = &p.sort {
            check("sort.by", by);
        }
        if let Some(g) = &p.grade {
            check("grade.reading", &g.reading);
            for (what, limit) in [
                ("grade.warning", &g.warning),
                ("grade.critical", &g.critical),
            ] {
                if let Some(Limit::Field(f)) = limit {
                    check(what, f);
                }
            }
            if let Some(a) = &g.absent {
                check("grade.absent", a);
            }
        }
        // Scripts: compile, then every identifier must be something the
        // scope holds.
        let engine = Engine::new();
        let mut scripts: Vec<(String, &str)> = Vec::new();
        for (name, slot) in [("label", &p.label), ("show", &p.show), ("note", &p.note)] {
            if let Some(s) = slot.as_ref().and_then(Slot::script) {
                scripts.push((name.to_string(), s));
            }
        }
        if let Some(Sort::Rhai { rhai }) = &p.sort {
            scripts.push(("sort".to_string(), rhai));
        }
        for (field, slot) in &p.format {
            if let Some(s) = slot.script() {
                scripts.push((format!("format.{field}"), s));
            }
        }
        for (slot, script) in scripts {
            if let Err(e) = engine.compile(script) {
                say(format!("{panel}: {slot} does not compile: {e}"));
                continue;
            }
            for ident in identifiers(script) {
                if ident.starts_with("row.") {
                    let field = ident.trim_start_matches("row.");
                    // `row.backup` alone is the partner's presence.
                    if !declared.contains(field) {
                        say(format!(
                            "{panel}: {slot} names row.{field}, which is not a field of {:?}",
                            p.scope
                        ));
                    }
                } else if ident == "row"
                    || ident == "decl"
                    || vars.contains(&ident)
                    || HOST_FUNCTIONS.contains(&ident.as_str())
                {
                } else {
                    say(format!(
                        "{panel}: {slot} names {ident:?}, which is neither a field, a variable of {:?}, nor a host function",
                        p.scope
                    ));
                }
            }
        }
    }
    out
}

const KEYWORDS: [&str; 22] = [
    "if", "else", "let", "const", "fn", "return", "true", "false", "loop", "while", "for", "in",
    "break", "continue", "switch", "throw", "try", "catch", "import", "export", "as", "private",
];

/// The identifiers a script names, lexically: bare names, and `row.<path>`
/// for property access on `row`. String literals are skipped except for the
/// `${…}` interpolations inside a template. `foo.bar` on anything but `row`
/// yields `foo`.
pub fn identifiers(script: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    let mut in_str: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
                i += 1;
                continue;
            }
            if q == '`' && c == '$' && chars.get(i + 1) == Some(&'{') {
                // An interpolation: scan to its closing brace.
                let start = i + 2;
                let mut depth = 1;
                let mut j = start;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                let inner: String = chars[start..j.saturating_sub(1)].iter().collect();
                out.extend(identifiers(&inner));
                i = j;
                continue;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            in_str = Some(c);
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            // A numeric literal, exponent and all: `-1.0e12` names nothing.
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // A property path: `row.a.b` is one identifier.
            let mut path = word.clone();
            while chars.get(i) == Some(&'.')
                && chars
                    .get(i + 1)
                    .is_some_and(|c| c.is_alphabetic() || *c == '_')
            {
                i += 1;
                let s = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let seg: String = chars[s..i].iter().collect();
                path.push('.');
                path.push_str(&seg);
            }
            if KEYWORDS.contains(&word.as_str()) {
                continue;
            }
            if word == "row" && path != "row" {
                out.insert(path);
            } else {
                out.insert(word);
            }
            continue;
        }
        i += 1;
    }
    out
}

/// A view rendered from its definition: the panels, and every script
/// failure the render met, each once, to show under the panels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rendered {
    pub panels: Vec<FamilyPanel>,
    pub failures: Vec<String>,
}

fn dyn_of(v: &zensight_common::TelemetryValue) -> Dynamic {
    use zensight_common::TelemetryValue::*;
    match v {
        Counter(c) => Dynamic::from(*c as f64),
        Gauge(g) => Dynamic::from(*g),
        Boolean(b) => Dynamic::from(*b),
        Text(t) => Dynamic::from(t.clone()),
        Binary(b) => Dynamic::from(format!("<{} bytes>", b.len())),
    }
}

fn row_map(instance: &Instance, fields: &[(String, &str)]) -> Map {
    let mut m = Map::new();
    for (short, full) in fields {
        let v = instance
            .point(full)
            .map(|p| dyn_of(&p.value))
            .unwrap_or(Dynamic::UNIT);
        m.insert(short.as_str().into(), v);
    }
    m
}

fn decl_map(family: &Family, fields: &[(String, &str)]) -> Map {
    let mut m = Map::new();
    for (short, full) in fields {
        if let Some(f) = family.field(full) {
            let mut d = Map::new();
            d.insert(
                "unit".into(),
                f.unit.clone().map(Dynamic::from).unwrap_or(Dynamic::UNIT),
            );
            d.insert(
                "kind".into(),
                f.kind
                    .map(|k| Dynamic::from(k.payload_tag().to_string()))
                    .unwrap_or(Dynamic::UNIT),
            );
            d.insert(
                "description".into(),
                f.description
                    .clone()
                    .map(Dynamic::from)
                    .unwrap_or(Dynamic::UNIT),
            );
            m.insert(short.as_str().into(), Dynamic::from(d));
        }
    }
    m
}

fn substitute_vars(literal: &str, bindings: &[(String, String)]) -> String {
    let mut s = literal.to_string();
    for (k, v) in bindings {
        s = s.replace(&format!("${k}"), v);
    }
    s
}

/// Render a device through its definition (#1259): the panels in the
/// document's order, each over the family model's instances, with the
/// slots evaluated per row and every script failure collected.
pub fn render(state: &DeviceDetailState, def: &Definition) -> Rendered {
    let mut out = Rendered::default();
    let Some(model) = state.family.as_ref() else {
        return out;
    };
    let folded = model.instances(state.metrics.iter());
    let instances_of = |family: usize| -> &[Instance] {
        folded
            .iter()
            .find(|f| f.family == family)
            .map(|f| f.instances.as_slice())
            .unwrap_or(&[])
    };
    let mut fail = |msg: String| {
        let line = format!("{FAILURE_MARKER}: {msg}");
        if !out.failures.contains(&line) {
            out.failures.push(line);
        }
    };
    for (i, p) in def.set.panel.iter().enumerate() {
        let title = p.title.clone().unwrap_or_else(|| p.scope.clone());
        match p.kind.as_str() {
            "table" | "facts" => {}
            // Documents render through the intake's own cards (#1256); the
            // rest of the vocabulary has no renderer yet, and says so rather
            // than rendering nothing.
            "document" => continue,
            other => {
                out.panels.push(FamilyPanel {
                    title,
                    is_table: false,
                    rows: vec![FamilyRow {
                        instance: String::new(),
                        cells: vec![FamilyCell {
                            field: "kind".into(),
                            text: format!("panel kind `{other}` is declared and not rendered yet"),
                        }],
                        verdict: None,
                        note: None,
                    }],
                });
                continue;
            }
        }
        let Some(resolved) = resolve_scope(model, &p.scope) else {
            out.panels.push(FamilyPanel {
                title,
                is_table: false,
                rows: vec![FamilyRow {
                    instance: String::new(),
                    cells: vec![FamilyCell {
                        field: "scope".into(),
                        text: format!("scope `{}` is not a family this producer declares", p.scope),
                    }],
                    verdict: None,
                    note: None,
                }],
            });
            continue;
        };
        let family = &model.families[resolved.family];
        let all_fields = scoped_fields(family, &resolved.prefix);
        // The partner family of a join, keyed by the shared variable's value.
        let join = p.join.as_ref().and_then(|j| {
            let idx = model.families.iter().position(|f| &f.path == j)?;
            let partner = &model.families[idx];
            let shared = family
                .vars
                .iter()
                .find(|v| partner.vars.contains(v))?
                .clone();
            let head = join_head(j);
            let by_value: BTreeMap<String, &Instance> = instances_of(idx)
                .iter()
                .filter_map(|inst| {
                    let v = inst.bindings.iter().find(|(k, _)| *k == shared)?.1.clone();
                    Some((v, inst))
                })
                .collect();
            Some((partner, shared, head, by_value))
        });
        // The columns: `fields` if given (join fields spelled `head.field`),
        // else every scoped field, minus `hide`.
        let columns: Vec<String> = match &p.fields {
            Some(f) => f.clone(),
            None => all_fields.iter().map(|(s, _)| s.clone()).collect(),
        }
        .into_iter()
        .filter(|c| !p.hide.contains(c))
        .collect();

        let mut rows: Vec<(Dynamic, FamilyRow)> = Vec::new();
        for instance in instances_of(resolved.family) {
            // The script's `row`: the scoped fields, plus the join partner
            // under its head (or `()`).
            let mut row = row_map(instance, &all_fields);
            let mut partner_inst: Option<&Instance> = None;
            if let Some((partner, shared, head, by_value)) = &join {
                let key = instance
                    .bindings
                    .iter()
                    .find(|(k, _)| k == shared)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                match by_value.get(&key) {
                    Some(pi) => {
                        partner_inst = Some(pi);
                        let pf: Vec<(String, &str)> = partner
                            .fields
                            .iter()
                            .map(|f| (f.name.clone(), f.name.as_str()))
                            .collect();
                        row.insert(head.as_str().into(), Dynamic::from(row_map(pi, &pf)));
                    }
                    None => {
                        row.insert(head.as_str().into(), Dynamic::UNIT);
                    }
                }
            }
            let mut scope = Scope::new();
            scope.push("row", row.clone());
            scope.push("decl", decl_map(family, &all_fields));
            for (k, v) in &instance.bindings {
                scope.push(k.clone(), v.clone());
            }

            // show
            if let Some(Slot::Rhai { .. }) = &p.show {
                match def.eval(&format!("panel[{i}].show"), &mut scope) {
                    Ok(v) => {
                        if v.as_bool() == Ok(false) {
                            continue;
                        }
                    }
                    Err(e) => fail(e),
                }
            }
            // label
            let label = match &p.label {
                Some(Slot::Literal(l)) => substitute_vars(l, &instance.bindings),
                Some(Slot::Rhai { .. }) => match def.eval(&format!("panel[{i}].label"), &mut scope)
                {
                    Ok(v) if v.is_unit() => instance.id.clone(),
                    Ok(v) => v.to_string(),
                    Err(e) => {
                        fail(e);
                        instance.id.clone()
                    }
                },
                None => instance.id.clone(),
            };
            // cells
            let value_of = |name: &str| -> Option<(f64, Option<String>)> {
                // `head.field` reaches the partner; a bare name the row.
                if let Some((partner, _, head, _)) = &join
                    && let Some(rest) = name.strip_prefix(&format!("{head}."))
                {
                    let pi = partner_inst?;
                    let unit = partner.field(rest).and_then(|f| f.display_unit());
                    return pi.number(rest).map(|v| (v, unit));
                }
                let full = all_fields.iter().find(|(s, _)| s == name)?.1;
                let unit = family.field(full).and_then(|f| f.display_unit());
                instance.number(full).map(|v| (v, unit))
            };
            let mut cells = Vec::new();
            for col in &columns {
                let text = match p.format.get(col).and_then(Slot::script) {
                    Some(_) => match def.eval(&format!("panel[{i}].format.{col}"), &mut scope) {
                        Ok(v) if v.is_unit() => {
                            default_cell(state, family, &all_fields, instance, col, &value_of)
                        }
                        Ok(v) => v.to_string(),
                        Err(e) => {
                            fail(e);
                            default_cell(state, family, &all_fields, instance, col, &value_of)
                        }
                    },
                    None => default_cell(state, family, &all_fields, instance, col, &value_of),
                };
                cells.push(FamilyCell {
                    field: col.clone(),
                    text,
                });
            }
            // grade — field names, or a literal that says whose it is
            let mut verdict = None;
            let mut note_parts: Vec<String> = Vec::new();
            if let Some(g) = &p.grade {
                let present = g
                    .absent
                    .as_ref()
                    .and_then(|a| value_of(a).map(|(v, _)| v != 0.0))
                    .unwrap_or(true);
                let reading = value_of(&g.reading).map(|(v, _)| v);
                let limit = |l: &Option<Limit>| -> Option<f64> {
                    match l {
                        Some(Limit::Field(f)) => value_of(f).map(|(v, _)| v),
                        Some(Limit::Const { r#const, .. }) => Some(*r#const),
                        None => None,
                    }
                };
                verdict = LimitRow::new("", reading, "")
                    .with_present(present)
                    .with_limits(limit(&g.warning), limit(&g.critical))
                    .verdict();
                if !present {
                    for c in cells.iter_mut() {
                        if c.field == g.reading {
                            c.text = "absent".to_string();
                        }
                    }
                }
                for l in [&g.warning, &g.critical] {
                    if let Some(Limit::Const {
                        r#const,
                        declared_by,
                    }) = l
                        && verdict.is_some()
                    {
                        note_parts.push(format!(
                            "limit {} is the {declared_by}'s, not the producer's",
                            fmt_num(*r#const)
                        ));
                    }
                }
            }
            // note
            if let Some(slot) = &p.note {
                match slot {
                    Slot::Literal(l) => note_parts.push(substitute_vars(l, &instance.bindings)),
                    Slot::Rhai { .. } => match def.eval(&format!("panel[{i}].note"), &mut scope) {
                        Ok(v) if v.is_unit() => {}
                        Ok(v) => note_parts.push(v.to_string()),
                        Err(e) => fail(e),
                    },
                }
            }
            // sort key
            let key = match &p.sort {
                Some(Sort::Rhai { .. }) => {
                    match def.eval(&format!("panel[{i}].sort"), &mut scope) {
                        Ok(v) => v,
                        Err(e) => {
                            fail(e);
                            Dynamic::from(label.clone())
                        }
                    }
                }
                Some(Sort::Field { by, dir }) => {
                    let v = value_of(by).map(|(v, _)| v).unwrap_or(f64::MAX);
                    Dynamic::from(if dir.as_deref() == Some("desc") {
                        -v
                    } else {
                        v
                    })
                }
                None => Dynamic::from(label.clone()),
            };
            rows.push((
                key,
                FamilyRow {
                    instance: label,
                    cells,
                    verdict,
                    note: (!note_parts.is_empty()).then(|| note_parts.join(" · ")),
                },
            ));
        }
        rows.sort_by(|(a, _), (b, _)| cmp_dynamic(a, b));
        if let Some(n) = p.top_n {
            rows.truncate(n);
        }
        let is_table = p.kind == "table" && family.is_table();
        out.panels.push(FamilyPanel {
            title,
            is_table,
            rows: rows.into_iter().map(|(_, r)| r).collect(),
        });
    }
    out
}

/// A numeric reading with its display unit, or nothing.
type Reading = Option<(f64, Option<String>)>;

/// The cell a field shows with no `format` slot: the default renderer's
/// presentation (#1258).
fn default_cell(
    state: &DeviceDetailState,
    family: &Family,
    all_fields: &[(String, &str)],
    instance: &Instance,
    col: &str,
    value_of: &dyn Fn(&str) -> Reading,
) -> String {
    match all_fields.iter().find(|(s, _)| s == col) {
        Some((_, full)) => match family.field(full) {
            Some(field) => crate::view::device::cell_text(state, field, instance, full),
            None => instance
                .point(full)
                .map(|p| format!("{:?}", p.value))
                .unwrap_or_default(),
        },
        None => match value_of(col) {
            Some((v, Some(unit))) => format!("{} {unit}", fmt_num(v)),
            Some((v, None)) => fmt_num(v),
            None => "—".to_string(),
        },
    }
}

fn cmp_dynamic(a: &Dynamic, b: &Dynamic) -> std::cmp::Ordering {
    match (a.as_float(), b.as_float()) {
        (Ok(x), Ok(y)) => x.total_cmp(&y),
        _ => match (a.as_int(), b.as_int()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => match (
                a.as_float().ok().or(a.as_int().ok().map(|i| i as f64)),
                b.as_float().ok().or(b.as_int().ok().map(|i| i as f64)),
            ) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => a.to_string().cmp(&b.to_string()),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use crate::mock::fake_sensor::{PRODUCER, SLICE, VIEWS};
    use crate::view::components::limit_table::LimitVerdict;
    use zensight_common::{TelemetryPoint, TelemetryValue};

    fn fixture_state() -> DeviceDetailState {
        let mut state = DeviceDetailState::new(DeviceId::fixture(PRODUCER, "rack7"));
        for (_, payload) in crate::mock::fake_sensor::samples_of("telemetry") {
            let p: TelemetryPoint = serde_json::from_slice(&payload).unwrap();
            state.update(p);
        }
        state.family = Some(FamilyModel::from_slice(
            &zenkey::slice::parse_slice(SLICE).unwrap(),
        ));
        state
    }

    /// The bundled documents and the fixture lint clean against their own
    /// slices — the "must not lie" posture, for views.
    #[test]
    fn the_bundled_documents_and_the_fixture_lint_clean() {
        for (name, text) in zensight_common::views::VIEWS {
            let set = ViewSet::parse_toml(text).unwrap();
            let model = FamilyModel::for_producer(name).unwrap();
            let findings = lint(&set, &model, &format!("registry/views/{name}.toml"));
            assert!(findings.is_empty(), "{findings:#?}");
        }
        let set = ViewSet::parse_toml(VIEWS).unwrap();
        let model = FamilyModel::from_slice(&zenkey::slice::parse_slice(SLICE).unwrap());
        let findings = lint(&set, &model, "tests/fixtures/fake-sensor/views.toml");
        assert!(findings.is_empty(), "{findings:#?}");
    }

    /// The lint rejects a definition naming an undeclared field, and the
    /// message names the field and the file.
    #[test]
    fn the_lint_names_the_undeclared_field_and_the_file() {
        let doc = r#"
[view]
version = "1"
producer = "fake-sensor"
[[panel]]
kind = "table"
scope = "{unit}/temp/{sensor}"
fields = ["celsius", "humidity"]
label = { rhai = "`${sensor} ${rack}`" }
note = { rhai = "if row.wetness > 1.0 { \"x\" }" }
[panel.grade]
reading = "celsius"
critical = "upper_critical_c"
"#;
        let set = ViewSet::parse_toml(doc).unwrap();
        let model = FamilyModel::from_slice(&zenkey::slice::parse_slice(SLICE).unwrap());
        let findings = lint(&set, &model, "bad.toml");
        assert_eq!(findings.len(), 3, "{findings:#?}");
        assert!(findings[0].contains("bad.toml") && findings[0].contains("\"humidity\""));
        assert!(findings.iter().any(|f| f.contains("\"rack\"")));
        assert!(findings.iter().any(|f| f.contains("row.wetness")));
        // An unknown scope, too.
        let doc2 = doc.replace("{unit}/temp/{sensor}", "{unit}/fans/{fan}");
        let set2 = ViewSet::parse_toml(&doc2).unwrap();
        let f2 = lint(&set2, &model, "bad.toml");
        assert!(
            f2.iter()
                .any(|f| f.contains("scope") && f.contains("{unit}/fans/{fan}"))
        );
    }

    /// Identifiers, lexically: names, `row.` paths, template interpolations;
    /// not string contents, not keywords, not property names on other values.
    #[test]
    fn identifiers_are_extracted_lexically() {
        let ids = identifiers(
            "if row.backup == () { \"never\" } else { fmt_age(row.backup.age_secs) + `${vmid} ${x.y}` } + 1.0e12 - 3_000",
        );
        let v: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(
            v,
            vec!["fmt_age", "row.backup", "row.backup.age_secs", "vmid", "x"]
        );
    }

    /// Gate 5's substance: the fixture definition renders the temperature
    /// table with Rhai labels, the sort order, and the note on exhaust only.
    #[test]
    fn the_fixture_definition_renders_labels_sort_and_the_honest_note() {
        let state = fixture_state();
        let def = Definition::compile(ViewSet::parse_toml(VIEWS).unwrap());
        let r = render(&state, &def);
        assert!(r.failures.is_empty(), "{:?}", r.failures);
        let temps = r.panels.iter().find(|p| p.title == "Temperatures").unwrap();
        let labels: Vec<&str> = temps.rows.iter().map(|r| r.instance.as_str()).collect();
        assert_eq!(
            labels,
            vec!["exhaust @ rack7", "inlet @ rack7", "outlet @ rack7"],
            "hottest first"
        );
        assert_eq!(
            temps.rows[0].note.as_deref(),
            Some("no limit published — not graded")
        );
        assert_eq!(temps.rows[1].note, None);
        assert_eq!(temps.rows[1].verdict, Some(LimitVerdict::Critical));
        assert_eq!(temps.rows[0].verdict, None);
        let uplink = r.panels.iter().find(|p| p.title == "Uplink").unwrap();
        assert!(!uplink.is_table);
        assert_eq!(uplink.rows[0].cells[0].field, "rx_bytes");
        assert!(
            uplink.rows[0].cells[0].text.ends_with("By/s"),
            "{}",
            uplink.rows[0].cells[0].text
        );
    }

    /// A runaway script terminates inside the budget and is visibly reported;
    /// the row keeps its fallback label.
    #[test]
    fn a_runaway_script_terminates_and_is_reported() {
        let state = fixture_state();
        let doc = VIEWS.replace(
            "label  = { rhai = \"`${sensor} @ ${unit}`\" }",
            "label  = { rhai = \"loop {}\" }",
        );
        assert_ne!(doc, VIEWS, "the fixture's label slot moved");
        let def = Definition::compile(ViewSet::parse_toml(&doc).unwrap());
        let t = Instant::now();
        let r = render(&state, &def);
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "took {:?}",
            t.elapsed()
        );
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("view script failed: panel[0].label")),
            "{:?}",
            r.failures
        );
        let temps = r.panels.iter().find(|p| p.title == "Temperatures").unwrap();
        assert!(
            temps.rows.iter().any(|r| r.instance == "rack7/inlet"),
            "fallback label"
        );
        // A script that does not compile is a failure too, not a crash.
        let doc = VIEWS.replace(
            "label  = { rhai = \"`${sensor} @ ${unit}`\" }",
            "label  = { rhai = \"if (\" }",
        );
        let def = Definition::compile(ViewSet::parse_toml(&doc).unwrap());
        let r = render(&state, &def);
        assert!(r.failures.iter().any(|f| f.contains("does not compile")));
    }

    /// The pve join: rows are guests, a guest with no backup is first and
    /// says "never", and the const limit's verdict is labelled the GUI's.
    #[test]
    fn the_pve_join_renders_guests_with_their_backups() {
        let mut state = DeviceDetailState::new(DeviceId::fixture("pve", "pve01"));
        for (metric, v) in [
            ("guest/101/running", 1.0),
            ("backup/101/age_secs", 3600.0),
            ("backup/101/ok", 1.0),
            ("guest/102/running", 1.0),
            ("guest/103/running", 0.0),
            ("backup/103/age_secs", 400_000.0),
        ] {
            state.update(TelemetryPoint::new(
                "pve01",
                metric,
                TelemetryValue::Gauge(v),
            ));
        }
        state.family = FamilyModel::for_producer("pve");
        let def = Definition::bundled("pve").unwrap();
        let r = render(&state, &def);
        assert!(r.failures.is_empty(), "{:?}", r.failures);
        let backups = r
            .panels
            .iter()
            .find(|p| p.title == "Backup freshness")
            .unwrap();
        let labels: Vec<&str> = backups.rows.iter().map(|r| r.instance.as_str()).collect();
        assert_eq!(
            labels,
            vec!["vmid 102", "vmid 103", "vmid 101"],
            "never first, then oldest"
        );
        let never = &backups.rows[0];
        assert!(
            never
                .cells
                .iter()
                .any(|c| c.field == "backup.age_secs" && c.text == "never")
        );
        assert_eq!(never.verdict, None, "no backup, no age, no verdict");
        let stale = &backups.rows[1];
        assert_eq!(stale.verdict, Some(LimitVerdict::Critical));
        assert!(
            stale.note.as_deref().unwrap_or("").contains("gui's"),
            "{:?}",
            stale.note
        );
        assert!(stale.cells.iter().any(|c| c.text == "4.6d"));
    }
}
