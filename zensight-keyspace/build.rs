//! Registry codegen (RFC 08, issue #455).
//!
//! Reads every `registry/*.toml`, lints it against RFC 08 §5 (violations are
//! build errors), and generates `$OUT_DIR/registry_gen.rs`: per producer, a
//! `Subject` enum with typed constructors and a precedence-ordered parser, a
//! `ProcedureId` enum with `@rpc` key builders, and the raw registry slice
//! for the `introspect` procedure.

include!("src/chunk_rules.rs");

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
enum Chunk {
    Literal(String),
    Var(String),
    Rest(String),
}

#[derive(Debug)]
struct SubjectEntry {
    path: String,
    chunks: Vec<Chunk>,
    class: String,
    payload_type: String,
    unit: Option<String>,
    cardinality: Option<i64>,
    qos: String,
    ttl_s: Option<i64>,
    rate: Option<String>,
    variant: String,
}

#[derive(Debug)]
struct ProcedureEntry {
    path: String,
    chunks: Vec<String>,
    kind: String,
    request: Option<String>,
    reply: Option<String>,
    variant: String,
}

#[derive(Debug)]
struct RegistryFile {
    /// Producer base name, or service name for `[service]` files.
    name: String,
    /// `Some("@catalog")`-style origin for services, `None` for producers.
    service_origin: Option<String>,
    toml_path: String,
    subjects: Vec<SubjectEntry>,
    procedures: Vec<ProcedureEntry>,
    deprecated: Vec<String>,
}

fn fail(file: &str, msg: &str) -> ! {
    panic!("registry lint failed [{file}]: {msg}");
}

fn parse_pattern(file: &str, path: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if let Some(var) = part.strip_prefix('{').and_then(|p| p.strip_suffix("...}")) {
            if i != parts.len() - 1 {
                fail(
                    file,
                    &format!("{path:?}: {{var...}} only in trailing position (RFC 08 §2)"),
                );
            }
            if !is_valid_plain_chunk(var) {
                fail(file, &format!("{path:?}: bad rest-variable name {var:?}"));
            }
            chunks.push(Chunk::Rest(var.to_string()));
        } else if let Some(var) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            if !is_valid_plain_chunk(var) {
                fail(file, &format!("{path:?}: bad variable name {var:?}"));
            }
            chunks.push(Chunk::Var(var.to_string()));
        } else {
            if !is_valid_plain_chunk(part) {
                fail(
                    file,
                    &format!("{path:?}: chunk {part:?} violates RFC 03 §2"),
                );
            }
            if *part == "alive" {
                fail(
                    file,
                    &format!("{path:?}: `alive` is a reserved liveliness leaf (RFC 03 §3)"),
                );
            }
            chunks.push(Chunk::Literal(part.to_string()));
        }
    }
    if chunks.is_empty() {
        fail(file, "empty subject path");
    }
    chunks
}

fn camel(parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts {
        for seg in part.split(|c: char| !c.is_ascii_alphanumeric()) {
            let mut chars = seg.chars();
            if let Some(first) = chars.next() {
                out.push(first.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

/// A hand-written `variant = "..."` override must be a plain CamelCase Rust
/// identifier — it lands verbatim in the generated enum.
fn is_valid_variant(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Variant name: CamelCase of the literal chunks; all-variable patterns use
/// the variable names instead.
fn variant_name(chunks: &[Chunk]) -> String {
    let literals: Vec<&str> = chunks
        .iter()
        .filter_map(|c| match c {
            Chunk::Literal(l) => Some(l.as_str()),
            _ => None,
        })
        .collect();
    if !literals.is_empty() {
        return camel(&literals);
    }
    let vars: Vec<&str> = chunks
        .iter()
        .map(|c| match c {
            Chunk::Literal(l) => l.as_str(),
            Chunk::Var(v) | Chunk::Rest(v) => v.as_str(),
        })
        .collect();
    camel(&vars)
}

fn snake(name: &str) -> String {
    name.replace(['-', '.'], "_")
}

fn producer_module(name: &str) -> String {
    snake(name)
}

/// Parse-precedence: at the first differing position, literal beats var
/// beats rest; shorter fixed arity ties break by pattern text (stable).
fn precedence_rank(c: &Chunk) -> u8 {
    match c {
        Chunk::Literal(_) => 0,
        Chunk::Var(_) => 1,
        Chunk::Rest(_) => 2,
    }
}

fn load_registry(dir: &Path) -> Vec<RegistryFile> {
    let mut files = Vec::new();
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("registry/ directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&text)
            .unwrap_or_else(|e| fail(&fname, &format!("TOML parse error: {e}")));

        // [registry] header (RFC 08 §2).
        let header = doc
            .get("registry")
            .unwrap_or_else(|| fail(&fname, "missing [registry] header"));
        for field in ["version", "app"] {
            if header.get(field).and_then(|v| v.as_str()).is_none() {
                fail(
                    &fname,
                    &format!("[registry] missing string field {field:?}"),
                );
            }
        }
        if header.get("convention").and_then(|v| v.as_integer()) != Some(1) {
            fail(&fname, "[registry] convention must be 1 for this crate");
        }

        let (name, service_origin) = if let Some(svc) = doc.get("service") {
            let name = svc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, "[service] missing name"));
            let origin = svc
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, "[service] missing origin"));
            if !is_valid_verbatim_chunk(origin) {
                fail(
                    &fname,
                    &format!("[service] origin {origin:?} is not a verbatim chunk"),
                );
            }
            (name.to_string(), Some(origin.to_string()))
        } else {
            let prod = doc
                .get("producer")
                .unwrap_or_else(|| fail(&fname, "missing [producer] or [service]"));
            let name = prod
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, "[producer] missing name"));
            if !is_valid_plain_chunk(name) {
                fail(
                    &fname,
                    &format!("producer name {name:?} violates RFC 03 §2"),
                );
            }
            if name.rsplit_once('-').is_some_and(|(b, t)| {
                !b.is_empty() && t.bytes().all(|c| c.is_ascii_digit()) && !t.is_empty()
            }) {
                fail(
                    &fname,
                    &format!("producer name {name:?} ends in -<int> (RFC 03 §1.5)"),
                );
            }
            if ["artifact", "tree", "store"].contains(&name) {
                fail(
                    &fname,
                    &format!("producer name {name:?} is a reserved blob tier token"),
                );
            }
            (name.to_string(), None)
        };

        let empty = Vec::new();
        let subject_entries = doc
            .get("subject")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let mut subjects = Vec::new();
        for entry in subject_entries {
            let spath = entry
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, "[[subject]] missing path"));
            let chunks = parse_pattern(&fname, spath);
            let class = entry
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, &format!("{spath:?}: missing class")));
            if !["telemetry", "state", "events"].contains(&class) {
                fail(&fname, &format!("{spath:?}: unknown class {class:?}"));
            }
            let default_qos = match class {
                "telemetry" => "sampled",
                "state" => "refreshed",
                _ => "transition",
            };
            let qos = entry
                .get("qos")
                .and_then(|v| v.as_str())
                .unwrap_or(default_qos)
                .to_string();
            if !["sampled", "refreshed", "transition", "alert", "frame"].contains(&qos.as_str()) {
                fail(
                    &fname,
                    &format!("{spath:?}: unknown qos profile {qos:?} (RFC 04 §3)"),
                );
            }
            // RFC 08 §2: `type` is required — it is what binds one payload type
            // to every expansion of the pattern (P5), and what lets a consumer
            // decode a wildcard result set without sniffing.
            let payload_type = entry
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, &format!("{spath:?}: missing type (RFC 08 §2)")))
                .to_string();
            let unit = entry
                .get("unit")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let cardinality = entry.get("cardinality").and_then(|v| v.as_integer());
            let has_var = chunks.iter().any(|c| !matches!(c, Chunk::Literal(_)));
            if has_var && cardinality.is_none() {
                fail(
                    &fname,
                    &format!("{spath:?}: {{var}} pattern needs integer cardinality (RFC 08 §5)"),
                );
            }
            let ttl_s = entry.get("ttl_s").and_then(|v| v.as_integer());
            if class == "state" && ttl_s.is_none() {
                fail(
                    &fname,
                    &format!("{spath:?}: state subject needs ttl_s (RFC 08 §5)"),
                );
            }
            let rate = entry
                .get("rate")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if class == "events" {
                match rate.as_deref() {
                    Some("rare") | Some("low") => {}
                    Some(r) if r.starts_with("burst(") && r.ends_with("/h)") => {}
                    _ => fail(
                        &fname,
                        &format!(
                            "{spath:?}: events subject needs rate rare|low|burst(n/h) (RFC 08 §5)"
                        ),
                    ),
                }
            }
            if entry.get("description").and_then(|v| v.as_str()).is_none()
                || entry.get("since").and_then(|v| v.as_str()).is_none()
            {
                fail(&fname, &format!("{spath:?}: missing description/since"));
            }
            // `variant` overrides the derived name. Two patterns with the same
            // literal chunks but different arity (`cpu/usage` vs
            // `cpu/{core}/usage`) derive the same name and would otherwise trip
            // the collision lint below with no way out.
            let variant = match entry.get("variant").and_then(|v| v.as_str()) {
                Some(v) if is_valid_variant(v) => v.to_string(),
                Some(v) => fail(
                    &fname,
                    &format!("{spath:?}: variant {v:?} is not a CamelCase identifier"),
                ),
                None => variant_name(&chunks),
            };
            subjects.push(SubjectEntry {
                path: spath.to_string(),
                variant,
                chunks,
                class: class.to_string(),
                payload_type,
                unit,
                cardinality,
                qos,
                ttl_s,
                rate,
            });
        }

        // Variant-name and exact-path collisions.
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for s in &subjects {
            if let Some(other) = seen.insert(s.variant.as_str(), s.path.as_str()) {
                fail(
                    &fname,
                    &format!(
                        "subjects {other:?} and {:?} collide on variant {:?}",
                        s.path, s.variant
                    ),
                );
            }
        }
        let mut paths_seen = std::collections::BTreeSet::new();
        for s in &subjects {
            if !paths_seen.insert(&s.path) {
                fail(&fname, &format!("duplicate subject path {:?}", s.path));
            }
        }

        let procedure_entries = doc
            .get("procedure")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let mut procedures = Vec::new();
        for entry in procedure_entries {
            let ppath = entry
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, "[[procedure]] missing path"));
            let chunks: Vec<String> = ppath.split('/').map(str::to_string).collect();
            for c in &chunks {
                if !is_valid_plain_chunk(c) {
                    fail(
                        &fname,
                        &format!(
                            "procedure {ppath:?}: chunk {c:?} violates RFC 03 §2 (procedure paths are literal; parameters ride the selector)"
                        ),
                    );
                }
            }
            let kind = entry
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| fail(&fname, &format!("procedure {ppath:?}: missing kind")));
            if !["read", "write", "long-running"].contains(&kind) {
                fail(
                    &fname,
                    &format!("procedure {ppath:?}: unknown kind {kind:?}"),
                );
            }
            let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            procedures.push(ProcedureEntry {
                path: ppath.to_string(),
                variant: camel(&refs),
                chunks,
                kind: kind.to_string(),
                request: entry
                    .get("request")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                reply: entry
                    .get("reply")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            });
        }

        // [[media]] entries: validated shape only (patterns), no codegen yet.
        if let Some(media) = doc.get("media").and_then(|v| v.as_array()) {
            for entry in media {
                let mpath = entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| fail(&fname, "[[media]] missing path"));
                parse_pattern(&fname, mpath);
            }
        }

        let deprecated = doc
            .get("deprecated")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|entry| {
                        entry
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or_else(|| fail(&fname, "[[deprecated]] missing path"))
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Deprecated paths may never be re-registered as live subjects.
        for d in &deprecated {
            if subjects.iter().any(|s| &s.path == d) {
                fail(
                    &fname,
                    &format!("deprecated path {d:?} re-registered as a live subject (RFC 08 §3)"),
                );
            }
        }

        files.push(RegistryFile {
            name,
            service_origin,
            toml_path: path.canonicalize().unwrap().to_string_lossy().to_string(),
            subjects,
            procedures,
            deprecated,
        });
    }
    files
}

/// The append-only deprecation ledger (RFC 08 §3/§5): every line is
/// `<file-name>\t<path>`. A ledger line without its TOML entry = someone
/// deleted a deprecation; a TOML deprecation missing from the ledger = the
/// ledger append was forgotten. Both fail the build.
fn check_deprecation_ledger(dir: &Path, files: &[RegistryFile]) {
    let ledger_path = dir.join("deprecated.lock");
    let ledger = std::fs::read_to_string(&ledger_path).unwrap_or_default();
    let ledger_entries: Vec<(&str, &str)> = ledger
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            l.split_once('\t')
                .unwrap_or_else(|| panic!("bad ledger line {l:?}"))
        })
        .collect();
    for (producer, path) in &ledger_entries {
        let present = files
            .iter()
            .any(|f| f.name == *producer && f.deprecated.iter().any(|d| d == path));
        if !present {
            fail(
                "deprecated.lock",
                &format!(
                    "ledger entry {producer}\t{path} has no [[deprecated] ] entry — deprecations are append-only, restore it (RFC 08 §3)"
                ),
            );
        }
    }
    for f in files {
        for d in &f.deprecated {
            let listed = ledger_entries
                .iter()
                .any(|(p, path)| *p == f.name && path == d);
            if !listed {
                fail(
                    "deprecated.lock",
                    &format!(
                        "[[deprecated]] {d:?} in {} is not in the ledger — append `{}\t{d}` to registry/deprecated.lock",
                        f.name, f.name
                    ),
                );
            }
        }
    }
}

fn emit(files: &[RegistryFile]) -> String {
    let mut out = String::from(
        "// @generated by zensight-keyspace/build.rs from registry/*.toml — do not edit.\n\n",
    );
    for f in files {
        let module = producer_module(&f.name);
        let _ = writeln!(out, "pub mod {module} {{");
        let _ = writeln!(out, "    #[allow(unused_imports)]");
        let _ = writeln!(
            out,
            "    use crate::grammar::{{self, Class, KeyError, Origin, Producer}};"
        );
        let _ = writeln!(out, "    use crate::qos::QosProfile;");
        let _ = writeln!(
            out,
            "\n    /// The raw registry slice — served verbatim by `introspect` (RFC 08 §6).\n    pub const REGISTRY_TOML: &str = include_str!({:?});\n",
            f.toml_path
        );
        if let Some(origin) = &f.service_origin {
            let _ = writeln!(
                out,
                "    /// This is a service registry: keys omit the producer chunk.\n    pub fn origin() -> Origin {{ Origin::service({origin:?}).expect(\"validated at build time\") }}\n"
            );
        } else {
            let _ = writeln!(
                out,
                "    pub fn producer() -> Producer {{ Producer::new({:?}).expect(\"validated at build time\") }}\n",
                f.name
            );
        }

        // Subject enum.
        let _ = writeln!(out, "    #[derive(Debug, Clone, PartialEq, Eq)]");
        let _ = writeln!(out, "    pub enum Subject {{");
        for s in &f.subjects {
            let mut fields = String::new();
            for c in &s.chunks {
                match c {
                    Chunk::Var(v) => {
                        let _ = write!(fields, "{}: String, ", snake(v));
                    }
                    Chunk::Rest(v) => {
                        let _ = write!(fields, "{}: Vec<String>, ", snake(v));
                    }
                    Chunk::Literal(_) => {}
                }
            }
            if fields.is_empty() {
                let _ = writeln!(out, "        {},", s.variant);
            } else {
                let _ = writeln!(out, "        {} {{ {}}},", s.variant, fields);
            }
        }
        let _ = writeln!(out, "    }}\n");

        // impl Subject.
        let _ = writeln!(out, "    impl Subject {{");

        // class()
        let _ = writeln!(out, "        pub fn class(&self) -> Class {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let class = match s.class.as_str() {
                "telemetry" => "Class::Telemetry",
                "state" => "Class::State",
                _ => "Class::Events",
            };
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {class},",
                s.variant
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // qos()
        let _ = writeln!(out, "        pub fn qos(&self) -> QosProfile {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let qos = match s.qos.as_str() {
                "sampled" => "QosProfile::Sampled",
                "refreshed" => "QosProfile::Refreshed",
                "transition" => "QosProfile::Transition",
                "alert" => "QosProfile::Alert",
                _ => "QosProfile::Frame",
            };
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {qos},",
                s.variant
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // ttl_s()
        let _ = writeln!(out, "        pub fn ttl_s(&self) -> Option<u64> {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let ttl = match s.ttl_s {
                Some(t) => format!("Some({t})"),
                None => "None".to_string(),
            };
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {ttl},",
                s.variant
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // rate() — events rate class (RFC 04 §1.3), None for other classes.
        let _ = writeln!(out, "        pub fn rate(&self) -> Option<&'static str> {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let rate = match &s.rate {
                Some(r) => format!("Some({r:?})"),
                None => "None".to_string(),
            };
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {rate},",
                s.variant
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // payload_type() — the registry `type` column (RFC 08 §2). One payload
        // type per pattern (P5), so this is what a consumer decodes as.
        let _ = writeln!(
            out,
            "        /// The one payload type of every expansion of this subject (RFC 08 §2)."
        );
        let _ = writeln!(out, "        pub fn payload_type(&self) -> &'static str {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {:?},",
                s.variant, s.payload_type
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // unit() — RFC 08 §4: the registry is authoritative for units, whether
        // or not the key spells one as a suffix.
        let _ = writeln!(out, "        pub fn unit(&self) -> Option<&'static str> {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let unit = match &s.unit {
                Some(u) => format!("Some({u:?})"),
                None => "None".to_string(),
            };
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {unit},",
                s.variant
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // cardinality() — the key-population bound the budget review enforces.
        let _ = writeln!(out, "        pub fn cardinality(&self) -> Option<u64> {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let c = match s.cardinality {
                Some(c) => format!("Some({c})"),
                None => "None".to_string(),
            };
            let _ = writeln!(out, "                Self::{} {{ .. }} => {c},", s.variant);
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // pattern()
        let _ = writeln!(out, "        pub fn pattern(&self) -> &'static str {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let _ = writeln!(
                out,
                "                Self::{} {{ .. }} => {:?},",
                s.variant, s.path
            );
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // chunks()
        let _ = writeln!(out, "        pub fn chunks(&self) -> Vec<String> {{");
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let mut binds = String::new();
            let mut body = String::from("vec![");
            for c in &s.chunks {
                match c {
                    Chunk::Literal(l) => {
                        let _ = write!(body, "{l:?}.to_string(), ");
                    }
                    Chunk::Var(v) => {
                        let n = snake(v);
                        let _ = write!(binds, "{n}, ");
                        let _ = write!(body, "{n}.clone(), ");
                    }
                    Chunk::Rest(_) => {}
                }
            }
            body.push(']');
            if let Some(Chunk::Rest(v)) = s.chunks.last() {
                let n = snake(v);
                let _ = write!(binds, "{n}, ");
                body = format!("{{ let mut c = {body}; c.extend({n}.iter().cloned()); c }}");
            }
            let pat = if binds.is_empty() {
                format!("Self::{}", s.variant)
            } else {
                format!("Self::{} {{ {binds}}}", s.variant)
            };
            let _ = writeln!(out, "                {pat} => {body},");
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // vars() — the variable bindings, in pattern order. A `{var...}` rest
        // binds as its chunks rejoined with `/`.
        let _ = writeln!(
            out,
            "        /// The pattern's variable bindings, in order (a `{{var...}}` rest rejoins with `/`)."
        );
        let _ = writeln!(
            out,
            "        pub fn vars(&self) -> Vec<(&'static str, String)> {{"
        );
        let _ = writeln!(out, "            match self {{");
        for s in &f.subjects {
            let mut binds = String::new();
            let mut body = String::from("vec![");
            for c in &s.chunks {
                match c {
                    Chunk::Literal(_) => {}
                    Chunk::Var(v) => {
                        let n = snake(v);
                        let _ = write!(binds, "{n}, ");
                        let _ = write!(body, "({v:?}, {n}.clone()), ");
                    }
                    Chunk::Rest(v) => {
                        let n = snake(v);
                        let _ = write!(binds, "{n}, ");
                        let _ = write!(body, "({v:?}, {n}.join(\"/\")), ");
                    }
                }
            }
            body.push(']');
            let pat = if binds.is_empty() {
                format!("Self::{}", s.variant)
            } else {
                format!("Self::{} {{ {binds}}}", s.variant)
            };
            let _ = writeln!(out, "                {pat} => {body},");
        }
        let _ = writeln!(out, "            }}\n        }}\n");

        // parse(), precedence-ordered (RFC 08 §1: literal beats {var} beats {var...}).
        let mut ordered: Vec<&SubjectEntry> = f.subjects.iter().collect();
        ordered.sort_by(|a, b| {
            let ranks =
                |s: &SubjectEntry| -> Vec<u8> { s.chunks.iter().map(precedence_rank).collect() };
            ranks(a).cmp(&ranks(b)).then_with(|| a.path.cmp(&b.path))
        });
        let _ = writeln!(
            out,
            "        /// Refine a subject tail (chunks after the producer position).\n        pub fn parse(class: Class, tail: &[&str]) -> Option<Self> {{"
        );
        for s in &ordered {
            let class = match s.class.as_str() {
                "telemetry" => "Class::Telemetry",
                "state" => "Class::State",
                _ => "Class::Events",
            };
            let has_rest = matches!(s.chunks.last(), Some(Chunk::Rest(_)));
            let fixed = if has_rest {
                s.chunks.len() - 1
            } else {
                s.chunks.len()
            };
            let len_cond = if has_rest {
                if fixed == 0 {
                    "!tail.is_empty()".to_string()
                } else {
                    format!("tail.len() > {fixed}")
                }
            } else {
                format!("tail.len() == {fixed}")
            };
            let mut conds = vec![format!("class == {class}"), len_cond];
            for (i, c) in s.chunks.iter().enumerate() {
                if let Chunk::Literal(l) = c {
                    conds.push(format!("tail[{i}] == {l:?}"));
                }
            }
            let mut fields = String::new();
            for (i, c) in s.chunks.iter().enumerate() {
                match c {
                    Chunk::Var(v) => {
                        let _ = write!(fields, "{}: tail[{i}].to_string(), ", snake(v));
                    }
                    Chunk::Rest(v) => {
                        let _ = write!(
                            fields,
                            "{}: tail[{i}..].iter().map(|c| c.to_string()).collect(), ",
                            snake(v)
                        );
                    }
                    Chunk::Literal(_) => {}
                }
            }
            let construct = if fields.is_empty() {
                format!("Self::{}", s.variant)
            } else {
                format!("Self::{} {{ {fields}}}", s.variant)
            };
            let _ = writeln!(
                out,
                "            if {} {{ return Some({construct}); }}",
                conds.join(" && ")
            );
        }
        let _ = writeln!(out, "            None\n        }}\n");

        // parse_metric() — the ergonomic entry point. Consumers hold the
        // telemetry tail as one string (`TelemetryPoint::metric` *is* the
        // subject tail, verbatim), so splitting it belongs here, once.
        let _ = writeln!(
            out,
            "        /// Refine a telemetry metric name (the subject tail as one `/`-joined string)."
        );
        let _ = writeln!(
            out,
            "        pub fn parse_metric(metric: &str) -> Option<Self> {{\n            let tail: Vec<&str> = metric.split('/').collect();\n            Self::parse(Class::Telemetry, &tail)\n        }}"
        );
        let _ = writeln!(out, "    }}\n");

        // Full-key builder.
        if f.service_origin.is_some() {
            let _ = writeln!(
                out,
                "    /// Base-relative data key for this service's subject.\n    pub fn key(subject: &Subject) -> Result<String, KeyError> {{\n        let chunks = subject.chunks();\n        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();\n        grammar::data_key(&origin(), subject.class(), None, &refs)\n    }}"
            );
        } else {
            let _ = writeln!(
                out,
                "    /// Base-relative data key for this producer's subject.\n    pub fn key(origin: &Origin, subject: &Subject) -> Result<String, KeyError> {{\n        key_as(origin, &producer(), subject)\n    }}\n\n    /// As [`key`], for an instance-suffixed producer (RFC 03 §1.5).\n    pub fn key_as(origin: &Origin, producer: &Producer, subject: &Subject) -> Result<String, KeyError> {{\n        let chunks = subject.chunks();\n        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();\n        grammar::data_key(origin, subject.class(), Some(producer), &refs)\n    }}"
            );
        }

        // Procedures.
        if !f.procedures.is_empty() {
            let _ = writeln!(out, "\n    #[derive(Debug, Clone, Copy, PartialEq, Eq)]");
            let _ = writeln!(out, "    pub enum ProcedureId {{");
            for p in &f.procedures {
                let _ = writeln!(out, "        /// `{}` ({})", p.path, p.kind);
                let _ = writeln!(out, "        {},", p.variant);
            }
            let _ = writeln!(out, "    }}\n");
            let _ = writeln!(out, "    impl ProcedureId {{");
            let _ = writeln!(out, "        pub const ALL: &[ProcedureId] = &[");
            for p in &f.procedures {
                let _ = writeln!(out, "            ProcedureId::{},", p.variant);
            }
            let _ = writeln!(out, "        ];\n");
            let _ = writeln!(
                out,
                "        pub fn chunks(self) -> &'static [&'static str] {{"
            );
            let _ = writeln!(out, "            match self {{");
            for p in &f.procedures {
                let list: Vec<String> = p.chunks.iter().map(|c| format!("{c:?}")).collect();
                let _ = writeln!(
                    out,
                    "                Self::{} => &[{}],",
                    p.variant,
                    list.join(", ")
                );
            }
            let _ = writeln!(out, "            }}\n        }}\n");
            let _ = writeln!(out, "        pub fn kind(self) -> &'static str {{");
            let _ = writeln!(out, "            match self {{");
            for p in &f.procedures {
                let _ = writeln!(out, "                Self::{} => {:?},", p.variant, p.kind);
            }
            let _ = writeln!(out, "            }}\n        }}\n");
            let _ = writeln!(
                out,
                "        /// The registered path, e.g. `artifact/request`."
            );
            let _ = writeln!(out, "        pub fn path(self) -> &'static str {{");
            let _ = writeln!(out, "            match self {{");
            for p in &f.procedures {
                let _ = writeln!(out, "                Self::{} => {:?},", p.variant, p.path);
            }
            let _ = writeln!(out, "            }}\n        }}\n");
            // request_type()/reply_type() — RFC 08 §2. A `read` has no request
            // body; a write that acks with nothing has no reply type.
            for (method, field) in [
                ("request_type", &|p: &ProcedureEntry| {
                    p.request.clone() as Option<String>
                }),
                ("reply_type", &|p: &ProcedureEntry| {
                    p.reply.clone() as Option<String>
                }),
            ]
                as [(&str, &dyn Fn(&ProcedureEntry) -> Option<String>); 2]
            {
                let _ = writeln!(
                    out,
                    "        pub fn {method}(self) -> Option<&'static str> {{"
                );
                let _ = writeln!(out, "            match self {{");
                for p in &f.procedures {
                    let t = match field(p) {
                        Some(t) => format!("Some({t:?})"),
                        None => "None".to_string(),
                    };
                    let _ = writeln!(out, "                Self::{} => {t},", p.variant);
                }
                let _ = writeln!(out, "            }}\n        }}\n");
            }
            let _ = writeln!(out, "    }}\n");
            if f.service_origin.is_some() {
                let _ = writeln!(
                    out,
                    "    /// Base-relative `@rpc` key for this service's procedure.\n    pub fn rpc_key(p: ProcedureId) -> Result<String, KeyError> {{\n        grammar::rpc_key(&origin(), None, p.chunks())\n    }}"
                );
            } else {
                let _ = writeln!(
                    out,
                    "    /// Base-relative `@rpc` key for this producer's procedure.\n    pub fn rpc_key(origin: &Origin, p: ProcedureId) -> Result<String, KeyError> {{\n        grammar::rpc_key(origin, Some(&producer()), p.chunks())\n    }}"
                );
            }
        }

        let _ = writeln!(out, "}}\n");
    }

    // Raw registry slice by producer name (introspect, RFC 08 §6).
    let _ = writeln!(
        out,
        "/// The raw registry slice for a producer/service, by base name."
    );
    let _ = writeln!(
        out,
        "pub fn registry_toml(name: &str) -> Option<&'static str> {{"
    );
    let _ = writeln!(out, "    match name {{");
    for f in files {
        let module = producer_module(&f.name);
        let _ = writeln!(
            out,
            "        {:?} => Some({module}::REGISTRY_TOML),",
            f.name
        );
    }
    let _ = writeln!(out, "        _ => None,\n    }}\n}}\n");

    // Every registry slice this build was compiled against, by base name. The
    // fleet-capabilities view (#469) fans `introspect` out over exactly this
    // list and diffs each reply against the local slice.
    let _ = writeln!(
        out,
        "/// Every producer/service this build knows, with its raw registry slice."
    );
    let _ = writeln!(out, "pub const REGISTRIES: &[(&str, &str)] = &[");
    for f in files {
        let module = producer_module(&f.name);
        let _ = writeln!(out, "    ({:?}, {module}::REGISTRY_TOML),", f.name);
    }
    let _ = writeln!(out, "];\n");

    // Cross-producer dispatch: refine a structural key into (producer, subject).
    let _ = writeln!(out, "/// Refined subject from any registered producer.");
    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
    let _ = writeln!(out, "pub enum AnySubject {{");
    for f in files {
        let module = producer_module(&f.name);
        let _ = writeln!(out, "    {}({module}::Subject),", camel(&[f.name.as_str()]));
    }
    let _ = writeln!(out, "}}\n");

    // AnySubject accessors: producer-agnostic introspection of a refined
    // subject. `vars()` is what lets a consumer read `{iface}` by name instead
    // of by position.
    let _ = writeln!(out, "impl AnySubject {{");
    for (method, ret, body) in [
        ("producer_name", "&'static str", "NAME"),
        ("class", "crate::grammar::Class", "s.class()"),
        ("pattern", "&'static str", "s.pattern()"),
        ("vars", "Vec<(&'static str, String)>", "s.vars()"),
        ("payload_type", "&'static str", "s.payload_type()"),
        ("unit", "Option<&'static str>", "s.unit()"),
        ("cardinality", "Option<u64>", "s.cardinality()"),
        ("qos", "crate::qos::QosProfile", "s.qos()"),
        ("ttl_s", "Option<u64>", "s.ttl_s()"),
        ("rate", "Option<&'static str>", "s.rate()"),
    ] {
        let _ = writeln!(out, "    pub fn {method}(&self) -> {ret} {{");
        let _ = writeln!(out, "        match self {{");
        for f in files {
            let variant = camel(&[f.name.as_str()]);
            let arm = if body == "NAME" {
                format!("{:?}", f.name)
            } else {
                body.to_string()
            };
            let bind = if body == "NAME" { "_" } else { "s" };
            let _ = writeln!(out, "            Self::{variant}({bind}) => {arm},");
        }
        let _ = writeln!(out, "        }}\n    }}\n");
    }
    let _ = writeln!(out, "}}\n");

    // AnySubject::common_state() — refine any producer's state subject into the
    // shared framework set (see src/common_state.rs). Driven off the registered
    // *pattern*, so it cannot drift from the registry: rename a subject in a
    // TOML and its arm disappears here.
    let common: &[(&str, &str)] = &[
        ("health", "Health"),
        ("errors", "Errors"),
        ("sensor", "Sensor"),
        ("alert/{alert_key}", "Alert { alert_key }"),
        ("artifact/{kind}", "Artifact { kind }"),
        ("evidence/self", "EvidenceSelf"),
        ("evidence/device/{device}", "EvidenceDevice { device }"),
        ("evidence/names/{ip_slug}", "EvidenceNames { ip_slug }"),
        ("stream/{stream}", "Stream { stream }"),
        ("entity/{entity_id}", "CatalogEntity { entity_id }"),
        ("alias/{old_id}", "CatalogAlias { old_id }"),
        ("pdns/{ip_slug}", "CatalogPdns { ip_slug }"),
        ("assertion/{id}", "CatalogAssertion { id }"),
    ];
    let _ = writeln!(out, "impl AnySubject {{");
    let _ = writeln!(
        out,
        "    /// The framework state subject this refines to, if any (issue #475).\n    pub fn common_state(&self) -> Option<crate::common_state::CommonState<'_>> {{\n        use crate::common_state::CommonState as C;\n        match self {{"
    );
    for f in files {
        let module = producer_module(&f.name);
        let variant = camel(&[f.name.as_str()]);
        let _ = writeln!(out, "            Self::{variant}(s) => match s {{");
        let mut mapped = 0usize;
        for s in &f.subjects {
            if s.class != "state" {
                continue;
            }
            let Some((_, ctor)) = common.iter().find(|(pat, _)| *pat == s.path) else {
                continue;
            };
            mapped += 1;
            // Bind the pattern's variables by name; the CommonState ctor uses
            // the same names, so field-init shorthand lines them up.
            let binds: Vec<String> = s
                .chunks
                .iter()
                .filter_map(|c| match c {
                    Chunk::Var(v) | Chunk::Rest(v) => Some(snake(v)),
                    Chunk::Literal(_) => None,
                })
                .collect();
            let pat = if binds.is_empty() {
                format!("{module}::Subject::{}", s.variant)
            } else {
                format!(
                    "{module}::Subject::{} {{ {} }}",
                    s.variant,
                    binds.join(", ")
                )
            };
            let _ = writeln!(out, "                {pat} => Some(C::{ctor}),");
        }
        // A producer whose every subject is a framework one (the `@catalog`
        // service) leaves nothing for a wildcard arm to catch.
        if mapped < f.subjects.len() {
            let _ = writeln!(out, "                _ => None,");
        }
        let _ = writeln!(out, "            }},");
    }
    let _ = writeln!(out, "        }}\n    }}\n}}\n");

    let _ = writeln!(
        out,
        "/// Registry-refine a structurally parsed key (RFC 08 §1 parse direction).\n/// `producer_name` is the *base* name (instance suffix already stripped by\n/// [`crate::grammar::Producer::parse_chunk`]); service keys pass the service\n/// name (e.g. \"catalog\").\npub fn parse_subject(producer_name: &str, class: crate::grammar::Class, tail: &[&str]) -> Option<AnySubject> {{"
    );
    let _ = writeln!(out, "    match producer_name {{");
    for f in files {
        let module = producer_module(&f.name);
        let _ = writeln!(
            out,
            "        {:?} => {module}::Subject::parse(class, tail).map(AnySubject::{}),",
            f.name,
            camel(&[f.name.as_str()])
        );
    }
    let _ = writeln!(out, "        _ => None,\n    }}\n}}");

    out
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dir = Path::new(&manifest).join("registry");
    println!("cargo::rerun-if-changed=registry");
    println!("cargo::rerun-if-changed=src/chunk_rules.rs");
    let files = load_registry(&dir);
    check_deprecation_ledger(&dir, &files);
    let generated = emit(&files);
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("registry_gen.rs");
    std::fs::write(out, generated).unwrap();
}
