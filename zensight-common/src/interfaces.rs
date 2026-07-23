//! Typed per-device interface table (#529).
//!
//! The SNMP sensor walks IF-MIB's ifTable/ifXTable anyway; this model is the
//! join done **once at the source**: per interface, identity (name/descr/
//! alias), decoded statuses, speed (ifHighSpeed preferred), MAC, and the
//! HC-preferred counters with their derived per-second rates. Published as a
//! per-device LWW state doc (`state/snmp/<device>/interfaces`), replacing
//! every consumer's stringly-typed reassembly of `if/<index>/<column>`
//! metric names. The raw per-column metric tree keeps publishing unchanged.

use serde::{Deserialize, Serialize};

/// One device's joined interface table (LWW state doc, refreshed per poll).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InterfaceTable {
    /// Unix epoch milliseconds when the cycle producing this doc completed.
    pub timestamp: i64,
    /// The polled device (first subject chunk of the key).
    pub device: String,
    /// Interfaces sorted by `index`.
    pub interfaces: Vec<InterfaceEntry>,
}

/// One interface, joined across ifTable and (when present) ifXTable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InterfaceEntry {
    /// ifIndex.
    pub index: u32,

    /// Best display name: ifName when the device serves ifXTable, else ifDescr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// ifDescr verbatim (may repeat `name` on ifXTable-less devices).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descr: Option<String>,

    /// ifAlias — the operator-assigned description, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_status: Option<IfStatus>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oper_status: Option<IfStatus>,

    /// Link speed in bits/s — ifHighSpeed (Mb/s × 10⁶) preferred over ifSpeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_bits: Option<u64>,

    /// ifPhysAddress as lowercase `aa:bb:cc:dd:ee:ff`, when non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,

    /// Lifetime counters, HC (64-bit) preferred where the device serves them.
    #[serde(default)]
    pub counters: InterfaceCounters,

    /// Derived per-second rates (from the poller's rate tracker); absent on
    /// the first cycle and across counter resets.
    #[serde(default)]
    pub rates: InterfaceRates,
}

/// IF-MIB ifAdminStatus/ifOperStatus, decoded (RFC 2863).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IfStatus {
    Up,
    Down,
    Testing,
    Unknown,
    Dormant,
    NotPresent,
    LowerLayerDown,
    /// A value outside RFC 2863's enumeration, kept verbatim.
    Other(i32),
}

impl IfStatus {
    pub fn from_wire(value: i32) -> Self {
        match value {
            1 => IfStatus::Up,
            2 => IfStatus::Down,
            3 => IfStatus::Testing,
            4 => IfStatus::Unknown,
            5 => IfStatus::Dormant,
            6 => IfStatus::NotPresent,
            7 => IfStatus::LowerLayerDown,
            other => IfStatus::Other(other),
        }
    }

    pub fn is_up(self) -> bool {
        self == IfStatus::Up
    }
}

/// Lifetime counters (octets, unicast packets, errors, discards).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InterfaceCounters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_octets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_octets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_packets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_packets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_errors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_errors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_discards: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_discards: Option<u64>,
}

/// Per-second rates for the same counters (bytes/s for octets, 1/s for the
/// rest).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InterfaceRates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_octets_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_octets_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_packets_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_packets_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_errors_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_errors_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_discards_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_discards_per_sec: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_decodes_rfc2863() {
        assert_eq!(IfStatus::from_wire(1), IfStatus::Up);
        assert_eq!(IfStatus::from_wire(2), IfStatus::Down);
        assert_eq!(IfStatus::from_wire(7), IfStatus::LowerLayerDown);
        assert_eq!(IfStatus::from_wire(42), IfStatus::Other(42));
        assert!(IfStatus::from_wire(1).is_up());
        assert!(!IfStatus::from_wire(2).is_up());
    }

    #[test]
    fn doc_roundtrips_and_tolerates_absent_fields() {
        let doc = InterfaceTable {
            timestamp: 1,
            device: "router01".into(),
            interfaces: vec![InterfaceEntry {
                index: 1,
                name: Some("eth0".into()),
                admin_status: Some(IfStatus::Up),
                oper_status: Some(IfStatus::Down),
                ..Default::default()
            }],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: InterfaceTable = serde_json::from_str(&json).unwrap();
        assert_eq!(back.interfaces[0].name.as_deref(), Some("eth0"));
        assert_eq!(back.interfaces[0].oper_status, Some(IfStatus::Down));
        // Absent optional fields deserialize as their defaults.
        let sparse: InterfaceTable =
            serde_json::from_str(r#"{"timestamp":0,"device":"x","interfaces":[{"index":3}]}"#)
                .unwrap();
        assert_eq!(sparse.interfaces[0].index, 3);
        assert!(sparse.interfaces[0].counters.in_octets.is_none());
    }
}
