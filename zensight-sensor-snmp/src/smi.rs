//! Real SMI MIB support (#532), via mib-rs (async-snmp's `mib` backend).
//!
//! Stock vendor `.mib`/`.txt` files drop into `snmp.mib.dirs` unmodified and
//! their OIDs resolve in published metrics without code changes. The SMI
//! layer is the **fallback** behind the existing resolver chain (built-in
//! tables, config `oid_names`, profiles): explicit mappings keep their
//! names; everything else gets `snake_case(object)[/suffix]` instead of a
//! dotted OID — chunk-grammar-valid by construction (#559).
//!
//! Beyond naming, the MIB metadata feeds:
//! - **enum decode**: INTEGER named-values (`up(1)`) ride an `enum` label on
//!   the point (the numeric value stays numeric for thresholds/plots);
//! - **units**: the UNITS clause fills `TelemetryPoint.unit` when the value
//!   conversion didn't already;
//! - **typing**: the SMI base type backs rate eligibility (Counter32/64)
//!   exactly like the hand-maintained SYNTAX hints;
//! - **trap translation**: notification OIDs resolve to names for the trap
//!   receiver (linkDown, coldStart, vendor notifications).

use anyhow::{Context, Result};
use mib_rs::{BaseType, Loader, Mib, Object, source};

/// A loaded SMI MIB set.
pub struct SmiResolver {
    mib: Mib,
}

impl std::fmt::Debug for SmiResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmiResolver").finish_non_exhaustive()
    }
}

impl SmiResolver {
    /// Load every module found in `dirs`. Parse/link failures are hard
    /// errors — a silently-thinner MIB set means silently-worse names.
    pub fn load_dirs(dirs: &[String]) -> Result<Self> {
        let mut loader = Loader::new();
        for dir in dirs {
            loader = loader
                .source(source::dir(dir).with_context(|| format!("MIB dir {dir} unreadable"))?);
        }
        let mib = loader
            .load()
            .map_err(|e| anyhow::anyhow!("MIB load failed: {e}"))?;
        Ok(Self { mib })
    }

    /// Load from in-memory module text (tests).
    pub fn load_memory(name: &str, text: &str) -> Result<Self> {
        let mib = Loader::new()
            .source(source::memory(name, text.as_bytes().to_vec()))
            .load()
            .map_err(|e| anyhow::anyhow!("MIB load failed: {e}"))?;
        Ok(Self { mib })
    }

    fn lookup(&self, oid_str: &str) -> Option<(Object<'_>, Vec<u32>)> {
        let oid: mib_rs::Oid = oid_str.parse().ok()?;
        let lookup = self.mib.lookup_instance(&oid);
        let suffix = lookup.suffix().to_vec();
        let object = lookup.node().object()?;
        Some((object, suffix))
    }

    /// Metric name for a polled OID: `snake_case(object)` for scalars
    /// (`.0`), `snake_case(object)/<suffix>` for table instances. `None`
    /// when the OID matches no OBJECT-TYPE instance.
    pub fn metric_name(&self, oid_str: &str) -> Option<String> {
        let (object, suffix) = self.lookup(oid_str)?;
        if suffix.is_empty() {
            return None; // the object node itself, not an instance
        }
        let name = snake_case(object.name());
        if suffix == [0] {
            Some(name)
        } else {
            let suffix = suffix
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(".");
            Some(format!("{name}/{suffix}"))
        }
    }

    /// SMI base type mapped onto the resolver's SYNTAX vocabulary, for rate
    /// eligibility and typing.
    pub fn syntax(&self, oid_str: &str) -> Option<&'static str> {
        let (object, _) = self.lookup(oid_str)?;
        Some(match object.ty()?.base() {
            BaseType::Counter32 => "Counter32",
            BaseType::Counter64 => "Counter64",
            BaseType::Gauge32 => "Gauge32",
            BaseType::TimeTicks => "TimeTicks",
            _ => return None,
        })
    }

    /// The enum label for an INTEGER value (`up` for ifOperStatus 1).
    pub fn enum_label(&self, oid_str: &str, value: i64) -> Option<String> {
        let (object, _) = self.lookup(oid_str)?;
        object
            .effective_enums()
            .iter()
            .find(|nv| nv.value == value)
            .map(|nv| nv.label.clone())
    }

    /// The UNITS clause, when present and non-empty.
    pub fn unit(&self, oid_str: &str) -> Option<String> {
        let (object, _) = self.lookup(oid_str)?;
        let units = object.units();
        (!units.is_empty()).then(|| units.to_string())
    }

    /// Resolve a notification/trap OID to its snake-case name (trap ids are
    /// key chunks too).
    pub fn notification_name(&self, oid_str: &str) -> Option<String> {
        let oid: mib_rs::Oid = oid_str.parse().ok()?;
        let lookup = self.mib.lookup_instance(&oid);
        if !lookup.suffix().is_empty() {
            return None;
        }
        lookup.node().notification().map(|n| snake_case(n.name()))
    }
}

/// `ifInOctets` → `if_in_octets`; already-lowercase names pass through.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower = false;
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c.is_ascii_alphanumeric() {
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            out.push(c);
        } else {
            // Hyphens etc. become underscores (chunk-grammar-safe).
            prev_lower = false;
            out.push('_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny but legal SMIv2 module: a scalar with UNITS, a table with an
    /// enum column and a Counter64 column, and a notification.
    const TEST_MIB: &str = r#"
ZENTEST-MIB DEFINITIONS ::= BEGIN

IMPORTS
    MODULE-IDENTITY, OBJECT-TYPE, NOTIFICATION-TYPE, Counter64, Integer32,
    enterprises
        FROM SNMPv2-SMI;

zentest MODULE-IDENTITY
    LAST-UPDATED "202601010000Z"
    ORGANIZATION "zensight"
    CONTACT-INFO "test"
    DESCRIPTION  "test module"
    ::= { enterprises 4242 }

zenTemp OBJECT-TYPE
    SYNTAX      Integer32
    UNITS       "Cel"
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "chassis temperature"
    ::= { zentest 1 }

zenPortTable OBJECT-TYPE
    SYNTAX      SEQUENCE OF ZenPortEntry
    MAX-ACCESS  not-accessible
    STATUS      current
    DESCRIPTION "ports"
    ::= { zentest 2 }

zenPortEntry OBJECT-TYPE
    SYNTAX      ZenPortEntry
    MAX-ACCESS  not-accessible
    STATUS      current
    DESCRIPTION "port row"
    INDEX       { zenPortIndex }
    ::= { zenPortTable 1 }

ZenPortEntry ::= SEQUENCE {
    zenPortIndex   Integer32,
    zenPortState   INTEGER,
    zenPortOctets  Counter64
}

zenPortIndex OBJECT-TYPE
    SYNTAX      Integer32
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "index"
    ::= { zenPortEntry 1 }

zenPortState OBJECT-TYPE
    SYNTAX      INTEGER { up(1), down(2), degraded(3) }
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "state"
    ::= { zenPortEntry 2 }

zenPortOctets OBJECT-TYPE
    SYNTAX      Counter64
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "octets"
    ::= { zenPortEntry 3 }

zenLinkFlap NOTIFICATION-TYPE
    OBJECTS     { zenPortState }
    STATUS      current
    DESCRIPTION "flap"
    ::= { zentest 3 }

END
"#;

    fn resolver() -> SmiResolver {
        SmiResolver::load_memory("ZENTEST-MIB", TEST_MIB).expect("test MIB loads")
    }

    #[test]
    fn scalar_and_column_names() {
        let r = resolver();
        assert_eq!(
            r.metric_name("1.3.6.1.4.1.4242.1.0").as_deref(),
            Some("zen_temp")
        );
        assert_eq!(
            r.metric_name("1.3.6.1.4.1.4242.2.1.3.7").as_deref(),
            Some("zen_port_octets/7")
        );
        // Unknown arcs stay unresolved.
        assert_eq!(r.metric_name("1.3.6.1.4.1.9999.1.0"), None);
    }

    #[test]
    fn names_are_chunk_grammar_valid() {
        let r = resolver();
        for oid in ["1.3.6.1.4.1.4242.1.0", "1.3.6.1.4.1.4242.2.1.3.7"] {
            for chunk in r.metric_name(oid).unwrap().split('/') {
                assert!(
                    zenkey::grammar::is_valid_plain_chunk(chunk),
                    "chunk {chunk:?} invalid"
                );
            }
        }
    }

    #[test]
    fn syntax_units_and_enums() {
        let r = resolver();
        assert_eq!(r.syntax("1.3.6.1.4.1.4242.2.1.3.7"), Some("Counter64"));
        assert_eq!(r.syntax("1.3.6.1.4.1.4242.1.0"), None); // Integer32
        assert_eq!(r.unit("1.3.6.1.4.1.4242.1.0").as_deref(), Some("Cel"));
        assert_eq!(
            r.enum_label("1.3.6.1.4.1.4242.2.1.2.7", 3).as_deref(),
            Some("degraded")
        );
        assert_eq!(r.enum_label("1.3.6.1.4.1.4242.2.1.2.7", 9), None);
    }

    #[test]
    fn notification_names_resolve() {
        let r = resolver();
        assert_eq!(
            r.notification_name("1.3.6.1.4.1.4242.3").as_deref(),
            Some("zen_link_flap")
        );
        assert_eq!(r.notification_name("1.3.6.1.4.1.4242.1.0"), None);
    }

    #[test]
    fn snake_case_shapes() {
        assert_eq!(snake_case("ifInOctets"), "if_in_octets");
        assert_eq!(snake_case("sysDescr"), "sys_descr");
        assert_eq!(snake_case("ifHCInOctets"), "if_hcin_octets");
        assert_eq!(snake_case("already_snake"), "already_snake");
        assert_eq!(snake_case("with-hyphen"), "with_hyphen");
    }
}
