//! MIB (Management Information Base) loading and OID resolution.
//!
//! This module provides functionality to load pre-compiled OID-to-name mappings
//! from JSON files. These mappings are derived from standard MIB definitions
//! (IF-MIB, SNMPv2-MIB, HOST-RESOURCES-MIB, etc.) and allow the sensor to
//! publish human-readable metric names instead of numeric OIDs.
//!
//! # Example
//!
//! ```ignore
//! let mut resolver = MibResolver::new();
//! resolver.load_builtin_mibs()?;
//! resolver.load_file("custom-mibs.json")?;
//!
//! // Resolve OID to name
//! let name = resolver.resolve("1.3.6.1.2.1.1.3.0");
//! assert_eq!(name, "system/uptime");
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// A MIB resolver that converts numeric OIDs to human-readable names.
#[derive(Debug, Clone, Default)]
pub struct MibResolver {
    /// Exact OID to name mappings.
    exact_mappings: HashMap<String, OidEntry>,
    /// Prefix mappings for table entries (longest prefix match).
    prefix_mappings: Vec<(String, OidEntry)>,
    /// Loaded MIB modules.
    loaded_modules: Vec<String>,
}

/// An entry in the OID mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidEntry {
    /// Human-readable name (e.g., "sysUpTime", "if/{index}/in_octets").
    pub name: String,
    /// MIB module this OID belongs to (e.g., "SNMPv2-MIB", "IF-MIB").
    #[serde(default)]
    pub module: Option<String>,
    /// Description of the OID.
    #[serde(default)]
    pub description: Option<String>,
    /// SYNTAX type (e.g., "Counter32", "INTEGER", "DisplayString").
    #[serde(default)]
    pub syntax: Option<String>,
    /// Whether this is a table entry (has index suffix).
    #[serde(default)]
    pub is_table_entry: bool,
}

/// A MIB definition file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MibDefinition {
    /// Module name (e.g., "IF-MIB").
    pub module: String,
    /// Module description.
    #[serde(default)]
    pub description: Option<String>,
    /// OID mappings: OID string -> entry.
    pub oids: HashMap<String, OidEntry>,
}

impl MibResolver {
    /// Create a new empty MIB resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load built-in MIB definitions (common SNMP MIBs).
    pub fn load_builtin_mibs(&mut self) -> Result<()> {
        // Load SNMPv2-MIB
        self.load_snmpv2_mib();
        // Load IF-MIB
        self.load_if_mib();
        // Load HOST-RESOURCES-MIB
        self.load_host_resources_mib();
        // Load IP-MIB
        self.load_ip_mib();

        Ok(())
    }

    /// Load MIB definitions from a JSON file.
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read MIB file: {}", path.display()))?;

        self.load_json(&content)
            .with_context(|| format!("Failed to parse MIB file: {}", path.display()))?;

        Ok(())
    }

    /// Load MIB definitions from a JSON string.
    pub fn load_json(&mut self, json: &str) -> Result<()> {
        let def: MibDefinition = serde_json::from_str(json)
            .or_else(|_| json5::from_str(json))
            .context("Failed to parse MIB JSON")?;

        self.load_definition(def);
        Ok(())
    }

    /// Load a MIB definition into the resolver.
    pub fn load_definition(&mut self, def: MibDefinition) {
        self.loaded_modules.push(def.module.clone());

        for (oid, mut entry) in def.oids {
            // Set module if not already set
            if entry.module.is_none() {
                entry.module = Some(def.module.clone());
            }

            if entry.is_table_entry {
                self.prefix_mappings.push((oid, entry));
            } else {
                self.exact_mappings.insert(oid, entry);
            }
        }

        // Sort prefix mappings by length (longest first) for best match
        self.prefix_mappings
            .sort_by_key(|b| std::cmp::Reverse(b.0.len()));
    }

    /// Add custom OID mappings from configuration.
    pub fn add_custom_mappings(&mut self, mappings: &HashMap<String, String>) {
        for (oid, name) in mappings {
            let is_table_entry = name.contains("{index}");
            let entry = OidEntry {
                name: name.clone(),
                module: Some("custom".to_string()),
                description: None,
                syntax: None,
                is_table_entry,
            };

            if is_table_entry {
                self.prefix_mappings.push((oid.clone(), entry));
            } else {
                self.exact_mappings.insert(oid.clone(), entry);
            }
        }

        // Re-sort prefix mappings
        self.prefix_mappings
            .sort_by_key(|b| std::cmp::Reverse(b.0.len()));
    }

    /// Add profile mappings (#531): names with `{index}` placeholders plus
    /// SMI SYNTAX hints, e.g. from shipped/user profile TOMLs. Names win
    /// over earlier entries with the same OID only in the exact table;
    /// prefix matches keep longest-prefix-first order.
    pub fn add_profile_mappings(
        &mut self,
        names: &HashMap<String, String>,
        syntax: &HashMap<String, String>,
    ) {
        for (oid, name) in names {
            let is_table_entry = name.contains("{index}");
            let entry = OidEntry {
                name: name.clone(),
                module: Some("profile".to_string()),
                description: None,
                syntax: syntax.get(oid).cloned(),
                is_table_entry,
            };
            if is_table_entry {
                self.prefix_mappings.push((oid.clone(), entry));
            } else {
                // Earlier mappings (builtins, config `oid_names`) win.
                self.exact_mappings.entry(oid.clone()).or_insert(entry);
            }
        }
        // Syntax hints for OIDs without a name mapping still matter (rate
        // eligibility). A prefix entry covers both shapes: a table column
        // resolves to `<oid>.<index>` (the dotted form) and a scalar falls
        // through to the dotted OID — while `syntax()` finds the hint.
        for (oid, syn) in syntax {
            if names.contains_key(oid) {
                continue;
            }
            let entry = OidEntry {
                name: oid.clone(),
                module: Some("profile".to_string()),
                description: None,
                syntax: Some(syn.clone()),
                is_table_entry: true,
            };
            self.prefix_mappings.push((oid.clone(), entry));
        }
        self.prefix_mappings
            .sort_by_key(|b| std::cmp::Reverse(b.0.len()));
    }

    /// OID prefix match on chunk boundaries (#581): `1.3.6.1.2.2.1.1` matches
    /// `1.3.6.1.2.2.1.1.3` but not `1.3.6.1.2.2.1.10.3` — a plain
    /// `starts_with` string-matches the latter, silently resolving an
    /// unregistered sibling column to the wrong name/syntax whenever the
    /// longer column isn't also registered.
    fn matches_prefix(oid: &str, prefix: &str) -> bool {
        oid == prefix
            || (oid.starts_with(prefix) && oid.as_bytes().get(prefix.len()) == Some(&b'.'))
    }

    /// Resolve an OID to a human-readable name.
    ///
    /// Returns the mapped name if found, otherwise returns the original OID.
    pub fn resolve(&self, oid: &str) -> String {
        // Check exact match first
        if let Some(entry) = self.exact_mappings.get(oid) {
            return entry.name.clone();
        }

        // Check prefix matches for table entries
        for (prefix, entry) in &self.prefix_mappings {
            if Self::matches_prefix(oid, prefix) {
                let suffix = &oid[prefix.len()..];
                let index = suffix.trim_start_matches('.');

                if !index.is_empty() {
                    if entry.name.contains("{index}") {
                        return entry.name.replace("{index}", index);
                    } else {
                        return format!("{}.{}", entry.name, index);
                    }
                }
            }
        }

        // No mapping found
        oid.to_string()
    }

    /// SMI SYNTAX for an OID (exact or table-prefix match), when the MIB
    /// table knows it — e.g. `"Counter32"`, `"Counter64"`, `"TimeTicks"`.
    pub fn syntax(&self, oid: &str) -> Option<&str> {
        if let Some(entry) = self.exact_mappings.get(oid) {
            return entry.syntax.as_deref();
        }
        for (prefix, entry) in &self.prefix_mappings {
            if Self::matches_prefix(oid, prefix) {
                return entry.syntax.as_deref();
            }
        }
        None
    }

    /// Get list of loaded MIB modules.
    pub fn loaded_modules(&self) -> &[String] {
        &self.loaded_modules
    }

    /// Get total number of OID mappings.
    pub fn mapping_count(&self) -> usize {
        self.exact_mappings.len() + self.prefix_mappings.len()
    }

    // --- Built-in MIB definitions ---

    fn load_snmpv2_mib(&mut self) {
        let def = MibDefinition {
            module: "SNMPv2-MIB".to_string(),
            description: Some("SNMPv2 Management Information Base".to_string()),
            oids: HashMap::from([
                // system group (1.3.6.1.2.1.1)
                (
                    "1.3.6.1.2.1.1.1.0".to_string(),
                    OidEntry {
                        name: "system/descr".to_string(),
                        module: None,
                        description: Some("System description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.2.0".to_string(),
                    OidEntry {
                        name: "system/object_id".to_string(),
                        module: None,
                        description: Some("System object identifier".to_string()),
                        syntax: Some("OBJECT IDENTIFIER".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.3.0".to_string(),
                    OidEntry {
                        name: "system/uptime".to_string(),
                        module: None,
                        description: Some("Time since system started".to_string()),
                        syntax: Some("TimeTicks".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.4.0".to_string(),
                    OidEntry {
                        name: "system/contact".to_string(),
                        module: None,
                        description: Some("Contact person for system".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.5.0".to_string(),
                    OidEntry {
                        name: "system/name".to_string(),
                        module: None,
                        description: Some("System name".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.6.0".to_string(),
                    OidEntry {
                        name: "system/location".to_string(),
                        module: None,
                        description: Some("Physical location of system".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.7.0".to_string(),
                    OidEntry {
                        name: "system/services".to_string(),
                        module: None,
                        description: Some("Services offered by system".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: false,
                    },
                ),
                // snmp group (1.3.6.1.2.1.11)
                (
                    "1.3.6.1.2.1.11.1.0".to_string(),
                    OidEntry {
                        name: "snmp/in_pkts".to_string(),
                        module: None,
                        description: Some("Total SNMP messages received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.11.2.0".to_string(),
                    OidEntry {
                        name: "snmp/out_pkts".to_string(),
                        module: None,
                        description: Some("Total SNMP messages sent".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: false,
                    },
                ),
            ]),
        };
        self.load_definition(def);
    }

    fn load_if_mib(&mut self) {
        let def = MibDefinition {
            module: "IF-MIB".to_string(),
            description: Some("Interface MIB".to_string()),
            oids: HashMap::from([
                // interfaces group (1.3.6.1.2.1.2)
                (
                    "1.3.6.1.2.1.2.1.0".to_string(),
                    OidEntry {
                        name: "if_number".to_string(),
                        module: None,
                        description: Some("Number of network interfaces".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: false,
                    },
                ),
                // ifTable entries (1.3.6.1.2.1.2.2.1.x)
                (
                    "1.3.6.1.2.1.2.2.1.1".to_string(),
                    OidEntry {
                        name: "if/{index}/index".to_string(),
                        module: None,
                        description: Some("Interface index".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.2".to_string(),
                    OidEntry {
                        name: "if/{index}/descr".to_string(),
                        module: None,
                        description: Some("Interface description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.3".to_string(),
                    OidEntry {
                        name: "if/{index}/type".to_string(),
                        module: None,
                        description: Some("Interface type".to_string()),
                        syntax: Some("IANAifType".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.4".to_string(),
                    OidEntry {
                        name: "if/{index}/mtu".to_string(),
                        module: None,
                        description: Some("Interface MTU".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.5".to_string(),
                    OidEntry {
                        name: "if/{index}/speed".to_string(),
                        module: None,
                        description: Some("Interface speed (bps)".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.6".to_string(),
                    OidEntry {
                        name: "if/{index}/phys_address".to_string(),
                        module: None,
                        description: Some("Interface MAC address".to_string()),
                        syntax: Some("PhysAddress".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.7".to_string(),
                    OidEntry {
                        name: "if/{index}/admin_status".to_string(),
                        module: None,
                        description: Some("Desired interface state".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.8".to_string(),
                    OidEntry {
                        name: "if/{index}/oper_status".to_string(),
                        module: None,
                        description: Some("Current interface state".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.9".to_string(),
                    OidEntry {
                        name: "if/{index}/last_change".to_string(),
                        module: None,
                        description: Some("Last status change time".to_string()),
                        syntax: Some("TimeTicks".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.10".to_string(),
                    OidEntry {
                        name: "if/{index}/in_octets".to_string(),
                        module: None,
                        description: Some("Bytes received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.11".to_string(),
                    OidEntry {
                        name: "if/{index}/in_ucast_pkts".to_string(),
                        module: None,
                        description: Some("Unicast packets received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.13".to_string(),
                    OidEntry {
                        name: "if/{index}/in_discards".to_string(),
                        module: None,
                        description: Some("Inbound discards".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.14".to_string(),
                    OidEntry {
                        name: "if/{index}/in_errors".to_string(),
                        module: None,
                        description: Some("Inbound errors".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.16".to_string(),
                    OidEntry {
                        name: "if/{index}/out_octets".to_string(),
                        module: None,
                        description: Some("Bytes sent".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.17".to_string(),
                    OidEntry {
                        name: "if/{index}/out_ucast_pkts".to_string(),
                        module: None,
                        description: Some("Unicast packets sent".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.19".to_string(),
                    OidEntry {
                        name: "if/{index}/out_discards".to_string(),
                        module: None,
                        description: Some("Outbound discards".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.20".to_string(),
                    OidEntry {
                        name: "if/{index}/out_errors".to_string(),
                        module: None,
                        description: Some("Outbound errors".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                // ifXTable entries (1.3.6.1.2.1.31.1.1.1.x) - 64-bit counters
                (
                    "1.3.6.1.2.1.31.1.1.1.1".to_string(),
                    OidEntry {
                        name: "ifx/{index}/name".to_string(),
                        module: None,
                        description: Some("Interface name".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.6".to_string(),
                    OidEntry {
                        name: "ifx/{index}/hc_in_octets".to_string(),
                        module: None,
                        description: Some("Bytes received (64-bit)".to_string()),
                        syntax: Some("Counter64".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.10".to_string(),
                    OidEntry {
                        name: "ifx/{index}/hc_out_octets".to_string(),
                        module: None,
                        description: Some("Bytes sent (64-bit)".to_string()),
                        syntax: Some("Counter64".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.15".to_string(),
                    OidEntry {
                        name: "ifx/{index}/high_speed".to_string(),
                        module: None,
                        description: Some("Interface speed (Mbps)".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.18".to_string(),
                    OidEntry {
                        name: "ifx/{index}/alias".to_string(),
                        module: None,
                        description: Some("Interface alias/description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
            ]),
        };
        self.load_definition(def);
    }

    fn load_host_resources_mib(&mut self) {
        let def = MibDefinition {
            module: "HOST-RESOURCES-MIB".to_string(),
            description: Some("Host Resources MIB".to_string()),
            oids: HashMap::from([
                // hrSystem group
                (
                    "1.3.6.1.2.1.25.1.1.0".to_string(),
                    OidEntry {
                        name: "host/uptime".to_string(),
                        module: None,
                        description: Some("Host uptime".to_string()),
                        syntax: Some("TimeTicks".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.1.2.0".to_string(),
                    OidEntry {
                        name: "host/date".to_string(),
                        module: None,
                        description: Some("Current date and time".to_string()),
                        syntax: Some("DateAndTime".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.1.5.0".to_string(),
                    OidEntry {
                        name: "host/users".to_string(),
                        module: None,
                        description: Some("Number of logged in users".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.1.6.0".to_string(),
                    OidEntry {
                        name: "host/processes".to_string(),
                        module: None,
                        description: Some("Number of processes".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: false,
                    },
                ),
                // hrStorage table
                (
                    "1.3.6.1.2.1.25.2.3.1.1".to_string(),
                    OidEntry {
                        name: "storage/{index}/index".to_string(),
                        module: None,
                        description: Some("Storage index".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.2".to_string(),
                    OidEntry {
                        name: "storage/{index}/type".to_string(),
                        module: None,
                        description: Some("Storage type".to_string()),
                        syntax: Some("OBJECT IDENTIFIER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.3".to_string(),
                    OidEntry {
                        name: "storage/{index}/descr".to_string(),
                        module: None,
                        description: Some("Storage description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.4".to_string(),
                    OidEntry {
                        name: "storage/{index}/allocation_units".to_string(),
                        module: None,
                        description: Some("Allocation unit size".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.5".to_string(),
                    OidEntry {
                        name: "storage/{index}/size".to_string(),
                        module: None,
                        description: Some("Total storage units".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.6".to_string(),
                    OidEntry {
                        name: "storage/{index}/used".to_string(),
                        module: None,
                        description: Some("Used storage units".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                // hrProcessor table
                (
                    "1.3.6.1.2.1.25.3.3.1.2".to_string(),
                    OidEntry {
                        name: "cpu/{index}/load".to_string(),
                        module: None,
                        description: Some("CPU load (1 min avg)".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
            ]),
        };
        self.load_definition(def);
    }

    fn load_ip_mib(&mut self) {
        let def = MibDefinition {
            module: "IP-MIB".to_string(),
            description: Some("IP MIB".to_string()),
            oids: HashMap::from([
                // ip group scalars
                (
                    "1.3.6.1.2.1.4.1.0".to_string(),
                    OidEntry {
                        name: "ip/forwarding".to_string(),
                        module: None,
                        description: Some("IP forwarding enabled".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.2.0".to_string(),
                    OidEntry {
                        name: "ip/default_ttl".to_string(),
                        module: None,
                        description: Some("Default TTL".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.3.0".to_string(),
                    OidEntry {
                        name: "ip/in_receives".to_string(),
                        module: None,
                        description: Some("IP datagrams received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: false,
                    },
                ),
                // ipAddrTable
                (
                    "1.3.6.1.2.1.4.20.1.1".to_string(),
                    OidEntry {
                        name: "ip/{index}/addr".to_string(),
                        module: None,
                        description: Some("IP address".to_string()),
                        syntax: Some("IpAddress".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.20.1.2".to_string(),
                    OidEntry {
                        name: "ip/{index}/if_index".to_string(),
                        module: None,
                        description: Some("Interface index".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.20.1.3".to_string(),
                    OidEntry {
                        name: "ip/{index}/netmask".to_string(),
                        module: None,
                        description: Some("Subnet mask".to_string()),
                        syntax: Some("IpAddress".to_string()),
                        is_table_entry: true,
                    },
                ),
            ]),
        };
        self.load_definition(def);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_mibs() {
        let mut resolver = MibResolver::new();
        resolver.load_builtin_mibs().unwrap();

        // Check SNMPv2-MIB
        assert_eq!(resolver.resolve("1.3.6.1.2.1.1.3.0"), "system/uptime");
        assert_eq!(resolver.resolve("1.3.6.1.2.1.1.5.0"), "system/name");

        // Check IF-MIB scalars
        assert_eq!(resolver.resolve("1.3.6.1.2.1.2.1.0"), "if_number");

        // Check IF-MIB table entries
        assert_eq!(resolver.resolve("1.3.6.1.2.1.2.2.1.10.1"), "if/1/in_octets");
        assert_eq!(
            resolver.resolve("1.3.6.1.2.1.2.2.1.16.5"),
            "if/5/out_octets"
        );

        // Check HOST-RESOURCES-MIB
        assert_eq!(resolver.resolve("1.3.6.1.2.1.25.1.1.0"), "host/uptime");
        assert_eq!(resolver.resolve("1.3.6.1.2.1.25.3.3.1.2.1"), "cpu/1/load");

        // Unknown OID returns as-is
        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.9.9.999.0"),
            "1.3.6.1.4.1.9.9.999.0"
        );
    }

    /// #559: every built-in MIB name is chunk-grammar-valid (profile
    /// convention, lowercase snake), so a debug-build poll cycle publishing
    /// them never trips the metric guard.
    #[test]
    fn builtin_names_are_chunk_grammar_valid() {
        let mut resolver = MibResolver::new();
        resolver.load_builtin_mibs().unwrap();
        let names = resolver
            .exact_mappings
            .values()
            .chain(resolver.prefix_mappings.iter().map(|(_, e)| e))
            .map(|e| e.name.as_str());
        for name in names {
            for chunk in name.split('/') {
                let plain = chunk.replace("{index}", "1");
                assert!(
                    zenkey::grammar::is_valid_plain_chunk(&plain),
                    "built-in MIB name {name:?} violates the chunk grammar (#559)"
                );
            }
        }
    }

    /// #581: prefix matching must respect OID chunk boundaries. With only
    /// column ...1 (ifIndex) registered, an instance of the *unregistered*
    /// sibling column ...10 (ifInOctets) is a string-prefix match of it —
    /// `starts_with` alone resolves it to `ifIndex.0.3` / the wrong syntax,
    /// silently.
    #[test]
    fn prefix_match_respects_dot_boundaries() {
        let mut resolver = MibResolver::new();
        resolver.load_definition(MibDefinition {
            module: "TEST-MIB".to_string(),
            description: None,
            oids: HashMap::from([(
                "1.3.6.1.2.1.2.2.1.1".to_string(),
                OidEntry {
                    name: "ifIndex".to_string(),
                    module: None,
                    description: None,
                    syntax: Some("INTEGER".to_string()),
                    is_table_entry: true,
                },
            )]),
        });

        // The registered column still resolves, with its index.
        assert_eq!(resolver.resolve("1.3.6.1.2.1.2.2.1.1.3"), "ifIndex.3");
        assert_eq!(resolver.syntax("1.3.6.1.2.1.2.2.1.1.3"), Some("INTEGER"));

        // An adjacent, unregistered column that is a string prefix match
        // (...1 vs ...10) must fall through untouched.
        assert_eq!(
            resolver.resolve("1.3.6.1.2.1.2.2.1.10.3"),
            "1.3.6.1.2.1.2.2.1.10.3"
        );
        assert_eq!(resolver.syntax("1.3.6.1.2.1.2.2.1.10.3"), None);
    }

    #[test]
    fn test_load_json() {
        let mut resolver = MibResolver::new();

        let json = r#"{
            "module": "TEST-MIB",
            "description": "Test MIB",
            "oids": {
                "1.3.6.1.4.1.12345.1.0": {
                    "name": "testScalar.0",
                    "syntax": "INTEGER"
                },
                "1.3.6.1.4.1.12345.2.1.1": {
                    "name": "testTableEntry",
                    "is_table_entry": true
                }
            }
        }"#;

        resolver.load_json(json).unwrap();

        assert_eq!(resolver.resolve("1.3.6.1.4.1.12345.1.0"), "testScalar.0");
        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.12345.2.1.1.5"),
            "testTableEntry.5"
        );
    }

    #[test]
    fn test_custom_mappings() {
        let mut resolver = MibResolver::new();

        let mut custom = HashMap::new();
        custom.insert(
            "1.3.6.1.4.1.9999.1.0".to_string(),
            "myCustomOid.0".to_string(),
        );
        custom.insert(
            "1.3.6.1.4.1.9999.2.1".to_string(),
            "myTable/{index}/value".to_string(),
        );

        resolver.add_custom_mappings(&custom);

        assert_eq!(resolver.resolve("1.3.6.1.4.1.9999.1.0"), "myCustomOid.0");
        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.9999.2.1.3"),
            "myTable/3/value"
        );
    }

    #[test]
    fn test_loaded_modules() {
        let mut resolver = MibResolver::new();
        resolver.load_builtin_mibs().unwrap();

        let modules = resolver.loaded_modules();
        assert!(modules.contains(&"SNMPv2-MIB".to_string()));
        assert!(modules.contains(&"IF-MIB".to_string()));
        assert!(modules.contains(&"HOST-RESOURCES-MIB".to_string()));
        assert!(modules.contains(&"IP-MIB".to_string()));
    }
}
