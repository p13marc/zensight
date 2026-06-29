//! Chunking policy.
//!
//! [`Chunker`] decides where chunk boundaries fall. Tier 1 uses
//! [`FixedSizeChunker`] (constant size → resume + identical-chunk dedup). A later
//! tier can add a content-defined chunker (FastCDC) behind the same trait without
//! touching the transport.

/// Smallest allowed chunk size (256 KiB) — keeps progress fine-grained.
pub const MIN_CHUNK_SIZE: u32 = 256 * 1024;
/// Largest allowed chunk size (1 MiB) — keeps per-chunk RAM bounded.
pub const MAX_CHUNK_SIZE: u32 = 1024 * 1024;
/// Default chunk size (512 KiB).
pub const DEFAULT_CHUNK_SIZE: u32 = 512 * 1024;

/// A validated chunk size, clamped to `[MIN_CHUNK_SIZE, MAX_CHUNK_SIZE]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSize(u32);

impl ChunkSize {
    /// Build a chunk size, clamping into the allowed range.
    pub fn new(bytes: u32) -> Self {
        ChunkSize(bytes.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE))
    }

    /// The size in bytes.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for ChunkSize {
    fn default() -> Self {
        ChunkSize(DEFAULT_CHUNK_SIZE)
    }
}

/// A chunk boundary policy. Tier 1 is fixed-size; the trait leaves room for
/// content-defined chunking later.
pub trait Chunker: Send + Sync {
    /// The (nominal) chunk size in bytes.
    fn chunk_size(&self) -> u32;

    /// Byte offset of chunk `index` (fixed-size: `index * chunk_size`).
    fn offset(&self, index: u32) -> u64 {
        index as u64 * self.chunk_size() as u64
    }

    /// Number of chunks needed for a blob of `total_len` bytes.
    fn count(&self, total_len: u64) -> u32 {
        if total_len == 0 {
            return 0;
        }
        let size = self.chunk_size() as u64;
        total_len.div_ceil(size) as u32
    }

    /// Length of chunk `index` for a blob of `total_len` bytes (the final chunk
    /// may be short).
    fn chunk_len(&self, index: u32, total_len: u64) -> u32 {
        let start = self.offset(index);
        if start >= total_len {
            return 0;
        }
        let remaining = total_len - start;
        remaining.min(self.chunk_size() as u64) as u32
    }
}

/// Constant-size chunker (Tier 1 default).
#[derive(Debug, Clone, Copy, Default)]
pub struct FixedSizeChunker {
    size: ChunkSize,
}

impl FixedSizeChunker {
    /// Build a fixed-size chunker; `bytes` is clamped to the allowed range.
    pub fn new(bytes: u32) -> Self {
        FixedSizeChunker {
            size: ChunkSize::new(bytes),
        }
    }
}

impl Chunker for FixedSizeChunker {
    fn chunk_size(&self) -> u32 {
        self.size.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_clamps() {
        assert_eq!(ChunkSize::new(1).get(), MIN_CHUNK_SIZE);
        assert_eq!(ChunkSize::new(u32::MAX).get(), MAX_CHUNK_SIZE);
        assert_eq!(ChunkSize::new(DEFAULT_CHUNK_SIZE).get(), DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn count_and_lengths() {
        let c = FixedSizeChunker::new(MIN_CHUNK_SIZE); // 256 KiB
        let size = MIN_CHUNK_SIZE as u64;

        assert_eq!(c.count(0), 0);
        assert_eq!(c.count(1), 1);
        assert_eq!(c.count(size), 1);
        assert_eq!(c.count(size + 1), 2);

        // 3.5 chunks worth → 4 chunks, last one half-size.
        let total = size * 3 + size / 2;
        assert_eq!(c.count(total), 4);
        assert_eq!(c.chunk_len(0, total), MIN_CHUNK_SIZE);
        assert_eq!(c.chunk_len(2, total), MIN_CHUNK_SIZE);
        assert_eq!(c.chunk_len(3, total) as u64, size / 2);
        assert_eq!(c.chunk_len(4, total), 0); // past the end
    }

    #[test]
    fn offsets() {
        let c = FixedSizeChunker::new(MAX_CHUNK_SIZE);
        assert_eq!(c.offset(0), 0);
        assert_eq!(c.offset(3), 3 * MAX_CHUNK_SIZE as u64);
    }
}
