//! Content digests for blob integrity.
//!
//! [`Digest`] is the pluggable hashing boundary; [`Sha256Digest`] is the default
//! (R4). A future content-defined-chunking tier can swap in a different algorithm
//! without touching the protocol — the algorithm name travels in the manifest.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest as _;

/// A 32-byte content hash, rendered as lowercase hex on the wire and in keys.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

/// Error parsing a [`Hash`] from a hex string.
#[derive(Debug, thiserror::Error)]
#[error("invalid hex hash: {0}")]
pub struct HashParseError(String);

impl FromStr for Hash {
    type Err = HashParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(HashParseError(format!(
                "expected 64 hex chars, got {}",
                s.len()
            )));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            let hi = s.as_bytes()[i * 2];
            let lo = s.as_bytes()[i * 2 + 1];
            *byte = (hex_val(hi).ok_or_else(|| HashParseError(s.to_string()))? << 4)
                | hex_val(lo).ok_or_else(|| HashParseError(s.to_string()))?;
        }
        Ok(Hash(out))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A streaming digest: feed bytes with [`Digest::update`], finish with
/// [`Digest::finalize`]. `Default` constructs a fresh hasher.
pub trait Digest: Default {
    /// Absorb more input.
    fn update(&mut self, data: &[u8]);
    /// Consume the hasher and produce the final [`Hash`].
    fn finalize(self) -> Hash;
    /// The wire/key name of this algorithm (e.g. `"sha256"`).
    fn name() -> &'static str;
}

/// SHA-256, the default blob digest.
#[derive(Default)]
pub struct Sha256Digest(sha2::Sha256);

impl Digest for Sha256Digest {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> Hash {
        Hash(self.0.finalize().into())
    }

    fn name() -> &'static str {
        "sha256"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // SHA-256("abc")
        let mut d = Sha256Digest::default();
        d.update(b"abc");
        let h = d.finalize();
        assert_eq!(
            h.to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hash_hex_roundtrip() {
        let mut d = Sha256Digest::default();
        d.update(b"abc");
        let h = d.finalize();
        let parsed: Hash = h.to_string().parse().unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn hash_serde_is_hex_string() {
        let mut d = Sha256Digest::default();
        d.update(b"abc");
        let h = d.finalize();
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.starts_with('"') && json.contains("ba7816bf"));
        let back: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn bad_hex_rejected() {
        assert!("nothex".parse::<Hash>().is_err());
        assert!("zz".repeat(32).parse::<Hash>().is_err());
    }
}
