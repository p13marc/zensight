//! MIB (Management Information Base) loading and OID resolution.
//!
//! This module provides functionality to load pre-compiled OID-to-name mappings
//! from JSON files. These mappings are derived from standard MIB definitions
//! (IF-MIB, SNMPv2-MIB, HOST-RESOURCES-MIB, etc.) and allow the bridge to
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
//! assert_eq!(name, "sysUpTime.0");
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
    /// Human-readable name (e.g., "sysUpTime", "ifInOctets").
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
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()));
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
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()));
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
            if oid.starts_with(prefix) {
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

    /// Get detailed entry for an OID if available.
    pub fn get_entry(&self, oid: &str) -> Option<&OidEntry> {
        if let Some(entry) = self.exact_mappings.get(oid) {
            return Some(entry);
        }

        for (prefix, entry) in &self.prefix_mappings {
            if oid.starts_with(prefix) {
                return Some(entry);
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
                        name: "sysDescr.0".to_string(),
                        module: None,
                        description: Some("System description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.2.0".to_string(),
                    OidEntry {
                        name: "sysObjectID.0".to_string(),
                        module: None,
                        description: Some("System object identifier".to_string()),
                        syntax: Some("OBJECT IDENTIFIER".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.3.0".to_string(),
                    OidEntry {
                        name: "sysUpTime.0".to_string(),
                        module: None,
                        description: Some("Time since system started".to_string()),
                        syntax: Some("TimeTicks".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.4.0".to_string(),
                    OidEntry {
                        name: "sysContact.0".to_string(),
                        module: None,
                        description: Some("Contact person for system".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.5.0".to_string(),
                    OidEntry {
                        name: "sysName.0".to_string(),
                        module: None,
                        description: Some("System name".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.6.0".to_string(),
                    OidEntry {
                        name: "sysLocation.0".to_string(),
                        module: None,
                        description: Some("Physical location of system".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.1.7.0".to_string(),
                    OidEntry {
                        name: "sysServices.0".to_string(),
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
                        name: "snmpInPkts.0".to_string(),
                        module: None,
                        description: Some("Total SNMP messages received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.11.2.0".to_string(),
                    OidEntry {
                        name: "snmpOutPkts.0".to_string(),
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
                        name: "ifNumber.0".to_string(),
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
                        name: "ifIndex".to_string(),
                        module: None,
                        description: Some("Interface index".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.2".to_string(),
                    OidEntry {
                        name: "ifDescr".to_string(),
                        module: None,
                        description: Some("Interface description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.3".to_string(),
                    OidEntry {
                        name: "ifType".to_string(),
                        module: None,
                        description: Some("Interface type".to_string()),
                        syntax: Some("IANAifType".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.4".to_string(),
                    OidEntry {
                        name: "ifMtu".to_string(),
                        module: None,
                        description: Some("Interface MTU".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.5".to_string(),
                    OidEntry {
                        name: "ifSpeed".to_string(),
                        module: None,
                        description: Some("Interface speed (bps)".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.6".to_string(),
                    OidEntry {
                        name: "ifPhysAddress".to_string(),
                        module: None,
                        description: Some("Interface MAC address".to_string()),
                        syntax: Some("PhysAddress".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.7".to_string(),
                    OidEntry {
                        name: "ifAdminStatus".to_string(),
                        module: None,
                        description: Some("Desired interface state".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.8".to_string(),
                    OidEntry {
                        name: "ifOperStatus".to_string(),
                        module: None,
                        description: Some("Current interface state".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.9".to_string(),
                    OidEntry {
                        name: "ifLastChange".to_string(),
                        module: None,
                        description: Some("Last status change time".to_string()),
                        syntax: Some("TimeTicks".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.10".to_string(),
                    OidEntry {
                        name: "ifInOctets".to_string(),
                        module: None,
                        description: Some("Bytes received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.11".to_string(),
                    OidEntry {
                        name: "ifInUcastPkts".to_string(),
                        module: None,
                        description: Some("Unicast packets received".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.13".to_string(),
                    OidEntry {
                        name: "ifInDiscards".to_string(),
                        module: None,
                        description: Some("Inbound discards".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.14".to_string(),
                    OidEntry {
                        name: "ifInErrors".to_string(),
                        module: None,
                        description: Some("Inbound errors".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.16".to_string(),
                    OidEntry {
                        name: "ifOutOctets".to_string(),
                        module: None,
                        description: Some("Bytes sent".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.17".to_string(),
                    OidEntry {
                        name: "ifOutUcastPkts".to_string(),
                        module: None,
                        description: Some("Unicast packets sent".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.19".to_string(),
                    OidEntry {
                        name: "ifOutDiscards".to_string(),
                        module: None,
                        description: Some("Outbound discards".to_string()),
                        syntax: Some("Counter32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.2.2.1.20".to_string(),
                    OidEntry {
                        name: "ifOutErrors".to_string(),
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
                        name: "ifName".to_string(),
                        module: None,
                        description: Some("Interface name".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.6".to_string(),
                    OidEntry {
                        name: "ifHCInOctets".to_string(),
                        module: None,
                        description: Some("Bytes received (64-bit)".to_string()),
                        syntax: Some("Counter64".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.10".to_string(),
                    OidEntry {
                        name: "ifHCOutOctets".to_string(),
                        module: None,
                        description: Some("Bytes sent (64-bit)".to_string()),
                        syntax: Some("Counter64".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.15".to_string(),
                    OidEntry {
                        name: "ifHighSpeed".to_string(),
                        module: None,
                        description: Some("Interface speed (Mbps)".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.31.1.1.1.18".to_string(),
                    OidEntry {
                        name: "ifAlias".to_string(),
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
                        name: "hrSystemUptime.0".to_string(),
                        module: None,
                        description: Some("Host uptime".to_string()),
                        syntax: Some("TimeTicks".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.1.2.0".to_string(),
                    OidEntry {
                        name: "hrSystemDate.0".to_string(),
                        module: None,
                        description: Some("Current date and time".to_string()),
                        syntax: Some("DateAndTime".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.1.5.0".to_string(),
                    OidEntry {
                        name: "hrSystemNumUsers.0".to_string(),
                        module: None,
                        description: Some("Number of logged in users".to_string()),
                        syntax: Some("Gauge32".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.1.6.0".to_string(),
                    OidEntry {
                        name: "hrSystemProcesses.0".to_string(),
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
                        name: "hrStorageIndex".to_string(),
                        module: None,
                        description: Some("Storage index".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.2".to_string(),
                    OidEntry {
                        name: "hrStorageType".to_string(),
                        module: None,
                        description: Some("Storage type".to_string()),
                        syntax: Some("OBJECT IDENTIFIER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.3".to_string(),
                    OidEntry {
                        name: "hrStorageDescr".to_string(),
                        module: None,
                        description: Some("Storage description".to_string()),
                        syntax: Some("DisplayString".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.4".to_string(),
                    OidEntry {
                        name: "hrStorageAllocationUnits".to_string(),
                        module: None,
                        description: Some("Allocation unit size".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.5".to_string(),
                    OidEntry {
                        name: "hrStorageSize".to_string(),
                        module: None,
                        description: Some("Total storage units".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.25.2.3.1.6".to_string(),
                    OidEntry {
                        name: "hrStorageUsed".to_string(),
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
                        name: "hrProcessorLoad".to_string(),
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
                        name: "ipForwarding.0".to_string(),
                        module: None,
                        description: Some("IP forwarding enabled".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.2.0".to_string(),
                    OidEntry {
                        name: "ipDefaultTTL.0".to_string(),
                        module: None,
                        description: Some("Default TTL".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: false,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.3.0".to_string(),
                    OidEntry {
                        name: "ipInReceives.0".to_string(),
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
                        name: "ipAdEntAddr".to_string(),
                        module: None,
                        description: Some("IP address".to_string()),
                        syntax: Some("IpAddress".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.20.1.2".to_string(),
                    OidEntry {
                        name: "ipAdEntIfIndex".to_string(),
                        module: None,
                        description: Some("Interface index".to_string()),
                        syntax: Some("INTEGER".to_string()),
                        is_table_entry: true,
                    },
                ),
                (
                    "1.3.6.1.2.1.4.20.1.3".to_string(),
                    OidEntry {
                        name: "ipAdEntNetMask".to_string(),
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
        assert_eq!(resolver.resolve("1.3.6.1.2.1.1.3.0"), "sysUpTime.0");
        assert_eq!(resolver.resolve("1.3.6.1.2.1.1.5.0"), "sysName.0");

        // Check IF-MIB scalars
        assert_eq!(resolver.resolve("1.3.6.1.2.1.2.1.0"), "ifNumber.0");

        // Check IF-MIB table entries
        assert_eq!(resolver.resolve("1.3.6.1.2.1.2.2.1.10.1"), "ifInOctets.1");
        assert_eq!(resolver.resolve("1.3.6.1.2.1.2.2.1.16.5"), "ifOutOctets.5");

        // Check HOST-RESOURCES-MIB
        assert_eq!(
            resolver.resolve("1.3.6.1.2.1.25.1.1.0"),
            "hrSystemUptime.0"
        );
        assert_eq!(
            resolver.resolve("1.3.6.1.2.1.25.3.3.1.2.1"),
            "hrProcessorLoad.1"
        );

        // Unknown OID returns as-is
        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.9.9.999.0"),
            "1.3.6.1.4.1.9.9.999.0"
        );
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

        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.12345.1.0"),
            "testScalar.0"
        );
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

        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.9999.1.0"),
            "myCustomOid.0"
        );
        assert_eq!(
            resolver.resolve("1.3.6.1.4.1.9999.2.1.3"),
            "myTable/3/value"
        );
    }

    #[test]
    fn test_get_entry() {
        let mut resolver = MibResolver::new();
        resolver.load_builtin_mibs().unwrap();

        let entry = resolver.get_entry("1.3.6.1.2.1.1.3.0").unwrap();
        assert_eq!(entry.name, "sysUpTime.0");
        assert_eq!(entry.syntax, Some("TimeTicks".to_string()));

        let table_entry = resolver.get_entry("1.3.6.1.2.1.2.2.1.10.1").unwrap();
        assert_eq!(table_entry.name, "ifInOctets");
        assert!(table_entry.is_table_entry);
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
