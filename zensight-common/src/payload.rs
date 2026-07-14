//! The shared type table (RFC 08 §5) and the payload dispatch it enables.
//!
//! RFC 08 §2 binds every registered subject to exactly one payload type, named
//! by a string in the registry TOML (`type = "TelemetryPoint"`). RFC 08 §5 then
//! requires the convention to hold "one TOML/JSON document mapping each `type`
//! name to its schema location … A `type` name not present in the type table
//! fails CI; that is what makes `type = 'TelemetryPoint'` resolvable".
//!
//! That document never existed, so the `type` column was a string nothing
//! checked and nothing could act on. It had already drifted: `parallax`
//! declared `type = "StreamDoc"` for a subject whose wire payload is a
//! [`StreamStatus`] — a name that appeared nowhere else in the workspace.
//!
//! This module is that type table, expressed as code rather than as a document
//! so that a name which does not resolve is a *compile* error rather than a
//! lint. [`decode_payload`] is what the table buys: given the registry's
//! `payload_type()` for a key, decode that key's bytes without the caller
//! knowing the type. It is the last piece a generic bus explorer needs — a
//! consumer can now go wire key → subject → type → value with no compiled-in
//! knowledge of the producer.
//!
//! The `types_are_total` test in this module is the CI enforcement RFC 08 §5
//! asks for: every `type` in every registry file must resolve here.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::serialization::decode_auto;
use crate::{Error, Result};

/// Decode `bytes` as the registry payload type named `type_name`, into a
/// serde_json [`Value`].
///
/// `type_name` is what [`zensight_keyspace::AnySubject::payload_type`] returns
/// for a key, so the caller never needs to know which producer it is talking
/// to. The wire encoding is sniffed (CBOR by default, JSON tolerated) by
/// [`decode_auto`] — the registry says *what* the payload is, not how it is
/// framed.
///
/// An unknown `type_name` is an error rather than a fallback: silently handing
/// back raw bytes would reintroduce exactly the "decode by guessing" the
/// registry exists to abolish. Callers that want the bytes anyway should ask
/// for them explicitly.
pub fn decode_payload(type_name: &str, bytes: &[u8]) -> Result<Value> {
    // One arm per entry in PAYLOAD_TYPES. Keep the two in step — the
    // `types_are_total` test below fails if a registry type has no arm.
    fn as_json<T: DeserializeOwned + serde::Serialize>(bytes: &[u8]) -> Result<Value> {
        let v: T = decode_auto(bytes)?;
        Ok(serde_json::to_value(v)?)
    }

    match type_name {
        "TelemetryPoint" => as_json::<crate::TelemetryPoint>(bytes),
        "Alert" => as_json::<crate::Alert>(bytes),
        "HealthSnapshot" => as_json::<crate::HealthSnapshot>(bytes),
        "ErrorReport" => as_json::<crate::ErrorReport>(bytes),
        "SensorInfo" => as_json::<crate::SensorInfo>(bytes),
        "ArtifactStatus" => as_json::<crate::ArtifactStatus>(bytes),
        "HostEvidence" => as_json::<crate::HostEvidence>(bytes),
        "NameObservation" => as_json::<crate::NameObservation>(bytes),
        "HostEntity" => as_json::<crate::HostEntity>(bytes),
        "AliasRecord" => as_json::<crate::AliasRecord>(bytes),
        "PdnsRecord" => as_json::<crate::PdnsRecord>(bytes),
        "StreamStatus" => as_json::<crate::StreamStatus>(bytes),
        other => Err(Error::UnknownPayloadType(other.to_string())),
    }
}

/// Every payload type name the dispatch resolves, with the crate item it names
/// — the RFC 08 §5 "shared type table", in the form the reference application
/// can actually enforce.
///
/// The second field is the schema location (crate + item). It exists for
/// `interface show`: a human asking "what *is* a `HostEvidence`?" gets pointed
/// at the definition rather than at a name.
pub const PAYLOAD_TYPES: &[(&str, &str)] = &[
    (
        "TelemetryPoint",
        "zensight_common::telemetry::TelemetryPoint",
    ),
    ("Alert", "zensight_common::alert::Alert"),
    ("HealthSnapshot", "zensight_common::health::HealthSnapshot"),
    ("ErrorReport", "zensight_common::health::ErrorReport"),
    ("SensorInfo", "zensight_common::health::SensorInfo"),
    (
        "ArtifactStatus",
        "zensight_common::artifact::ArtifactStatus",
    ),
    ("HostEvidence", "zensight_common::evidence::HostEvidence"),
    (
        "NameObservation",
        "zensight_common::evidence::NameObservation",
    ),
    ("HostEntity", "zensight_common::entity::HostEntity"),
    ("AliasRecord", "zensight_common::entity::AliasRecord"),
    ("PdnsRecord", "zensight_common::entity::PdnsRecord"),
    ("StreamStatus", "zensight_common::stream::StreamStatus"),
];

/// Where the type named `type_name` is defined, if the table knows it.
pub fn schema_location(type_name: &str) -> Option<&'static str> {
    PAYLOAD_TYPES
        .iter()
        .find(|(n, _)| *n == type_name)
        .map(|(_, loc)| *loc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::{Format, encode};

    /// RFC 08 §5: "A `type` name not present in the type table fails CI."
    ///
    /// This is that check. It walks every subject of every registry file this
    /// build compiled against and asserts the declared `type` both resolves in
    /// [`PAYLOAD_TYPES`] and has a live arm in [`decode_payload`] — so a
    /// registry entry can never again name a payload type that does not exist
    /// (which `parallax`'s `StreamDoc` did until this table was written).
    #[test]
    fn types_are_total() {
        let mut checked = 0usize;
        for (producer, toml_src) in zensight_keyspace::registry::REGISTRIES {
            let slice = zensight_keyspace::parse_slice(toml_src)
                .unwrap_or_else(|e| panic!("{producer}: registry slice does not parse: {e}"));
            for subject in &slice.subjects {
                let ty = &subject.type_name;
                assert!(
                    schema_location(ty).is_some(),
                    "{producer}: subject {:?} declares type {ty:?}, which is not in the \
                     RFC 08 §5 type table (zensight-common/src/payload.rs). Either the type \
                     is misspelled or the table needs an entry.",
                    subject.path
                );
                // The table and the dispatch must agree: a name in PAYLOAD_TYPES
                // with no match arm would resolve above and still fail on the
                // wire. Garbage bytes never decode successfully, so any error
                // *other* than UnknownPayloadType proves the arm exists.
                assert!(
                    !matches!(
                        decode_payload(ty, b"\xff\xff\xff\xff"),
                        Err(Error::UnknownPayloadType(_))
                    ),
                    "{producer}: type {ty:?} is in PAYLOAD_TYPES but has no decode_payload arm"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 300,
            "expected the full registry, walked only {checked}"
        );
    }

    #[test]
    fn decodes_a_telemetry_point_from_cbor() {
        let point = crate::TelemetryPoint::new(
            "host-1",
            crate::Protocol::Sysinfo,
            "cpu/usage",
            crate::TelemetryValue::Gauge(42.5),
        );
        let bytes = encode(&point, Format::Cbor).unwrap();

        let value = decode_payload("TelemetryPoint", &bytes).unwrap();

        assert_eq!(value["metric"], "cpu/usage");
        assert_eq!(value["source"], "host-1");
    }

    /// The dispatch is keyed off the registry, so this is the whole
    /// wire-key → value path a bus explorer walks, with nothing producer-
    /// specific compiled in.
    #[test]
    fn refines_a_wire_key_then_decodes_it() {
        let key = "v1/h-3fa9c2d41b7e/state/sysinfo/health";
        let (_, producer, subject) = crate::keyexpr::refine_key(key).expect("registered key");
        assert_eq!(producer, "sysinfo");
        assert_eq!(subject.payload_type(), "HealthSnapshot");

        // JSON on the wire is legal (decode_auto sniffs the first byte), and it
        // keeps the fixture honest: these are the bytes, not a struct we built.
        let bytes = br#"{"sensor":"sysinfo-1","status":"healthy","uptime_secs":10,
            "devices_total":0,"devices_responding":0,"devices_failed":0,
            "last_poll_duration_ms":0,"errors_last_hour":0,"metrics_published":7}"#;

        let value = decode_payload(subject.payload_type(), bytes).unwrap();
        assert_eq!(value["sensor"], "sysinfo-1");
        assert_eq!(value["metrics_published"], 7);
    }

    /// `StreamDoc` is the phantom type this table was written to catch: the
    /// parallax registry named it, nothing defined it, and the subject's real
    /// payload was a `StreamStatus` all along.
    #[test]
    fn unknown_type_is_an_error_not_a_fallback() {
        assert!(matches!(
            decode_payload("StreamDoc", b"{}"),
            Err(Error::UnknownPayloadType(_))
        ));
    }
}
