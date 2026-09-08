//! Semantic field resolution: what a flow record *means*, independent of the
//! version that carried it (#1072).
//!
//! **The bug this exists to make impossible.** `rollup.rs` read
//! `record.fields.get("bytes")`, `get("packets")` and `get("protocol")`. Those
//! literal keys are minted only by the v5/v7 parsers. v9 and IPFIX minted their
//! keys as `format!("{:?}", field_type).to_lowercase()`, so against
//! `netflow_parser` the actual keys were:
//!
//! | Version | bytes | packets | protocol |
//! |---|---|---|---|
//! | v5 / v7 | `bytes` | `packets` | `protocol` |
//! | v9 | `inbytes` | `inpkts` | `protocol` |
//! | IPFIX | `iana(octetdeltacount)` | `iana(packetdeltacount)` | `iana(protocolidentifier)` |
//!
//! So on the only two versions anyone deploys today, `{exporter}/bytes_total`
//! and `packets_total` stayed at **0 forever** while `flows_total` counted
//! correctly — which is exactly the shape that makes an exporter look healthy.
//! IPFIX's protocol breakdown was entirely `unknown`; v9's happened to work, by
//! coincidence, because `V9Field::Protocol` lowercases to `protocol`.
//!
//! **Why this is a typed match and not a string table.** A `Debug` rendering is
//! not a stable API — `IPFixField` is an enum of enums, so its `Debug` carries
//! the wrapper and its parentheses, and a library refactor renames every key
//! this sensor publishes without a compile error anywhere. The semantic names
//! below are minted from the *variants*, so the same refactor is a build
//! failure in this file, which is where it belongs.
//!
//! The raw `{:?}`-derived names are still inserted, because they are the
//! record's own detail and `@rpc/netflow/flows` serves them. These canonical
//! keys sit beside them.

use netflow_parser::variable_versions::ipfix::lookup::{IANAIPFixField, IPFixField};
use netflow_parser::variable_versions::v9::lookup::V9Field;

/// `record.fields` key for total bytes in a flow, whatever version carried it.
pub const BYTES: &str = "bytes";
/// `record.fields` key for total packets.
pub const PACKETS: &str = "packets";
/// `record.fields` key for the IP protocol number.
pub const PROTOCOL: &str = "protocol";
/// `record.fields` key for the exporter's declared sampling interval —
/// "1 flow in N" (#1075).
pub const SAMPLING_INTERVAL: &str = "sampling_interval";

/// The semantic name a v9 field carries, if it is one this sensor rolls up.
///
/// `OutBytes`/`OutPkts` map to the same names as `InBytes`/`InPkts`: a record
/// carries one direction or the other, never both under one template, and a
/// rollup that only understood ingress would report zero on an egress-only
/// exporter — the same failure one layer down.
pub fn v9_semantic(field: &V9Field) -> Option<&'static str> {
    Some(match field {
        V9Field::InBytes | V9Field::OutBytes => BYTES,
        V9Field::InPkts | V9Field::OutPkts => PACKETS,
        V9Field::Protocol => PROTOCOL,
        // 34 is "1 in N" sampling; 50 is the sampler's own random interval,
        // which carries the same meaning for a rate correction.
        V9Field::SamplingInterval | V9Field::FlowSamplerRandomInterval => SAMPLING_INTERVAL,
        _ => return None,
    })
}

/// The semantic name an IPFIX field carries, if it is one this sensor rolls up.
///
/// Only IANA-registered elements: an enterprise element with the same meaning
/// is vendor-specific and would need its PEN checked, which is a different
/// question from "did the standard field arrive".
pub fn ipfix_semantic(field: &IPFixField) -> Option<&'static str> {
    let IPFixField::IANA(f) = field else {
        return None;
    };
    Some(match f {
        IANAIPFixField::OctetDeltaCount | IANAIPFixField::PostOctetDeltaCount => BYTES,
        IANAIPFixField::PacketDeltaCount | IANAIPFixField::PostPacketDeltaCount => PACKETS,
        IANAIPFixField::ProtocolIdentifier => PROTOCOL,
        IANAIPFixField::SamplingInterval
        | IANAIPFixField::SamplerRandomInterval
        | IANAIPFixField::SamplingPacketInterval
        | IANAIPFixField::SamplingFlowInterval => SAMPLING_INTERVAL,
        _ => return None,
    })
}

/// What each exporter last declared about its sampling (#1075).
///
/// **What this fixes.** Nothing read the sampling rate — not the v5 header
/// field, not the v9/IPFIX options template. `Rollups::ingest` summed raw
/// octets, so a router configured `1-out-of-1000` published `bytes_total` at a
/// **thousandth of throughput** as a plain `Counter`, and a consumer rating it
/// was three orders of magnitude low with nothing on the wire to say so.
///
/// The interval is declared out of band — once per options-template refresh, or
/// once per v5 datagram — so it is remembered per exporter rather than carried
/// on every record. Bounded by the same exporter cap the parser map has: a
/// spoofed source cannot grow this either.
#[derive(Debug, Default)]
pub struct SamplingRegistry {
    per_exporter: std::collections::HashMap<String, u32>,
}

/// The registry, shared between the listeners that learn the interval and the
/// rollup task that applies it.
pub type SharedSampling = std::sync::Arc<std::sync::Mutex<SamplingRegistry>>;

/// A shared, empty registry.
pub fn new_sampling() -> SharedSampling {
    std::sync::Arc::new(std::sync::Mutex::new(SamplingRegistry::default()))
}

impl SamplingRegistry {
    /// The most exporters remembered, matching the parser map's own cap.
    pub const MAX_EXPORTERS: usize = 512;

    /// Record an exporter's declared "1 in N".
    ///
    /// `n <= 1` is unsampled and is recorded as such rather than ignored: an
    /// exporter that turns sampling *off* must stop scaling, and forgetting the
    /// declaration would leave the last one in force forever.
    pub fn observe(&mut self, exporter: &str, n: u32) {
        if self.per_exporter.len() >= Self::MAX_EXPORTERS
            && !self.per_exporter.contains_key(exporter)
        {
            return;
        }
        self.per_exporter.insert(exporter.to_string(), n.max(1));
    }

    /// The exporter's declared interval, or `None` if it never said.
    ///
    /// `None` and `Some(1)` are different facts and the rollup labels them
    /// differently: "nobody told us" is not "we know it is unsampled".
    pub fn interval(&self, exporter: &str) -> Option<u32> {
        self.per_exporter.get(exporter).copied()
    }
}

/// The "1 in N" a v5 header's `sampling_interval` declares (#1075).
///
/// RFC-wise the field is two bits of mode and fourteen of interval. Mode 0 is
/// "no sampling configured", in which case the interval bits are meaningless
/// and must not be read — an exporter with mode 0 and stale interval bits would
/// otherwise have its counters scaled by a number it never meant.
pub fn v5_sampling(raw: u16) -> Option<u32> {
    let mode = raw >> 14;
    let interval = u32::from(raw & 0x3FFF);
    if mode == 0 || interval <= 1 {
        return None;
    }
    Some(interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four semantics, resolved from the typed variants of both versions.
    /// A library rename breaks this file rather than silently renaming every
    /// key the sensor publishes (#1072).
    #[test]
    fn both_versions_resolve_the_same_four_semantics() {
        assert_eq!(v9_semantic(&V9Field::InBytes), Some(BYTES));
        assert_eq!(v9_semantic(&V9Field::OutBytes), Some(BYTES));
        assert_eq!(v9_semantic(&V9Field::InPkts), Some(PACKETS));
        assert_eq!(v9_semantic(&V9Field::Protocol), Some(PROTOCOL));
        assert_eq!(
            v9_semantic(&V9Field::SamplingInterval),
            Some(SAMPLING_INTERVAL)
        );
        assert_eq!(v9_semantic(&V9Field::L4SrcPort), None);

        let iana = |f| IPFixField::IANA(f);
        assert_eq!(
            ipfix_semantic(&iana(IANAIPFixField::OctetDeltaCount)),
            Some(BYTES)
        );
        assert_eq!(
            ipfix_semantic(&iana(IANAIPFixField::PacketDeltaCount)),
            Some(PACKETS)
        );
        assert_eq!(
            ipfix_semantic(&iana(IANAIPFixField::ProtocolIdentifier)),
            Some(PROTOCOL)
        );
        assert_eq!(
            ipfix_semantic(&iana(IANAIPFixField::SamplerRandomInterval)),
            Some(SAMPLING_INTERVAL)
        );
        assert_eq!(
            ipfix_semantic(&iana(IANAIPFixField::SourceTransportPort)),
            None
        );
    }

    /// An enterprise element is not an IANA one, whatever it happens to mean.
    #[test]
    fn an_enterprise_element_resolves_to_nothing() {
        assert_eq!(
            ipfix_semantic(&IPFixField::Enterprise {
                enterprise_number: 9,
                field_number: 1,
            }),
            None
        );
    }

    /// v5's header packs the mode above the interval, and mode 0 means the
    /// interval bits are meaningless (#1075).
    #[test]
    fn a_v5_header_declares_sampling_only_when_a_mode_is_set() {
        // mode 1 (deterministic), 1-in-1000
        assert_eq!(v5_sampling((1 << 14) | 1000), Some(1000));
        // mode 2 (random), 1-in-100
        assert_eq!(v5_sampling((2 << 14) | 100), Some(100));
        // mode 0: not configured — the interval bits are not a declaration,
        // whatever they happen to contain.
        assert_eq!(v5_sampling(1000), None);
        assert_eq!(v5_sampling(0), None);
        // 1-in-1 is not sampling.
        assert_eq!(v5_sampling((1 << 14) | 1), None);
    }

    /// "Nobody told us" and "told us it is unsampled" are different facts, and
    /// an exporter that turns sampling off must stop scaling (#1075).
    #[test]
    fn the_registry_distinguishes_silence_from_unsampled() {
        let mut r = SamplingRegistry::default();
        assert_eq!(r.interval("edge01"), None);
        r.observe("edge01", 1000);
        assert_eq!(r.interval("edge01"), Some(1000));
        r.observe("edge01", 1);
        assert_eq!(r.interval("edge01"), Some(1), "off is a declaration too");
        // Bounded: a spoofed source cannot grow this map past the cap.
        for i in 0..SamplingRegistry::MAX_EXPORTERS + 10 {
            r.observe(&format!("spoof-{i}"), 5);
        }
        assert!(r.per_exporter.len() <= SamplingRegistry::MAX_EXPORTERS);
        assert_eq!(
            r.interval("edge01"),
            Some(1),
            "and a known one is not evicted"
        );
    }

    /// The keys the rollup looks up are the ones this module mints. Stated as a
    /// test because the two used to disagree silently, in the direction that
    /// reads as "the exporter is sending zero bytes".
    #[test]
    fn the_rollup_looks_up_exactly_these_keys() {
        assert_eq!(BYTES, "bytes");
        assert_eq!(PACKETS, "packets");
        assert_eq!(PROTOCOL, "protocol");
    }
}
