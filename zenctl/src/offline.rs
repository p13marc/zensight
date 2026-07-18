//! The offline half: everything answerable from the compiled-in registry.
//!
//! These commands need no bus, no session, and no live fleet. They describe
//! what the registry *declares* — which is a different question from what is
//! actually being published, and the output says so where the difference bites
//! (see the note on rest-variables in [`topic_list`]).

use anyhow::{Result, anyhow};
use zensight_keyspace::registry::REGISTRIES;
use zensight_keyspace::{RegistrySlice, parse_slice};

/// The offline default source: the registry slices this binary compiled
/// against, parsed. Both the offline commands and `doctor` share the same
/// [`REGISTRIES`] table; here it becomes the same `&[RegistrySlice]` shape the
/// bus-sourced path ([`crate::bus::fleet_registry`]) returns, so one renderer
/// serves both.
pub fn compiled_slices() -> Result<Vec<RegistrySlice>> {
    REGISTRIES
        .iter()
        .map(|(name, toml_src)| {
            parse_slice(toml_src)
                .map_err(|e| anyhow!("registry slice for {name} does not parse: {e}"))
        })
        .collect()
}

/// `topic list` — every registered subject in the given slices.
///
/// The slices come from either the compiled-in registry ([`compiled_slices`])
/// or the live bus ([`crate::bus::fleet_registry`]); the rendering is identical,
/// which is the point — a slice is a slice regardless of where it was read.
///
/// **Declared, not observed.** A pattern with a trailing rest-variable
/// (`{path...}`) stands for a whole family whose real members only exist on the
/// wire: the four proxy producers (snmp/modbus/gnmi/netflow) register
/// `{device}/{path...}` by design, because their metric tree belongs to the
/// polled device, not to us. For those, this command can only tell you the
/// shape. `zenctl topic echo` is what tells you the members.
pub fn topic_list(slices: &[RegistrySlice], producer: Option<&str>, class: Option<&str>) -> Result<()> {
    if let Some(c) = class
        && !["telemetry", "state", "events"].contains(&c)
    {
        return Err(anyhow!(
            "unknown class {c:?} — the classes are telemetry, state, events (RFC 04 §1)"
        ));
    }

    let mut total = 0usize;
    let mut open_ended = 0usize;

    for slice in slices {
        let name = &slice.name;
        if producer.is_some_and(|p| p != name) {
            continue;
        }

        let subjects: Vec<_> = slice
            .subjects
            .iter()
            .filter(|s| class.is_none_or(|c| c == s.class))
            .collect();
        if subjects.is_empty() {
            continue;
        }

        println!("\n{name}  (registry {})", slice.version);
        for s in subjects {
            let open = s.path.contains("...");
            if open {
                open_ended += 1;
            }
            println!(
                "  {:<10} {:<44} {}{}",
                s.class,
                s.path,
                s.type_name,
                if open { "  [open-ended]" } else { "" }
            );
            total += 1;
        }
    }

    if total == 0 {
        println!("no subjects match.");
        return Ok(());
    }
    println!("\n{total} registered subject(s).");
    if open_ended > 0 {
        let is = if open_ended == 1 { "is" } else { "are" };
        println!(
            "{open_ended} {is} open-ended ({{var...}}): the registry fixes their shape, not their\n\
             members. Use `zenctl topic echo` to see what a live fleet actually publishes."
        );
    }
    Ok(())
}

/// `topic info` — refine one concrete wire key through the registry.
///
/// This is the registry's *parse* direction (RFC 08 §1): the thing that
/// replaces positional `split('/')` re-parsing. The variables come back
/// **named**, which is why the output can say `mount=root` rather than
/// `parts[6]`.
pub fn topic_info(base: &str, key: &str) -> Result<()> {
    let Some((structural, producer, subject)) =
        zensight_common::keyexpr::refine_full_key(base, key)
    else {
        // Distinguish the two failure modes: a key that is not v1-shaped at all,
        // versus one that parses structurally but names an unregistered subject.
        // RFC 08: "a subject that is not registered does not exist."
        return match zensight_common::keyexpr::parse_full_key(base, key) {
            Some(s) => Err(anyhow!(
                "key parses as v1 ({}), but its subject is not registered — \
                 a subject that is not registered does not exist (RFC 08).\n\
                 Either the producer is publishing something it never declared, or this build's \
                 registry is older than the fleet's (try `zenctl doctor`).",
                match s.class {
                    zensight_keyspace::grammar::ClassOrPlane::Class(c) => c.chunk(),
                    zensight_keyspace::grammar::ClassOrPlane::Plane(p) => p.chunk(),
                }
            )),
            None => Err(anyhow!(
                "not a v1 key. Expected <base>/v1/<origin>/<class>/<producer>/<subject...> \
                 (RFC 03 §1)."
            )),
        };
    };

    println!("key       {key}");
    println!("origin    {}", structural.origin.chunk());
    println!("producer  {producer}");
    println!("class     {}", subject.class().chunk());
    println!("subject   {}", subject.pattern());

    let vars = subject.vars();
    if !vars.is_empty() {
        println!("variables");
        for (name, value) in vars {
            println!("  {name} = {value}");
        }
    }

    println!("payload   {}", subject.payload_type());
    if let Some(loc) = zensight_common::schema_location(subject.payload_type()) {
        println!("  defined at {loc}");
    }
    if let Some(unit) = subject.unit() {
        println!("unit      {unit}");
    }
    println!("qos       {:?}", subject.qos());
    if let Some(ttl) = subject.ttl_s() {
        // RFC 04 §1.2: publishers refresh at <= ttl/2, consumers age out at ttl.
        println!(
            "ttl       {ttl}s  (refresh <= {}s; stale after {ttl}s)",
            ttl / 2
        );
    }
    if let Some(rate) = subject.rate() {
        println!("rate      {rate}");
    }
    if let Some(c) = subject.cardinality() {
        println!("cardinality  ~{c} keys expected");
    }
    Ok(())
}

/// `topic info`, sourced from a live fleet's introspect slices instead of the
/// compiled-in registry (used under `--base <other-app>`).
///
/// The compiled path ([`topic_info`]) refines the key through *this* build's
/// registry grammar, which only knows this app's subjects. Against a foreign app
/// we cannot, so we parse the key **structurally** (grammar only, no registry)
/// and match its subject tail against the producer's served slice. A served
/// [`SubjectDecl`](zensight_keyspace::slice::SubjectDecl) carries less than a
/// compiled subject — no unit/qos/ttl/rate — which is the honest limit of what a
/// slice off the wire says.
pub fn topic_info_bus(base: &str, key: &str, slices: &[RegistrySlice]) -> Result<()> {
    use zensight_keyspace::grammar::ClassOrPlane;

    let Some(parsed) = zensight_common::keyexpr::parse_full_key(base, key) else {
        return Err(anyhow!(
            "not a v1 key. Expected <base>/v1/<origin>/<class>/<producer>/<subject...> (RFC 03 §1)."
        ));
    };
    let class = match &parsed.class {
        ClassOrPlane::Class(c) => *c,
        ClassOrPlane::Plane(p) => {
            return Err(anyhow!(
                "key is on the {} plane, not a data class — topic info describes \
                 telemetry/state/events subjects (RFC 03 §1.5).",
                p.chunk()
            ));
        }
    };
    let Some(producer) = parsed.producer.as_ref().map(|p| p.name().to_string()) else {
        return Err(anyhow!(
            "no producer chunk — topic info needs <origin>/<class>/<producer>/<subject...>."
        ));
    };
    let tail: Vec<&str> = parsed.subject.iter().map(String::as_str).collect();

    let Some(slice) = slices.iter().find(|s| s.name == producer) else {
        return Err(anyhow!(
            "no live producer {producer:?} served an introspect slice — \
             `zenctl node list --base {base}` says who is up."
        ));
    };

    let hit = slice.subjects.iter().find_map(|s| {
        if s.class != class.chunk() {
            return None;
        }
        match_subject(&s.path, &tail).map(|vars| (s, vars))
    });
    let Some((subject, vars)) = hit else {
        return Err(anyhow!(
            "producer {producer:?} serves no {} subject matching {:?} — a subject that is not \
             registered does not exist (RFC 08). `zenctl topic list --base {base}` lists what it \
             does serve.",
            class.chunk(),
            tail.join("/")
        ));
    };

    println!("key       {key}");
    println!("origin    {}", parsed.origin.chunk());
    println!("producer  {producer}");
    println!("class     {}", subject.class);
    println!("subject   {}", subject.path);
    if !vars.is_empty() {
        println!("variables");
        for (name, value) in &vars {
            println!("  {name} = {value}");
        }
    }
    println!("payload   {}", subject.type_name);
    match zensight_common::schema_location(&subject.type_name) {
        Some(loc) => println!("  defined at {loc}"),
        // Honest about the generic-explorer limit: a foreign app's type has no
        // entry in this build's RFC 08 §5 table, so we cannot point at a schema.
        None => println!("  (foreign type — not in this build's type table)"),
    }
    if let Some(since) = &subject.since {
        println!("since     {since}");
    }
    if let Some(desc) = &subject.description {
        println!("note      {desc}");
    }
    Ok(())
}

/// Match a concrete subject tail against a registry pattern, binding variables.
///
/// `{var}` matches exactly one chunk; a trailing `{var...}` matches the whole
/// remainder (RFC 03 §1.4's rest-variable). Returns the bindings on a match, or
/// `None` if the shapes disagree — the slice-level equivalent of the compiled
/// registry's parse direction, done without a compiled subject.
fn match_subject(pattern: &str, tail: &[&str]) -> Option<Vec<(String, String)>> {
    let pchunks: Vec<&str> = pattern.split('/').collect();
    let mut vars = Vec::new();
    let mut ti = 0usize;
    for (pi, pc) in pchunks.iter().enumerate() {
        if let Some(var) = pc.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            if let Some(name) = var.strip_suffix("...") {
                // A rest-variable is legal only as the final chunk; it swallows
                // whatever is left (possibly nothing).
                if pi != pchunks.len() - 1 {
                    return None;
                }
                vars.push((name.to_string(), tail[ti..].join("/")));
                return Some(vars);
            }
            let chunk = tail.get(ti)?;
            vars.push((var.to_string(), (*chunk).to_string()));
            ti += 1;
        } else {
            if tail.get(ti) != Some(pc) {
                return None;
            }
            ti += 1;
        }
    }
    (ti == tail.len()).then_some(vars)
}

/// `service list` — every registered procedure on the `@rpc` plane, from the
/// given slices (compiled-in or bus-sourced — same rendering).
pub fn service_list(slices: &[RegistrySlice], producer: Option<&str>) -> Result<()> {
    let mut total = 0usize;
    for slice in slices {
        let name = &slice.name;
        if producer.is_some_and(|p| p != name) {
            continue;
        }
        if slice.procedures.is_empty() {
            continue;
        }

        println!("\n{name}  (registry {})", slice.version);
        for p in &slice.procedures {
            let reply = p.reply.as_deref().unwrap_or("-");
            println!("  {:<6} {:<24} → {}", p.kind, p.path, reply);
            total += 1;
        }
    }
    if total == 0 {
        println!("no procedures match.");
        return Ok(());
    }
    println!("\n{total} registered procedure(s).");
    println!("call one with: zenctl service call <origin|*> <producer> <procedure>");
    Ok(())
}

/// `interface list` — the RFC 08 §5 type table.
pub fn interface_list() -> Result<()> {
    println!("payload types (RFC 08 §5 type table):\n");
    for (name, location) in zensight_common::PAYLOAD_TYPES {
        println!("  {name:<18} {location}");
    }
    println!("\n{} type(s).", zensight_common::PAYLOAD_TYPES.len());
    Ok(())
}

/// `interface show` — one payload type, and every subject that carries it.
///
/// Field-level schema is deliberately absent: RFC 08 §5's type table maps a
/// type name to its *schema location*, and the definitions "stay with the
/// owning crates" (RFC 01 §5). So this points you at the definition rather than
/// pretending to reproduce it.
pub fn interface_show(type_name: &str) -> Result<()> {
    let Some(location) = zensight_common::schema_location(type_name) else {
        let known: Vec<&str> = zensight_common::PAYLOAD_TYPES
            .iter()
            .map(|(n, _)| *n)
            .collect();
        return Err(anyhow!(
            "unknown payload type {type_name:?}.\nknown types: {}",
            known.join(", ")
        ));
    };

    println!("type      {type_name}");
    println!("defined   {location}");

    // Which subjects actually carry it — the reverse of the registry's binding.
    let mut carriers: Vec<(String, String, String)> = Vec::new();
    for (name, toml_src) in REGISTRIES {
        let slice = zensight_keyspace::parse_slice(toml_src)
            .map_err(|e| anyhow!("registry slice for {name} does not parse: {e}"))?;
        for s in &slice.subjects {
            if s.type_name == type_name {
                carriers.push((name.to_string(), s.class.clone(), s.path.clone()));
            }
        }
    }

    if carriers.is_empty() {
        println!("\ncarried by no registered subject.");
        return Ok(());
    }
    // A type on hundreds of subjects (TelemetryPoint) is noise if fully listed;
    // the count is the useful fact, and a sample shows the shape.
    println!("\ncarried by {} subject(s):", carriers.len());
    for (producer, class, path) in carriers.iter().take(20) {
        println!("  {producer:<10} {class:<10} {path}");
    }
    if carriers.len() > 20 {
        println!("  … and {} more", carriers.len() - 20);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offline half must not need a bus. If any of these ever tries to open
    /// a session, it hangs here rather than in someone's terminal.
    #[test]
    fn offline_commands_need_no_bus() {
        let slices = compiled_slices().unwrap();
        topic_list(&slices, Some("sysinfo"), Some("telemetry")).unwrap();
        service_list(&slices, Some("netlink")).unwrap();
        interface_list().unwrap();
        interface_show("TelemetryPoint").unwrap();
    }

    #[test]
    fn topic_info_refines_a_concrete_key() {
        // A registered sysinfo state subject.
        topic_info(
            zensight_keyspace::DEFAULT_BASE,
            "zensight/v1/h-3fa9c2d41b7e/state/sysinfo/health",
        )
        .unwrap();
    }

    #[test]
    fn topic_info_rejects_a_non_v1_key() {
        let err = topic_info(
            zensight_keyspace::DEFAULT_BASE,
            "zensight/snmp/router-1/if/eth0/rx",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a v1 key"), "got: {err}");
    }

    /// "A subject that is not registered does not exist" — and the error must
    /// say which of the two things went wrong, because the fixes differ.
    #[test]
    fn topic_info_distinguishes_unregistered_from_malformed() {
        let err = topic_info(
            zensight_keyspace::DEFAULT_BASE,
            "zensight/v1/h-3fa9c2d41b7e/state/sysinfo/not_a_real_subject",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not registered"), "got: {err}");
    }

    #[test]
    fn unknown_class_is_rejected() {
        let slices = compiled_slices().unwrap();
        let err = topic_list(&slices, None, Some("alerts")).unwrap_err();
        assert!(err.to_string().contains("unknown class"), "got: {err}");
    }

    /// A tcgui-style registry slice — a *foreign* app, read as if off the wire —
    /// must parse and render without any of tcgui compiled in. This is the whole
    /// point of the app-agnostic path (tcgui#45): the extra `fanout` field tcgui
    /// carries is unknown to this build, and `parse_slice` must tolerate it (RFC
    /// 08 §6 forward-compat), then `topic list` / `service list` / `topic info`
    /// render sane rows from the parsed slice.
    const TCGUI_SLICE: &str = r#"
        [registry]
        version = "0.3"
        app = "tcgui"
        convention = 1

        [producer]
        name = "tc"
        description = "traffic-control netem shaper"

        [[subject]]
        path = "iface/{iface}/state"
        class = "state"
        type = "NetworkInterface"
        fanout = "per-iface"
        since = "0.1"
        description = "current netem config on an interface"

        [[subject]]
        path = "health"
        class = "state"
        type = "BackendHealthStatus"
        since = "0.1"

        [[procedure]]
        path = "iface/{iface}/set"
        kind = "write"
        reply = "Ack"
        fanout = "one"
        since = "0.2"
        description = "apply a netem config"
    "#;

    #[test]
    fn foreign_tcgui_slice_parses_and_renders() {
        // parse_slice tolerates the unknown `fanout` field (forward-compat).
        let slice = parse_slice(TCGUI_SLICE).unwrap();
        assert_eq!(slice.name, "tc");
        assert_eq!(slice.app, "tcgui");
        assert_eq!(slice.subjects.len(), 2);
        assert_eq!(slice.procedures.len(), 1);
        assert_eq!(slice.subjects[0].type_name, "NetworkInterface");
        assert_eq!(slice.procedures[0].kind, "write");

        let slices = vec![slice];

        // The shared renderers accept a bus-sourced slice with no tcgui compiled
        // in — same code path as `--base tcgui` would drive.
        topic_list(&slices, None, None).unwrap();
        topic_list(&slices, Some("tc"), Some("state")).unwrap();
        service_list(&slices, Some("tc")).unwrap();

        // A concrete foreign key refines against the served slice, binding the
        // `{iface}` variable, even though `NetworkInterface` is not in this
        // build's type table.
        topic_info_bus(
            "tcgui",
            "tcgui/v1/h-3fa9c2d41b7e/state/tc/iface/eth0/state",
            &slices,
        )
        .unwrap();
    }

    /// The subject-tail matcher binds `{var}` and trailing `{var...}`, and
    /// rejects shape mismatches — the slice-level parse direction.
    #[test]
    fn subject_matcher_binds_and_rejects() {
        let vars = match_subject("iface/{iface}/state", &["iface", "eth0", "state"]).unwrap();
        assert_eq!(vars, vec![("iface".to_string(), "eth0".to_string())]);

        let rest = match_subject("dev/{path...}", &["dev", "a", "b", "c"]).unwrap();
        assert_eq!(rest, vec![("path".to_string(), "a/b/c".to_string())]);

        // Literal chunk mismatch and length mismatch both fail.
        assert!(match_subject("health", &["health", "extra"]).is_none());
        assert!(match_subject("iface/{iface}/state", &["iface", "eth0"]).is_none());
    }

    #[test]
    fn unknown_type_lists_the_known_ones() {
        let err = interface_show("StreamDoc").unwrap_err();
        assert!(err.to_string().contains("TelemetryPoint"), "got: {err}");
    }
}
