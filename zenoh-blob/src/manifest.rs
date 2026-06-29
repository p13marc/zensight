//! The blob manifest — the single source of truth for a transfer.
//!
//! One query returns a manifest reply followed by N chunk replies. Everything a
//! client needs to size, resume, and verify a download lives here (not in the
//! chunk keys), so there is no second metadata source that can disagree.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::chunk::Chunker;
use crate::error::Result;
use crate::hash::{Digest, Hash};

/// Describes a single blob: its identity, size, chunking, and whole-blob digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Opaque caller-chosen id (ZenSight passes a ULID string). Appears in the
    /// blob key, so it must be a single, non-empty key segment.
    pub id: String,
    /// Suggested file name for the saved blob.
    pub filename: String,
    /// Total blob length in bytes.
    pub total_len: u64,
    /// Chunk size used to split the blob.
    pub chunk_size: u32,
    /// Number of chunks (`ceil(total_len / chunk_size)`).
    pub chunk_count: u32,
    /// Hash algorithm name (e.g. `"sha256"`).
    pub hash_algo: String,
    /// Whole-blob digest, used for end-to-end integrity (R4) and resume binding.
    pub hash: Hash,
    /// Creation time, Unix epoch milliseconds (caller-supplied; the crate avoids
    /// reading the wall clock so it stays side-effect-free and testable).
    pub created_ms: i64,
}

impl Manifest {
    /// Compute a manifest by streaming `reader` exactly once — **never**
    /// `read_to_end`, so memory stays O(buffer) regardless of blob size.
    ///
    /// `created_ms` is supplied by the caller (the crate does not read the clock).
    pub async fn compute<R, D>(
        reader: &mut R,
        chunker: &dyn Chunker,
        id: impl Into<String>,
        filename: impl Into<String>,
        created_ms: i64,
    ) -> Result<Manifest>
    where
        R: AsyncRead + Unpin,
        D: Digest,
    {
        let mut digest = D::default();
        let mut total_len: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
            total_len += n as u64;
        }
        Ok(Manifest {
            id: id.into(),
            filename: filename.into(),
            total_len,
            chunk_size: chunker.chunk_size(),
            chunk_count: chunker.count(total_len),
            hash_algo: D::name().to_string(),
            hash: digest.finalize(),
            created_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{FixedSizeChunker, MIN_CHUNK_SIZE};
    use crate::hash::Sha256Digest;

    #[tokio::test]
    async fn compute_sizes_and_hashes() {
        let data = vec![7u8; MIN_CHUNK_SIZE as usize * 2 + 100];
        let mut cursor = std::io::Cursor::new(data.clone());
        let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
        let m = Manifest::compute::<_, Sha256Digest>(&mut cursor, &chunker, "abc", "f.bin", 42)
            .await
            .unwrap();

        assert_eq!(m.total_len, data.len() as u64);
        assert_eq!(m.chunk_count, 3); // two full + one short
        assert_eq!(m.chunk_size, MIN_CHUNK_SIZE);
        assert_eq!(m.hash_algo, "sha256");
        assert_eq!(m.created_ms, 42);

        // Hash matches a direct one-shot digest.
        let mut d = Sha256Digest::default();
        d.update(&data);
        assert_eq!(m.hash, d.finalize());
    }

    #[tokio::test]
    async fn empty_blob() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
        let m = Manifest::compute::<_, Sha256Digest>(&mut cursor, &chunker, "e", "empty", 0)
            .await
            .unwrap();
        assert_eq!(m.total_len, 0);
        assert_eq!(m.chunk_count, 0);
    }
}
