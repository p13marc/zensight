//! Wire serialization for blob control messages (the manifest).
//!
//! Self-contained on purpose: `zenoh-blob` carries no ZenSight dependency, so it
//! keeps its own copy of the JSON/CBOR `Format` helper rather than reusing
//! `zensight-common::serialization`. Chunk payloads are raw bytes and never pass
//! through here — only the manifest does.

use serde::{Serialize, de::DeserializeOwned};

use crate::error::{BlobError, Result};

/// Serialization format for control messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// JSON (human-readable; easy to debug on the wire).
    #[default]
    Json,
    /// CBOR (compact binary).
    Cbor,
}

/// Encode a value to bytes using the given format.
pub fn encode<T: Serialize>(value: &T, format: Format) -> Result<Vec<u8>> {
    match format {
        Format::Json => serde_json::to_vec(value).map_err(BlobError::encode),
        Format::Cbor => {
            let mut buf = Vec::new();
            ciborium::into_writer(value, &mut buf).map_err(BlobError::encode)?;
            Ok(buf)
        }
    }
}

/// Decode bytes to a value using the given format.
pub fn decode<T: DeserializeOwned>(data: &[u8], format: Format) -> Result<T> {
    match format {
        Format::Json => serde_json::from_slice(data).map_err(BlobError::encode),
        Format::Cbor => ciborium::from_reader(data).map_err(BlobError::encode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn json_roundtrip() {
        let s = Sample {
            a: 7,
            b: "hi".into(),
        };
        let bytes = encode(&s, Format::Json).unwrap();
        assert_eq!(decode::<Sample>(&bytes, Format::Json).unwrap(), s);
    }

    #[test]
    fn cbor_roundtrip() {
        let s = Sample {
            a: 7,
            b: "hi".into(),
        };
        let bytes = encode(&s, Format::Cbor).unwrap();
        assert_eq!(decode::<Sample>(&bytes, Format::Cbor).unwrap(), s);
    }
}
