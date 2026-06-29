//! Chunking policy.
//!
//! [`Chunker`] decides where chunk boundaries fall. Tier 1 uses
//! [`FixedSizeChunker`] (constant size → resume + identical-chunk dedup, and the
//! offset addressing the Tier-1 wire protocol needs). Tier 2 directory sync can
//! additionally use [`FastCdcChunker`] (content-defined boundaries → cross-version
//! dedup) behind the same trait via [`Chunker::split`], without touching the
//! transport.

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

    /// Split `data` into chunk boundaries `(offset, len)`.
    ///
    /// This is the primitive Tier-2 ([`crate::TreeIndex`] building) uses, so a
    /// content-defined chunker can cut at data-derived boundaries. The default is
    /// fixed-size slicing derived from [`offset`](Self::offset) /
    /// [`count`](Self::count) — so a fixed-size chunker needs no override and stays
    /// usable by the offset-addressed Tier-1 protocol too.
    fn split(&self, data: &[u8]) -> Vec<(usize, usize)> {
        let total = data.len() as u64;
        (0..self.count(total))
            .map(|i| (self.offset(i) as usize, self.chunk_len(i, total) as usize))
            .collect()
    }

    /// A short, self-describing tag for this chunking policy, recorded in the tree
    /// index (e.g. `"fixed-524288"`, `"fastcdc-262144"`).
    fn policy_tag(&self) -> String {
        format!("fixed-{}", self.chunk_size())
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

/// Content-defined chunker (FastCDC, #200). Cut points are derived from a rolling
/// gear-hash of the data, so inserting/removing bytes only re-chunks the
/// neighborhood of the edit — chunks before and after a change keep their hashes
/// and dedup across versions. **Tier-2 only**: the cut points depend on the data,
/// so the offset-addressed Tier-1 protocol (which the client must address by
/// `index * chunk_size` without seeing the bytes) cannot use it.
#[derive(Debug, Clone, Copy)]
pub struct FastCdcChunker {
    min: u32,
    avg: u32,
    max: u32,
}

impl FastCdcChunker {
    /// Build a FastCDC chunker around an average chunk size, with `min = avg/4`
    /// and `max = avg*4` (the conventional FastCDC spread). `avg` is floored at
    /// 256 bytes so `min` stays above FastCDC's hard floor of 64.
    pub fn new(avg: u32) -> Self {
        let avg = avg.max(256);
        FastCdcChunker {
            min: avg / 4,
            avg,
            max: avg.saturating_mul(4),
        }
    }
}

impl Chunker for FastCdcChunker {
    fn chunk_size(&self) -> u32 {
        self.avg
    }

    fn split(&self, data: &[u8]) -> Vec<(usize, usize)> {
        if data.is_empty() {
            return Vec::new();
        }
        fastcdc::v2020::FastCDC::new(
            data,
            self.min as usize,
            self.avg as usize,
            self.max as usize,
        )
        .map(|c| (c.offset, c.length))
        .collect()
    }

    fn policy_tag(&self) -> String {
        format!("fastcdc-{}", self.avg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{Digest, Sha256Digest};

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

    #[test]
    fn fixed_split_matches_offset_arithmetic() {
        let c = FixedSizeChunker::new(MIN_CHUNK_SIZE);
        let total = MIN_CHUNK_SIZE as usize * 3 + 1000;
        let data = vec![7u8; total];
        let cuts = c.split(&data);
        assert_eq!(cuts.len(), 4);
        assert_eq!(cuts[0], (0, MIN_CHUNK_SIZE as usize));
        assert_eq!(cuts[3], (MIN_CHUNK_SIZE as usize * 3, 1000));
        // Cuts tile the whole input with no gaps or overlaps.
        let covered: usize = cuts.iter().map(|(_, l)| l).sum();
        assert_eq!(covered, total);
    }

    #[test]
    fn policy_tags() {
        assert_eq!(
            FixedSizeChunker::new(DEFAULT_CHUNK_SIZE).policy_tag(),
            format!("fixed-{DEFAULT_CHUNK_SIZE}")
        );
        assert_eq!(FastCdcChunker::new(262_144).policy_tag(), "fastcdc-262144");
    }

    /// Deterministic pseudo-random bytes (xorshift64; no `rand` dep).
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

    fn sha(data: &[u8]) -> crate::hash::Hash {
        let mut d = Sha256Digest::default();
        d.update(data);
        d.finalize()
    }

    /// FastCDC chunk-hash set over `data`.
    fn cdc_hashes(
        chunker: &FastCdcChunker,
        data: &[u8],
    ) -> std::collections::HashSet<crate::hash::Hash> {
        chunker
            .split(data)
            .into_iter()
            .map(|(o, l)| sha(&data[o..o + l]))
            .collect()
    }

    /// Fixed N-byte tiling chunk-hash set (the `FixedSizeChunker` clamps to 256 KiB,
    /// too coarse for this small fixture, so tile directly for the comparison).
    fn fixed_hashes(data: &[u8], size: usize) -> std::collections::HashSet<crate::hash::Hash> {
        data.chunks(size).map(sha).collect()
    }

    /// Inserting a few bytes near the front of a file should leave most FastCDC
    /// chunks unchanged (content-defined boundaries re-sync), whereas fixed-size
    /// chunking shifts every following boundary and changes almost everything.
    #[test]
    fn fastcdc_localizes_edits_far_better_than_fixed() {
        let base = pseudo_random(200_000, 0xC0FFEE);
        // Insert 50 bytes after a 4 KiB prefix.
        let mut edited = base.clone();
        edited.splice(4096..4096, pseudo_random(50, 0xBEEF));

        let cdc = FastCdcChunker::new(8192);
        let cdc_a = cdc_hashes(&cdc, &base);
        let cdc_b = cdc_hashes(&cdc, &edited);
        let cdc_ratio = cdc_a.intersection(&cdc_b).count() as f64 / cdc_a.len() as f64;

        let fix_a = fixed_hashes(&base, 8192);
        let fix_b = fixed_hashes(&edited, 8192);
        let fix_ratio = fix_a.intersection(&fix_b).count() as f64 / fix_a.len() as f64;

        // FastCDC keeps the vast majority of chunks; fixed-size loses most.
        assert!(
            cdc_ratio > 0.8,
            "FastCDC should retain >80% of chunks, got {cdc_ratio:.2}"
        );
        assert!(
            cdc_ratio > fix_ratio + 0.3,
            "FastCDC ({cdc_ratio:.2}) should dedup far better than fixed ({fix_ratio:.2})"
        );
    }
}
