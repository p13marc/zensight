//! Shared test helpers: one isolated, scouting-off session per test (the repo's
//! in-process loopback pattern), deterministic pseudo-random data, and small
//! byte sources.
#![allow(dead_code)] // each test binary uses a different subset of these.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use zenoh_blob::{AsyncReadSeek, BlobSource, Digest, Hash, OpenFuture, Sha256Digest};

pub fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}

pub async fn open_session() -> Arc<zenoh::Session> {
    Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"))
}

pub fn unique_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("blobtest/{nanos}")
}

/// Deterministic pseudo-random bytes (xorshift64, no rand dependency).
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed | 1;
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x & 0xff) as u8);
    }
    out
}

pub fn sha256(bytes: &[u8]) -> Hash {
    let mut d = Sha256Digest::default();
    d.update(bytes);
    d.finalize()
}

/// A [`BlobSource`] over an in-memory `Vec<u8>` (no temp file needed).
pub struct BytesSource(pub Arc<Vec<u8>>);

impl BlobSource for BytesSource {
    fn open(&self) -> OpenFuture {
        let data = self.0.clone();
        Box::pin(async move {
            let cursor = std::io::Cursor::new(BytesOwned(data));
            Ok(Box::new(cursor) as Box<dyn AsyncReadSeek>)
        })
    }
}

/// Owns an `Arc<Vec<u8>>` and exposes it as `AsRef<[u8]>` so a `Cursor` over it is
/// `AsyncRead + AsyncSeek`.
pub struct BytesOwned(pub Arc<Vec<u8>>);

impl AsRef<[u8]> for BytesOwned {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
