use anyhow::{Result, anyhow};
use async_snmp::Oid;

/// Parse an OID string (e.g., "1.3.6.1.2.1.1.3.0") into an OID.
pub fn parse_oid(oid_str: &str) -> Result<Oid> {
    Oid::parse(oid_str).map_err(|e| anyhow!("Failed to parse OID '{}': {}", oid_str, e))
}

/// Convert an OID back to a dotted string representation.
pub fn oid_to_string(oid: &Oid) -> String {
    oid.to_string()
}

/// Check if an OID is a child of (or equal to) a parent OID.
pub fn oid_starts_with(oid: &Oid, parent: &Oid) -> bool {
    oid.starts_with(parent)
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
}
