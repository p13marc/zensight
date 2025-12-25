use anyhow::{anyhow, Result};
use snmp2::Oid;
use std::collections::HashMap;

/// Parse an OID string (e.g., "1.3.6.1.2.1.1.3.0") into an snmp2::Oid.
pub fn parse_oid(oid_str: &str) -> Result<Oid<'static>> {
    oid_str
        .parse::<Oid>()
        .map_err(|e| anyhow!("Failed to parse OID '{}': {:?}", oid_str, e))
        .map(|oid| oid.to_owned())
}

/// Convert an snmp2::Oid back to a dotted string representation.
pub fn oid_to_string(oid: &Oid) -> String {
    oid.to_id_string()
}

/// Check if an OID is a child of (or equal to) a parent OID.
pub fn oid_starts_with(oid: &Oid, parent: &Oid) -> bool {
    oid.starts_with(parent)
}

/// OID name mapper for converting numeric OIDs to human-readable paths.
#[derive(Debug, Clone)]
pub struct OidNameMapper {
    /// Exact OID to name mappings.
    exact: HashMap<String, String>,
    /// Prefix OID to name pattern mappings (for table entries with {index}).
    prefixes: Vec<(String, String)>,
}

impl OidNameMapper {
    /// Create a new mapper from configuration.
    pub fn new(oid_names: &HashMap<String, String>) -> Self {
        let mut exact = HashMap::new();
        let mut prefixes = Vec::new();

        for (oid, name) in oid_names {
            if name.contains("{index}") {
                // This is a pattern for table entries
                prefixes.push((oid.clone(), name.clone()));
            } else {
                exact.insert(oid.clone(), name.clone());
            }
        }

        // Sort prefixes by length (longest first) for more specific matches
        prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        Self { exact, prefixes }
    }

    /// Get a human-readable name for an OID.
    ///
    /// Returns the mapped name if found, otherwise returns the original OID string.
    pub fn get_name(&self, oid_str: &str) -> String {
        // Check exact match first
        if let Some(name) = self.exact.get(oid_str) {
            return name.clone();
        }

        // Check prefix patterns
        for (prefix, pattern) in &self.prefixes {
            if oid_str.starts_with(prefix) {
                // Extract the index (remaining part after the prefix)
                let suffix = &oid_str[prefix.len()..];
                let index = suffix.trim_start_matches('.');

                if !index.is_empty() {
                    return pattern.replace("{index}", index);
                }
            }
        }

        // No mapping found, return the OID as-is
        oid_str.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_oid() {
        let oid = parse_oid("1.3.6.1.2.1.1.3.0").unwrap();
        assert_eq!(oid_to_string(&oid), "1.3.6.1.2.1.1.3.0");
    }

    #[test]
    fn test_oid_starts_with() {
        let parent = parse_oid("1.3.6.1.2.1.2.2.1").unwrap();
        let child = parse_oid("1.3.6.1.2.1.2.2.1.10.1").unwrap();
        let other = parse_oid("1.3.6.1.2.1.1.3.0").unwrap();

        assert!(oid_starts_with(&child, &parent));
        assert!(oid_starts_with(&parent, &parent)); // equal
        assert!(!oid_starts_with(&other, &parent));
        assert!(!oid_starts_with(&parent, &child)); // parent is shorter
    }

    #[test]
    fn test_oid_name_mapper_exact() {
        let mut names = HashMap::new();
        names.insert(
            "1.3.6.1.2.1.1.3.0".to_string(),
            "system/sysUpTime".to_string(),
        );
        names.insert(
            "1.3.6.1.2.1.1.5.0".to_string(),
            "system/sysName".to_string(),
        );

        let mapper = OidNameMapper::new(&names);

        assert_eq!(mapper.get_name("1.3.6.1.2.1.1.3.0"), "system/sysUpTime");
        assert_eq!(mapper.get_name("1.3.6.1.2.1.1.5.0"), "system/sysName");
        assert_eq!(mapper.get_name("1.3.6.1.2.1.1.1.0"), "1.3.6.1.2.1.1.1.0"); // no mapping
    }

    #[test]
    fn test_oid_name_mapper_pattern() {
        let mut names = HashMap::new();
        names.insert(
            "1.3.6.1.2.1.2.2.1.10".to_string(),
            "if/{index}/ifInOctets".to_string(),
        );
        names.insert(
            "1.3.6.1.2.1.2.2.1.16".to_string(),
            "if/{index}/ifOutOctets".to_string(),
        );

        let mapper = OidNameMapper::new(&names);

        assert_eq!(mapper.get_name("1.3.6.1.2.1.2.2.1.10.1"), "if/1/ifInOctets");
        assert_eq!(mapper.get_name("1.3.6.1.2.1.2.2.1.10.5"), "if/5/ifInOctets");
        assert_eq!(
            mapper.get_name("1.3.6.1.2.1.2.2.1.16.3"),
            "if/3/ifOutOctets"
        );
    }
}
