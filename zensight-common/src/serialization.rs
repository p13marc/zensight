use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

/// Serialization format for telemetry data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// JSON format (human-readable, good for debugging).
    Json,

    /// CBOR format (compact binary, better for high-volume telemetry).
    /// Default: the wire is bytes-sensitive on low-bandwidth links, and every
    /// consumer decodes via `decode_auto` (format-sniffing), so JSON senders stay
    /// interoperable during rollout.
    #[default]
    Cbor,
}

impl Format {
    /// The sample [`Encoding`](zenoh::bytes::Encoding) for payloads encoded in
    /// this format. Producers stamp it on every put so consumers resolve the
    /// payload from metadata (RFC 08 §7: sample encoding > registry > sniff)
    /// instead of first-byte sniffing.
    pub fn encoding(self) -> zenoh::bytes::Encoding {
        match self {
            Format::Json => zenoh::bytes::Encoding::APPLICATION_JSON,
            Format::Cbor => zenoh::bytes::Encoding::APPLICATION_CBOR,
        }
    }

    /// Read a format back off a sample's declared [`Encoding`] — the first step
    /// of RFC 08 §7's precedence, and the one that was missing (#1148).
    ///
    /// Every producer in this tree stamps [`Format::encoding`] on every put,
    /// and **no consumer read it back**: they all sniffed the first byte, which
    /// is the step §7 puts last. `None` here means the sample declared nothing
    /// this crate recognises, which is the only case a sniff is for.
    pub fn from_encoding(encoding: &zenoh::bytes::Encoding) -> Option<Self> {
        if encoding == &zenoh::bytes::Encoding::APPLICATION_JSON {
            Some(Format::Json)
        } else if encoding == &zenoh::bytes::Encoding::APPLICATION_CBOR {
            Some(Format::Cbor)
        } else {
            None
        }
    }
}

impl Format {
    /// Get the MIME type for this format.
    pub fn mime_type(&self) -> &'static str {
        match self {
            Format::Json => "application/json",
            Format::Cbor => "application/cbor",
        }
    }
}

/// Encode a value to bytes using the specified format.
pub fn encode<T: Serialize>(value: &T, format: Format) -> Result<Vec<u8>> {
    match format {
        Format::Json => serde_json::to_vec(value).map_err(Error::from),
        Format::Cbor => {
            let mut buf = Vec::new();
            ciborium::into_writer(value, &mut buf)?;
            Ok(buf)
        }
    }
}

/// Decode bytes to a value using the specified format.
pub fn decode<T: DeserializeOwned>(data: &[u8], format: Format) -> Result<T> {
    match format {
        Format::Json => serde_json::from_slice(data).map_err(Error::from),
        Format::Cbor => ciborium::from_reader(data).map_err(|e| Error::Cbor(e.to_string())),
    }
}

/// Try to auto-detect the format from the first byte.
///
/// `None` means **the first byte does not decide**, which is not the same as
/// "then it must be CBOR" — the distinction this used to lose (#1148).
///
/// Only two shapes are unambiguous, and both are containers:
///
/// | first byte | reading |
/// |---|---|
/// | `{` (`0x7B`), `[` (`0x5B`) | JSON object / array |
/// | `0x80`–`0xBF` | CBOR array or map |
/// | `0xC0`–`0xDB` | CBOR tag |
/// | anything else | **undecidable** |
///
/// Everything this bus carries at the top level is a struct or a sequence, so
/// the table covers the wire. What it excludes is the quiet corruption: `0x7B`
/// is also CBOR major 3 / ai 27, a text string with an 8-byte length, and
/// `0x5B` is major 2 / ai 27 — those mis-sniff as JSON and fail loudly, which
/// is survivable. The reverse does not fail at all. JSON `42` is `0x34`, which
/// is a complete, valid CBOR negative integer, so
/// `decode_auto::<i64>(b"42")` used to return **−21** with no error anywhere.
pub fn detect_format(data: &[u8]) -> Option<Format> {
    match data.first() {
        Some(b'{') | Some(b'[') => Some(Format::Json),
        // CBOR major types 4 (array), 5 (map) and 6 (tag). Major 7 is left out
        // — `false`/`true`/`null`/floats are scalars, and a bare one of those
        // is exactly what this refuses to guess at.
        Some(0x80..=0xDB) => Some(Format::Cbor),
        _ => None,
    }
}

/// Decode bytes, auto-detecting the format.
///
/// **An undecidable first byte is an error, not a guess** (#1148). See
/// [`detect_format`] for what decides and what does not. A caller that knows
/// the format — from the sample's `Encoding`, or because it wrote the bytes —
/// should use [`decode_with_encoding`] or [`decode`] and not sniff at all.
pub fn decode_auto<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    let Some(format) = detect_format(data) else {
        return Err(Error::AmbiguousEncoding(format!(
            "cannot tell JSON from CBOR: the payload starts with {} and neither \
             is a container. A top-level scalar is ambiguous — JSON `42` is a \
             valid CBOR negative integer — so the encoding has to be declared, \
             not guessed (RFC 08 §7).",
            match data.first() {
                Some(b) => format!("0x{b:02x}"),
                None => "nothing (empty payload)".to_string(),
            }
        )));
    };
    decode(data, format)
}

/// Decode bytes using a sample's declared [`Encoding`], sniffing only when it
/// declared nothing this crate knows — RFC 08 §7's precedence, in order.
///
/// This is what a subscriber should call. `decode_auto` is the last step of
/// that precedence used on its own, which is what the whole tree was doing
/// while every producer stamped the answer on the sample (#1148).
pub fn decode_with_encoding<T: DeserializeOwned>(
    encoding: &zenoh::bytes::Encoding,
    data: &[u8],
) -> Result<T> {
    match Format::from_encoding(encoding) {
        Some(format) => decode(data, format),
        None => decode_auto(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{TelemetryPoint, TelemetryValue};

    #[test]
    fn test_json_roundtrip() {
        let point = TelemetryPoint::new(
            "router01",
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        );

        let encoded = encode(&point, Format::Json).unwrap();
        let decoded: TelemetryPoint = decode(&encoded, Format::Json).unwrap();

        assert_eq!(point.source, decoded.source);
        assert_eq!(point.metric, decoded.metric);
        assert_eq!(point.value, decoded.value);
    }

    #[test]
    fn test_cbor_roundtrip() {
        let point = TelemetryPoint::new(
            "router01",
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        );

        let encoded = encode(&point, Format::Cbor).unwrap();
        let decoded: TelemetryPoint = decode(&encoded, Format::Cbor).unwrap();

        assert_eq!(point.source, decoded.source);
        assert_eq!(point.metric, decoded.metric);
        assert_eq!(point.value, decoded.value);
    }

    #[test]
    fn test_cbor_is_smaller() {
        let point = TelemetryPoint::new(
            "router01",
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        );

        let json = encode(&point, Format::Json).unwrap();
        let cbor = encode(&point, Format::Cbor).unwrap();

        // CBOR should be meaningfully smaller, not just `<` by a byte — pin a ratio
        // floor so a regression that bloats CBOR can't slip through.
        assert!(
            (cbor.len() as f64) < 0.8 * (json.len() as f64),
            "CBOR ({} B) should be <80% of JSON ({} B)",
            cbor.len(),
            json.len()
        );
    }

    #[test]
    fn test_default_format_is_cbor() {
        assert_eq!(Format::default(), Format::Cbor);
    }

    #[test]
    fn test_format_detection() {
        assert_eq!(detect_format(b"{\"key\": \"value\"}"), Some(Format::Json));
        assert_eq!(detect_format(b"[1, 2, 3]"), Some(Format::Json));
        assert_eq!(detect_format(b"\xa1\x63key\x65value"), Some(Format::Cbor));
    }

    /// The bug (#1148): a JSON scalar is a *valid* CBOR value, so the old sniff
    /// did not fail — it returned a different number.
    #[test]
    fn a_json_scalar_is_not_quietly_read_as_a_cbor_negative_integer() {
        // `4` is 0x34: CBOR major type 1 (negative integer), additional info
        // 20, i.e. -21. Complete, well-formed, and wrong.
        assert_eq!(
            ciborium::from_reader::<i64, _>(&b"42"[..]).ok(),
            Some(-21),
            "the premise: this is why a guess is not survivable here"
        );
        assert_eq!(detect_format(b"42"), None, "the first byte does not decide");
        let err = decode_auto::<i64>(b"42").expect_err("a guess would return -21");
        assert!(
            matches!(err, Error::AmbiguousEncoding(_)),
            "must refuse, not fall through to CBOR: {err}"
        );
        assert!(err.to_string().contains("0x34"), "name the byte: {err}");
    }

    #[test]
    fn an_empty_payload_is_refused_rather_than_read_as_cbor() {
        let err = decode_auto::<i64>(b"").expect_err("nothing to decide on");
        assert!(err.to_string().contains("empty payload"), "{err}");
    }

    /// A top-level CBOR *string* mis-sniffs as JSON, which is the failure this
    /// does not fix — and does not need to, because it is loud.
    #[test]
    fn a_cbor_text_string_still_mis_sniffs_but_cannot_be_silent() {
        // 0x7B is `{` in ASCII and CBOR major 3 / ai 27 — a text string with an
        // 8-byte length. There is no byte that tells these apart, so the sniff
        // reads JSON and fails to parse. Wrong answer, but an answer that says
        // so, unlike the scalar case above.
        assert_eq!(
            detect_format(&[0x7b, 0, 0, 0, 0, 0, 0, 0, 1, b'x']),
            Some(Format::Json)
        );
        assert!(decode_auto::<String>(&[0x7b, 0, 0, 0, 0, 0, 0, 0, 1, b'x']).is_err());
    }

    #[test]
    fn a_declared_encoding_beats_the_sniff() {
        // RFC 08 §7's precedence, which every producer already stamped and no
        // consumer read (#1148).
        let point = TelemetryPoint::new("h", "m", TelemetryValue::Counter(1));
        for format in [Format::Json, Format::Cbor] {
            let bytes = encode(&point, format).unwrap();
            let back: TelemetryPoint =
                decode_with_encoding(&format.encoding(), &bytes).expect("declared encoding");
            assert_eq!(back.metric, point.metric, "{format:?}");
        }
        // And a scalar, which the sniff alone refuses, decodes when the sample
        // says what it is — the whole reason the declaration comes first.
        let cbor = encode(&42i64, Format::Cbor).unwrap();
        assert!(decode_auto::<i64>(&cbor).is_err(), "the sniff cannot tell");
        assert_eq!(
            decode_with_encoding::<i64>(&Format::Cbor.encoding(), &cbor).unwrap(),
            42
        );
        assert_eq!(
            decode_with_encoding::<i64>(&Format::Json.encoding(), b"42").unwrap(),
            42
        );
    }

    #[test]
    fn an_unrecognised_encoding_falls_back_to_the_sniff() {
        let point = TelemetryPoint::new("h", "m", TelemetryValue::Counter(1));
        let bytes = encode(&point, Format::Cbor).unwrap();
        let back: TelemetryPoint =
            decode_with_encoding(&zenoh::bytes::Encoding::APPLICATION_OCTET_STREAM, &bytes)
                .expect("a sample that declares nothing we know still sniffs");
        assert_eq!(back.metric, point.metric);
    }

    #[test]
    fn test_auto_decode() {
        let point = TelemetryPoint::new("router01", "test", TelemetryValue::Counter(42));

        // Test with JSON
        let json = encode(&point, Format::Json).unwrap();
        let decoded: TelemetryPoint = decode_auto(&json).unwrap();
        assert_eq!(point.source, decoded.source);

        // Test with CBOR
        let cbor = encode(&point, Format::Cbor).unwrap();
        let decoded: TelemetryPoint = decode_auto(&cbor).unwrap();
        assert_eq!(point.source, decoded.source);
    }
}
