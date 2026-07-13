//! The offline half: everything answerable from the compiled-in registry.
//!
//! These commands need no bus, no session, and no live fleet. They describe
//! what the registry *declares* — which is a different question from what is
//! actually being published, and the output says so where the difference bites
//! (see the note on rest-variables in [`topic_list`]).

use anyhow::{Result, anyhow};
use zensight_keyspace::registry::REGISTRIES;

/// `topic list` — every registered subject, from the registry this build
/// compiled against.
///
/// **Declared, not observed.** A pattern with a trailing rest-variable
/// (`{path...}`) stands for a whole family whose real members only exist on the
/// wire: the four proxy producers (snmp/modbus/gnmi/netflow) register
/// `{device}/{path...}` by design, because their metric tree belongs to the
/// polled device, not to us. For those, this command can only tell you the
/// shape. `zenctl topic echo` is what tells you the members.
pub fn topic_list(producer: Option<&str>, class: Option<&str>) -> Result<()> {
    if let Some(c) = class
        && !["telemetry", "state", "events"].contains(&c)
    {
        return Err(anyhow!(
            "unknown class {c:?} — the classes are telemetry, state, events (RFC 04 §1)"
        ));
    }

    let mut total = 0usize;
    let mut open_ended = 0usize;

    for (name, toml_src) in REGISTRIES {
        if producer.is_some_and(|p| p != *name) {
            continue;
        }
        let slice = zensight_keyspace::parse_slice(toml_src)
            .map_err(|e| anyhow!("registry slice for {name} does not parse: {e}"))?;

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
pub fn topic_info(key: &str) -> Result<()> {
    let Some((structural, producer, subject)) = zensight_common::keyexpr::refine_wire_key(key)
    else {
        // Distinguish the two failure modes: a key that is not v1-shaped at all,
        // versus one that parses structurally but names an unregistered subject.
        // RFC 08: "a subject that is not registered does not exist."
        return match zensight_common::keyexpr::parse_wire_key(key) {
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

/// `service list` — every registered procedure on the `@rpc` plane.
pub fn service_list(producer: Option<&str>) -> Result<()> {
    let mut total = 0usize;
    for (name, toml_src) in REGISTRIES {
        if producer.is_some_and(|p| p != *name) {
            continue;
        }
        let slice = zensight_keyspace::parse_slice(toml_src)
            .map_err(|e| anyhow!("registry slice for {name} does not parse: {e}"))?;
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
        topic_list(Some("sysinfo"), Some("telemetry")).unwrap();
        service_list(Some("netlink")).unwrap();
        interface_list().unwrap();
        interface_show("TelemetryPoint").unwrap();
    }

    #[test]
    fn topic_info_refines_a_concrete_key() {
        // A registered sysinfo state subject.
        topic_info("zensight/v1/h-3fa9c2d41b7e/state/sysinfo/health").unwrap();
    }

    #[test]
    fn topic_info_rejects_a_non_v1_key() {
        let err = topic_info("zensight/snmp/router-1/if/eth0/rx").unwrap_err();
        assert!(err.to_string().contains("not a v1 key"), "got: {err}");
    }

    /// "A subject that is not registered does not exist" — and the error must
    /// say which of the two things went wrong, because the fixes differ.
    #[test]
    fn topic_info_distinguishes_unregistered_from_malformed() {
        let err =
            topic_info("zensight/v1/h-3fa9c2d41b7e/state/sysinfo/not_a_real_subject").unwrap_err();
        assert!(err.to_string().contains("not registered"), "got: {err}");
    }

    #[test]
    fn unknown_class_is_rejected() {
        let err = topic_list(None, Some("alerts")).unwrap_err();
        assert!(err.to_string().contains("unknown class"), "got: {err}");
    }

    #[test]
    fn unknown_type_lists_the_known_ones() {
        let err = interface_show("StreamDoc").unwrap_err();
        assert!(err.to_string().contains("TelemetryPoint"), "got: {err}");
    }
}
