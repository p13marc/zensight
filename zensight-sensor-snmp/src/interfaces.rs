//! Builds the per-device [`InterfaceTable`] state doc from one poll cycle's
//! walked IF-MIB values (#529) — the join done once, at the source.

use zensight_common::{IfStatus, InterfaceEntry, InterfaceTable};

const IF_TABLE: &str = "1.3.6.1.2.1.2.2.1";
const IF_X_TABLE: &str = "1.3.6.1.2.1.31.1.1.1";

/// Accumulates walked IF-MIB rows over one poll cycle.
#[derive(Default)]
pub struct TableBuilder {
    rows: std::collections::BTreeMap<u32, InterfaceEntry>,
}

impl TableBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any interface data was seen this cycle.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Finish the cycle: interfaces sorted by index.
    pub fn build(self, device: &str) -> InterfaceTable {
        InterfaceTable {
            timestamp: zensight_common::current_timestamp_millis(),
            device: device.to_string(),
            interfaces: self
                .rows
                .into_iter()
                .map(|(index, mut e)| {
                    e.index = index;
                    // ifName (set from ifXTable) wins; fall back to ifDescr.
                    if e.name.is_none() {
                        e.name = e.descr.clone();
                    }
                    e
                })
                .collect(),
        }
    }

    /// Ingest one polled value (+ derived per-second rate when the poller
    /// computed one). Non-IF-MIB OIDs are ignored.
    pub fn ingest(&mut self, oid: &str, value: &async_snmp::Value, rate: Option<f64>) {
        use async_snmp::Value;

        if let Some((column, index)) = split_column(oid, IF_TABLE) {
            let e = self.rows.entry(index).or_default();
            match (column, value) {
                (2, Value::OctetString(s)) => e.descr = utf8(s),
                // ifSpeed only when ifHighSpeed hasn't claimed it.
                (5, Value::Gauge32(n)) if e.speed_bits.is_none() => {
                    e.speed_bits = Some(u64::from(*n));
                }
                (6, Value::OctetString(s)) => e.mac = mac(s),
                (7, Value::Integer(n)) => e.admin_status = Some(IfStatus::from_wire(*n)),
                (8, Value::Integer(n)) => e.oper_status = Some(IfStatus::from_wire(*n)),
                (10, Value::Counter32(n)) => {
                    set_pref(&mut e.counters.in_octets, u64::from(*n), false);
                    set_rate_pref(&mut e.rates.in_octets_per_sec, rate, false);
                }
                (11, Value::Counter32(n)) => {
                    set_pref(&mut e.counters.in_packets, u64::from(*n), false);
                    set_rate_pref(&mut e.rates.in_packets_per_sec, rate, false);
                }
                (13, Value::Counter32(n)) => {
                    e.counters.in_discards = Some(u64::from(*n));
                    e.rates.in_discards_per_sec = rate.or(e.rates.in_discards_per_sec);
                }
                (14, Value::Counter32(n)) => {
                    e.counters.in_errors = Some(u64::from(*n));
                    e.rates.in_errors_per_sec = rate.or(e.rates.in_errors_per_sec);
                }
                (16, Value::Counter32(n)) => {
                    set_pref(&mut e.counters.out_octets, u64::from(*n), false);
                    set_rate_pref(&mut e.rates.out_octets_per_sec, rate, false);
                }
                (17, Value::Counter32(n)) => {
                    set_pref(&mut e.counters.out_packets, u64::from(*n), false);
                    set_rate_pref(&mut e.rates.out_packets_per_sec, rate, false);
                }
                (19, Value::Counter32(n)) => {
                    e.counters.out_discards = Some(u64::from(*n));
                    e.rates.out_discards_per_sec = rate.or(e.rates.out_discards_per_sec);
                }
                (20, Value::Counter32(n)) => {
                    e.counters.out_errors = Some(u64::from(*n));
                    e.rates.out_errors_per_sec = rate.or(e.rates.out_errors_per_sec);
                }
                _ => {}
            }
        } else if let Some((column, index)) = split_column(oid, IF_X_TABLE) {
            let e = self.rows.entry(index).or_default();
            match (column, value) {
                (1, Value::OctetString(s)) => e.name = utf8(s),
                (6, Value::Counter64(n)) => {
                    set_pref(&mut e.counters.in_octets, *n, true);
                    set_rate_pref(&mut e.rates.in_octets_per_sec, rate, true);
                }
                (7, Value::Counter64(n)) => {
                    set_pref(&mut e.counters.in_packets, *n, true);
                    set_rate_pref(&mut e.rates.in_packets_per_sec, rate, true);
                }
                (10, Value::Counter64(n)) => {
                    set_pref(&mut e.counters.out_octets, *n, true);
                    set_rate_pref(&mut e.rates.out_octets_per_sec, rate, true);
                }
                (11, Value::Counter64(n)) => {
                    set_pref(&mut e.counters.out_packets, *n, true);
                    set_rate_pref(&mut e.rates.out_packets_per_sec, rate, true);
                }
                (15, Value::Gauge32(n)) => e.speed_bits = Some(u64::from(*n) * 1_000_000),
                (18, Value::OctetString(s)) => {
                    e.alias = utf8(s).filter(|a| !a.is_empty());
                }
                _ => {}
            }
        }
    }
}

/// HC columns overwrite 32-bit values; 32-bit only fills gaps.
fn set_pref(slot: &mut Option<u64>, value: u64, is_hc: bool) {
    if is_hc || slot.is_none() {
        *slot = Some(value);
    }
}

fn set_rate_pref(slot: &mut Option<f64>, rate: Option<f64>, is_hc: bool) {
    if let Some(r) = rate
        && (is_hc || slot.is_none())
    {
        *slot = Some(r);
    }
}

fn utf8(s: &bytes::Bytes) -> Option<String> {
    String::from_utf8(s.to_vec()).ok()
}

/// ifPhysAddress bytes → `aa:bb:cc:dd:ee:ff` (None when empty/loopback-blank).
fn mac(s: &bytes::Bytes) -> Option<String> {
    if s.is_empty() || s.iter().all(|b| *b == 0) {
        return None;
    }
    Some(
        s.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// Split `<prefix>.<column>.<index>` → (column, index).
fn split_column(oid: &str, prefix: &str) -> Option<(u32, u32)> {
    let rest = oid.strip_prefix(prefix)?.strip_prefix('.')?;
    let (column, index) = rest.split_once('.')?;
    Some((column.parse().ok()?, index.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_snmp::Value;
    use bytes::Bytes;

    #[test]
    fn joins_if_table_and_ifx_table() {
        let mut b = TableBuilder::new();
        b.ingest(
            "1.3.6.1.2.1.2.2.1.2.1",
            &Value::OctetString(Bytes::from_static(b"GigabitEthernet0/1")),
            None,
        );
        b.ingest("1.3.6.1.2.1.2.2.1.5.1", &Value::Gauge32(100_000_000), None);
        b.ingest(
            "1.3.6.1.2.1.2.2.1.6.1",
            &Value::OctetString(Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x01])),
            None,
        );
        b.ingest("1.3.6.1.2.1.2.2.1.7.1", &Value::Integer(1), None);
        b.ingest("1.3.6.1.2.1.2.2.1.8.1", &Value::Integer(2), None);
        b.ingest(
            "1.3.6.1.2.1.2.2.1.10.1",
            &Value::Counter32(1_000),
            Some(10.0),
        );
        // ifXTable refinements.
        b.ingest(
            "1.3.6.1.2.1.31.1.1.1.1.1",
            &Value::OctetString(Bytes::from_static(b"Gi0/1")),
            None,
        );
        b.ingest(
            "1.3.6.1.2.1.31.1.1.1.6.1",
            &Value::Counter64(5_000_000_000),
            Some(1_000.0),
        );
        b.ingest("1.3.6.1.2.1.31.1.1.1.15.1", &Value::Gauge32(1_000), None);
        b.ingest(
            "1.3.6.1.2.1.31.1.1.1.18.1",
            &Value::OctetString(Bytes::from_static(b"uplink to core")),
            None,
        );

        let doc = b.build("router01");
        assert_eq!(doc.device, "router01");
        let e = &doc.interfaces[0];
        assert_eq!(e.index, 1);
        assert_eq!(e.name.as_deref(), Some("Gi0/1")); // ifName beats ifDescr
        assert_eq!(e.descr.as_deref(), Some("GigabitEthernet0/1"));
        assert_eq!(e.alias.as_deref(), Some("uplink to core"));
        assert_eq!(e.mac.as_deref(), Some("de:ad:be:ef:00:01"));
        assert_eq!(e.admin_status, Some(IfStatus::Up));
        assert_eq!(e.oper_status, Some(IfStatus::Down));
        assert_eq!(e.speed_bits, Some(1_000_000_000)); // ifHighSpeed wins
        assert_eq!(e.counters.in_octets, Some(5_000_000_000)); // HC wins
        assert_eq!(e.rates.in_octets_per_sec, Some(1_000.0)); // HC rate wins
    }

    #[test]
    fn no_ifx_table_falls_back_gracefully() {
        let mut b = TableBuilder::new();
        b.ingest(
            "1.3.6.1.2.1.2.2.1.2.2",
            &Value::OctetString(Bytes::from_static(b"eth1")),
            None,
        );
        b.ingest("1.3.6.1.2.1.2.2.1.5.2", &Value::Gauge32(10_000_000), None);
        b.ingest("1.3.6.1.2.1.2.2.1.10.2", &Value::Counter32(500), Some(5.0));

        let doc = b.build("legacy01");
        let e = &doc.interfaces[0];
        assert_eq!(e.name.as_deref(), Some("eth1")); // ifDescr fallback
        assert_eq!(e.speed_bits, Some(10_000_000)); // ifSpeed fallback
        assert_eq!(e.counters.in_octets, Some(500));
        assert_eq!(e.rates.in_octets_per_sec, Some(5.0));
    }

    #[test]
    fn empty_mac_and_alias_stay_absent() {
        let mut b = TableBuilder::new();
        b.ingest(
            "1.3.6.1.2.1.2.2.1.6.1",
            &Value::OctetString(Bytes::new()),
            None,
        );
        b.ingest(
            "1.3.6.1.2.1.31.1.1.1.18.1",
            &Value::OctetString(Bytes::new()),
            None,
        );
        let doc = b.build("d");
        assert!(doc.interfaces[0].mac.is_none());
        assert!(doc.interfaces[0].alias.is_none());
    }

    #[test]
    fn interfaces_sorted_by_index() {
        let mut b = TableBuilder::new();
        b.ingest("1.3.6.1.2.1.2.2.1.10.9", &Value::Counter32(1), None);
        b.ingest("1.3.6.1.2.1.2.2.1.10.2", &Value::Counter32(1), None);
        let doc = b.build("d");
        assert_eq!(
            doc.interfaces.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![2, 9]
        );
    }
}
