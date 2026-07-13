//! `RegistrySlice` — the reply type of the `introspect` procedure (RFC 08 §6).
//!
//! Every producer's registry file has declared `reply = "RegistrySlice"` since
//! the convention was ratified, and every sensor has answered `introspect` with
//! the raw registry TOML — but the type it named did not exist, so no consumer
//! could read the answer. This is that type.
//!
//! A slice is what one build *says* it serves. The point of having it is the
//! diff: compare a host's served slice against the slice this build compiled
//! in ([`REGISTRIES`](crate::registry::REGISTRIES)) and a disagreement is a
//! **finding** — a version skew, a subject the fleet serves that we cannot
//! name, or a subject we expect that nothing out there publishes. RFC 08 §6 is
//! explicit that this is a finding and not an ambiguity, which is why the
//! parser below is strict about the header and forgiving about nothing.

use std::fmt;

/// One `[[subject]]` entry of a served registry slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectDecl {
    /// The subject pattern, base-relative to `<class>/<producer>` — e.g.
    /// `disk/{mount}/used`.
    pub path: String,
    /// `telemetry` | `state` | `events`.
    pub class: String,
    /// The payload type name, as the producer declares it.
    pub type_name: String,
    /// Registry version this subject first appeared in.
    pub since: Option<String>,
    pub description: Option<String>,
}

/// One `[[procedure]]` entry of a served registry slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcedureDecl {
    /// The procedure path, base-relative to the producer's `@rpc` root.
    pub path: String,
    /// `read` | `write`.
    pub kind: String,
    pub reply: Option<String>,
    pub since: Option<String>,
    pub description: Option<String>,
}

/// One `[[deprecated]]` entry — RFC 08 §3's append-only retirement ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeprecationDecl {
    pub path: String,
    /// Registry version the retirement was recorded in.
    pub since: Option<String>,
    /// What replaced it, if anything.
    pub replaced_by: Option<String>,
}

/// What one build says it serves: the payload of an `introspect` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySlice {
    /// `[registry] version` — the number a version-skew check compares.
    pub version: String,
    pub app: String,
    /// The convention major version (`1` for keyspace-v2 as ratified).
    pub convention: i64,
    /// The producer or service base name (`sysinfo`, `catalog`, …).
    pub name: String,
    /// `Some(origin)` for a service (`@catalog`); `None` for a host producer,
    /// whose origin is the host it runs on and therefore not in the slice.
    pub service_origin: Option<String>,
    pub description: Option<String>,
    pub subjects: Vec<SubjectDecl>,
    pub procedures: Vec<ProcedureDecl>,
    pub deprecated: Vec<DeprecationDecl>,
}

impl RegistrySlice {
    /// Subjects of one class.
    pub fn subjects_in(&self, class: &str) -> impl Iterator<Item = &SubjectDecl> {
        self.subjects.iter().filter(move |s| s.class == class)
    }

    /// Does this slice serve a subject with exactly this pattern?
    pub fn serves_subject(&self, path: &str) -> bool {
        self.subjects.iter().any(|s| s.path == path)
    }

    /// Does this slice serve this procedure?
    pub fn serves_procedure(&self, path: &str) -> bool {
        self.procedures.iter().any(|p| p.path == path)
    }
}

/// Why a slice would not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceError(String);

impl fmt::Display for SliceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed registry slice: {}", self.0)
    }
}

impl std::error::Error for SliceError {}

/// Parse an `introspect` reply — the raw registry TOML a build serves.
///
/// Deliberately tolerant of *unknown* keys (a newer fleet member may declare
/// fields this build has never heard of, and refusing to read the rest of its
/// slice would turn a forward-compatible addition into an outage of the very
/// view that exists to spot skew) and intolerant of *missing* ones (a slice
/// without a version cannot be diffed, which is the whole point).
pub fn parse_slice(toml_src: &str) -> Result<RegistrySlice, SliceError> {
    let doc: toml::Value = toml::from_str(toml_src).map_err(|e| SliceError(e.to_string()))?;

    let err = |m: &str| SliceError(m.to_string());
    let s = |v: Option<&toml::Value>| v.and_then(|v| v.as_str()).map(str::to_string);

    let header = doc
        .get("registry")
        .ok_or_else(|| err("missing [registry]"))?;
    let version = s(header.get("version")).ok_or_else(|| err("[registry] missing version"))?;
    let app = s(header.get("app")).ok_or_else(|| err("[registry] missing app"))?;
    let convention = header
        .get("convention")
        .and_then(|v| v.as_integer())
        .ok_or_else(|| err("[registry] missing convention"))?;

    let (name, service_origin, description) = if let Some(svc) = doc.get("service") {
        (
            s(svc.get("name")).ok_or_else(|| err("[service] missing name"))?,
            Some(s(svc.get("origin")).ok_or_else(|| err("[service] missing origin"))?),
            s(svc.get("description")),
        )
    } else if let Some(prod) = doc.get("producer") {
        (
            s(prod.get("name")).ok_or_else(|| err("[producer] missing name"))?,
            None,
            s(prod.get("description")),
        )
    } else {
        return Err(err("missing [producer] or [service]"));
    };

    let array = |key: &str| -> Vec<&toml::Value> {
        doc.get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };

    let mut subjects = Vec::new();
    for e in array("subject") {
        subjects.push(SubjectDecl {
            path: s(e.get("path")).ok_or_else(|| err("[[subject]] missing path"))?,
            class: s(e.get("class")).ok_or_else(|| err("[[subject]] missing class"))?,
            type_name: s(e.get("type")).unwrap_or_default(),
            since: s(e.get("since")),
            description: s(e.get("description")),
        });
    }

    let mut procedures = Vec::new();
    for e in array("procedure") {
        procedures.push(ProcedureDecl {
            path: s(e.get("path")).ok_or_else(|| err("[[procedure]] missing path"))?,
            kind: s(e.get("kind")).unwrap_or_default(),
            reply: s(e.get("reply")),
            since: s(e.get("since")),
            description: s(e.get("description")),
        });
    }

    let mut deprecated = Vec::new();
    for e in array("deprecated") {
        deprecated.push(DeprecationDecl {
            path: s(e.get("path")).ok_or_else(|| err("[[deprecated]] missing path"))?,
            since: s(e.get("since")),
            replaced_by: s(e.get("replaced_by")),
        });
    }

    Ok(RegistrySlice {
        version,
        app,
        convention,
        name,
        service_origin,
        description,
        subjects,
        procedures,
        deprecated,
    })
}

/// A disagreement between a served slice and the slice this build compiled in.
///
/// RFC 08 §6: a disagreement is a finding. Each variant is one thing an
/// operator would otherwise have to SSH to a host to learn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceFinding {
    /// The served `[registry] version` differs from ours.
    VersionSkew {
        served: String,
        local: String,
    },
    /// The host serves a subject we do not know — it is newer than us.
    UnknownSubject {
        path: String,
        class: String,
    },
    /// We know a subject the host does not serve — it is older than us.
    MissingSubject {
        path: String,
        class: String,
    },
    /// Likewise for procedures.
    UnknownProcedure {
        path: String,
    },
    MissingProcedure {
        path: String,
    },
    /// The host still serves a subject its own ledger marks deprecated.
    ServesDeprecated {
        path: String,
        replaced_by: Option<String>,
    },
}

impl SliceFinding {
    /// One line, for a table cell.
    pub fn summary(&self) -> String {
        match self {
            Self::VersionSkew { served, local } => {
                format!("registry {served} (we compiled {local})")
            }
            Self::UnknownSubject { path, class } => format!("serves unknown {class} {path}"),
            Self::MissingSubject { path, class } => format!("does not serve {class} {path}"),
            Self::UnknownProcedure { path } => format!("serves unknown procedure {path}"),
            Self::MissingProcedure { path } => format!("does not serve procedure {path}"),
            Self::ServesDeprecated { path, replaced_by } => match replaced_by {
                Some(r) => format!("serves deprecated {path} (use {r})"),
                None => format!("serves deprecated {path}"),
            },
        }
    }
}

/// Diff a served slice against the slice this build compiled in.
///
/// Empty means the host agrees with us exactly, which is the answer the view
/// wants to be able to give in one glance.
pub fn diff(served: &RegistrySlice, local: &RegistrySlice) -> Vec<SliceFinding> {
    let mut out = Vec::new();
    if served.version != local.version {
        out.push(SliceFinding::VersionSkew {
            served: served.version.clone(),
            local: local.version.clone(),
        });
    }
    for s in &served.subjects {
        if !local.serves_subject(&s.path) {
            out.push(SliceFinding::UnknownSubject {
                path: s.path.clone(),
                class: s.class.clone(),
            });
        }
    }
    for s in &local.subjects {
        if !served.serves_subject(&s.path) {
            out.push(SliceFinding::MissingSubject {
                path: s.path.clone(),
                class: s.class.clone(),
            });
        }
    }
    for p in &served.procedures {
        if !local.serves_procedure(&p.path) {
            out.push(SliceFinding::UnknownProcedure {
                path: p.path.clone(),
            });
        }
    }
    for p in &local.procedures {
        if !served.serves_procedure(&p.path) {
            out.push(SliceFinding::MissingProcedure {
                path: p.path.clone(),
            });
        }
    }
    // A host that still serves what its own ledger retired. Quiet until the
    // first deprecation lands, and exactly the question that needs SSH today.
    for d in &served.deprecated {
        if served.serves_subject(&d.path) || served.serves_procedure(&d.path) {
            out.push(SliceFinding::ServesDeprecated {
                path: d.path.clone(),
                replaced_by: d.replaced_by.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slices this build compiled in must all parse — they are the ones we
    /// serve, so if they do not round-trip, every `introspect` reply on the bus
    /// is unreadable.
    #[test]
    fn every_compiled_slice_parses() {
        assert!(!crate::registry::REGISTRIES.is_empty());
        for (name, toml_src) in crate::registry::REGISTRIES {
            let slice = parse_slice(toml_src)
                .unwrap_or_else(|e| panic!("registry slice {name} does not parse: {e}"));
            assert_eq!(&slice.name, name);
            assert_eq!(slice.convention, 1);
            assert!(
                slice.serves_procedure("introspect"),
                "{name} must serve introspect — it is how this slice reaches a consumer"
            );
        }
    }

    #[test]
    fn catalog_is_a_service_and_carries_its_origin() {
        let (_, toml_src) = crate::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == "catalog")
            .expect("catalog registry");
        let slice = parse_slice(toml_src).unwrap();
        assert_eq!(slice.service_origin.as_deref(), Some("@catalog"));
    }

    #[test]
    fn a_slice_identical_to_ours_is_no_finding() {
        let (_, toml_src) = crate::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == "sysinfo")
            .expect("sysinfo registry");
        let slice = parse_slice(toml_src).unwrap();
        assert!(diff(&slice, &slice).is_empty());
    }

    #[test]
    fn skew_and_drift_are_findings() {
        let local = parse_slice(
            r#"
            [registry]
            version = "1.1"
            app = "zensight"
            convention = 1
            [producer]
            name = "sysinfo"
            [[subject]]
            path = "cpu/usage"
            class = "telemetry"
            type = "TelemetryPoint"
            [[procedure]]
            path = "introspect"
            kind = "read"
            "#,
        )
        .unwrap();
        let served = parse_slice(
            r#"
            [registry]
            version = "1.2"
            app = "zensight"
            convention = 1
            [producer]
            name = "sysinfo"
            [[subject]]
            path = "cpu/temperature"
            class = "telemetry"
            type = "TelemetryPoint"
            [[procedure]]
            path = "introspect"
            kind = "read"
            "#,
        )
        .unwrap();

        let findings = diff(&served, &local);
        assert!(findings.iter().any(|f| matches!(
            f,
            SliceFinding::VersionSkew { served, local } if served == "1.2" && local == "1.1"
        )));
        assert!(findings.iter().any(
            |f| matches!(f, SliceFinding::UnknownSubject { path, .. } if path == "cpu/temperature")
        ));
        assert!(findings.iter().any(
            |f| matches!(f, SliceFinding::MissingSubject { path, .. } if path == "cpu/usage")
        ));
    }

    /// A field we have never heard of must not cost us the rest of the slice —
    /// otherwise the view that exists to spot a newer fleet member breaks on
    /// exactly the member it was built to find.
    #[test]
    fn unknown_fields_do_not_break_the_parse() {
        let slice = parse_slice(
            r#"
            [registry]
            version = "9.9"
            app = "zensight"
            convention = 1
            future_knob = true
            [producer]
            name = "sysinfo"
            [[subject]]
            path = "cpu/usage"
            class = "telemetry"
            type = "TelemetryPoint"
            unheard_of = "whatever"
            "#,
        )
        .unwrap();
        assert_eq!(slice.version, "9.9");
        assert!(slice.serves_subject("cpu/usage"));
    }

    #[test]
    fn a_slice_without_a_version_cannot_be_diffed_and_is_rejected() {
        let e = parse_slice(
            r#"
            [registry]
            app = "zensight"
            convention = 1
            [producer]
            name = "sysinfo"
            "#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("version"));
    }
}
