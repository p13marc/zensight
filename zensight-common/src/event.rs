//! The `events`-class record envelope (#534).
//!
//! The keyspace convention has always defined three data classes —
//! `telemetry`, `state`, `events` (append-only records, RFC 04 §1.2) — but
//! nothing instantiated `events`: event-shaped data was smuggled as
//! droppable telemetry counters (SNMP traps) or RPC query rings. This is
//! the shared envelope for real event records: durable (reliable QoS, see
//! [`crate::QosClass::Event`]), orderable (ULID ids), and self-describing.
//!
//! Keys are `v1/<origin>/events/<producer>/<subject...>/<id>` — the ULID is
//! the last chunk, so records never overwrite each other and a storage
//! aligned on the events tree retains an append-only log.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{AlertSeverity, Protocol, current_timestamp_millis};

/// One append-only event record.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventRecord {
    /// ULID (lowercase) — unique, time-ordered; also the key's last chunk.
    pub id: String,

    /// Unix epoch milliseconds when the event was observed.
    pub timestamp: i64,

    /// The subject of the event: observed device / host identifier.
    pub source: String,

    /// Origin protocol.
    pub protocol: Protocol,

    /// Producer vocabulary for what happened, e.g. `"trap/link_down"`,
    /// `"unit/failed"` — stable, filterable.
    pub kind: String,

    pub severity: AlertSeverity,

    /// One-line human description.
    pub summary: String,

    /// The `alert_key` of the alert this event raised or cleared (#651), when
    /// the producer mapped it to one.
    ///
    /// Absent for records that drove no alert transition, and for every record
    /// written before #651 — consumers fall back to a source-scoped pivot. The
    /// alert's in-GUI identity is `<source>/<alert_key>`, and `source` is this
    /// record's own.
    ///
    /// `#[serde(default)]` is required, not cosmetic: serde's derive rejects a
    /// missing field even for an `Option`, and the frontend stores these
    /// records as raw JSON in redb — without it every row written before this
    /// field existed would fail to decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_key: Option<String>,

    /// Structured detail (e.g. decoded trap varbinds), name → rendered value.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub fields: HashMap<String, String>,
}

impl EventRecord {
    /// Create a record observed now, with a fresh ULID.
    pub fn new(
        source: impl Into<String>,
        protocol: Protocol,
        kind: impl Into<String>,
        severity: AlertSeverity,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: ulid::Ulid::new().to_string().to_ascii_lowercase(),
            timestamp: current_timestamp_millis(),
            source: source.into(),
            protocol,
            kind: kind.into(),
            severity,
            summary: summary.into(),
            alert_key: None,
            fields: HashMap::new(),
        }
    }

    /// Add one structured detail field.
    pub fn with_field(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(name.into(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_lowercase_key_chunks() {
        let ev = EventRecord::new(
            "router01",
            Protocol::Snmp,
            "trap/link_down",
            AlertSeverity::Warning,
            "eth0 went down",
        );
        assert!(zenkey::grammar::is_valid_plain_chunk(&ev.id), "{}", ev.id);
        assert_ne!(
            EventRecord::new("r", Protocol::Snmp, "k", AlertSeverity::Info, "s").id,
            ev.id
        );
    }

    #[test]
    fn roundtrips_with_fields() {
        let ev = EventRecord::new(
            "router01",
            Protocol::Snmp,
            "trap/link_down",
            AlertSeverity::Warning,
            "eth0 went down",
        )
        .with_field("if_index", "3");
        let json = serde_json::to_string(&ev).unwrap();
        let back: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.fields["if_index"], "3");
        assert_eq!(back.kind, "trap/link_down");
    }

    /// A record written before #651 must still decode. The frontend stores
    /// these as raw JSON in redb, so without `#[serde(default)]` every row on
    /// disk from an older build would fail to load — a silent history wipe on
    /// upgrade, not a compile error.
    #[test]
    fn a_pre_651_record_still_decodes() {
        let legacy = br#"{
            "id": "01k0000000000000000000000a",
            "timestamp": 1719999123000,
            "source": "router01",
            "protocol": "snmp",
            "kind": "trap/link_down",
            "severity": "warning",
            "summary": "router01: link down"
        }"#;
        let record: EventRecord = serde_json::from_slice(legacy).expect("legacy record decodes");
        assert_eq!(record.alert_key, None);
        assert!(record.fields.is_empty());
    }

    /// And the field stays off the wire when there is no alert to name, so a
    /// record that drove no transition is byte-identical to a pre-#651 one.
    #[test]
    fn no_alert_key_is_absent_from_the_wire() {
        let record = EventRecord::new(
            "router01",
            Protocol::Snmp,
            "trap/cold_start",
            AlertSeverity::Info,
            "router01: cold start",
        );
        let json = serde_json::to_string(&record).expect("encode");
        assert!(!json.contains("alert_key"), "{json}");

        let linked = EventRecord {
            alert_key: Some("00ff00ff00ff00ff".to_string()),
            ..record
        };
        let json = serde_json::to_string(&linked).expect("encode");
        assert!(
            json.contains("\"alert_key\":\"00ff00ff00ff00ff\""),
            "{json}"
        );
    }
}
