use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

/// Serialization format for telemetry data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// JSON format (human-readable, good for debugging).
    #[default]
    Json,

    /// CBOR format (compact binary, better for high-volume telemetry).
    Cbor,
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

/// Try to auto-detect the format from the data.
///
/// Returns `Json` if the data starts with `{` or `[`, otherwise `Cbor`.
pub fn detect_format(data: &[u8]) -> Format {
    match data.first() {
        Some(b'{') | Some(b'[') => Format::Json,
        _ => Format::Cbor,
    }
}

/// Decode bytes, auto-detecting the format.
pub fn decode_auto<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    let format = detect_format(data);
    decode(data, format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

    #[test]
    fn test_json_roundtrip() {
        let point = TelemetryPoint::new(
            "router01",
            Protocol::Snmp,
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        );

        let encoded = encode(&point, Format::Json).unwrap();
        let decoded: TelemetryPoint = decode(&encoded, Format::Json).unwrap();

        assert_eq!(point.source, decoded.source);
        assert_eq!(point.protocol, decoded.protocol);
        assert_eq!(point.metric, decoded.metric);
        assert_eq!(point.value, decoded.value);
    }

    #[test]
    fn test_cbor_roundtrip() {
        let point = TelemetryPoint::new(
            "router01",
            Protocol::Snmp,
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        );

        let encoded = encode(&point, Format::Cbor).unwrap();
        let decoded: TelemetryPoint = decode(&encoded, Format::Cbor).unwrap();

        assert_eq!(point.source, decoded.source);
        assert_eq!(point.protocol, decoded.protocol);
        assert_eq!(point.metric, decoded.metric);
        assert_eq!(point.value, decoded.value);
    }

    #[test]
    fn test_cbor_is_smaller() {
        let point = TelemetryPoint::new(
            "router01",
            Protocol::Snmp,
            "system/sysUpTime",
            TelemetryValue::Counter(123456),
        );

        let json = encode(&point, Format::Json).unwrap();
        let cbor = encode(&point, Format::Cbor).unwrap();

        assert!(cbor.len() < json.len(), "CBOR should be smaller than JSON");
    }

    #[test]
    fn test_format_detection() {
        assert_eq!(detect_format(b"{\"key\": \"value\"}"), Format::Json);
        assert_eq!(detect_format(b"[1, 2, 3]"), Format::Json);
        assert_eq!(detect_format(b"\xa1\x63key\x65value"), Format::Cbor);
    }

    #[test]
    fn test_auto_decode() {
        let point = TelemetryPoint::new(
            "router01",
            Protocol::Snmp,
            "test",
            TelemetryValue::Counter(42),
        );

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
