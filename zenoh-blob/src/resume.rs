//! On-disk resume state for an interrupted download.
//!
//! Progress is persisted next to the `.part` file as a small JSON sidecar so a
//! dropped connection *or* a process restart can continue instead of restarting.
//! The sidecar is bound to the blob's `id` + whole-blob `hash`, so a source that
//! was regenerated between attempts can never splice mismatched halves
//! (`docs/LARGE-DATA-TRANSFER.md` §5.8) — on any mismatch the partial is discarded
//! and the download starts fresh.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::hash::Hash;
use crate::manifest::Manifest;

/// Persistent record of which chunks of a blob are already on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResumeState {
    /// The blob id this partial belongs to.
    pub manifest_id: String,
    /// The whole-blob hash (resume binding — see module docs).
    pub hash: Hash,
    /// Chunk size used (must match the manifest to reuse the partial).
    pub chunk_size: u32,
    /// Total chunk count.
    pub chunk_count: u32,
    /// Per-chunk presence bitmap (`present[i]` = chunk `i` is fully written).
    pub present: Vec<bool>,
}

impl ResumeState {
    /// Path of the sidecar for a given `.part` file.
    pub fn sidecar_path(part: &Path) -> PathBuf {
        let mut p = part.as_os_str().to_os_string();
        p.push(".meta");
        PathBuf::from(p)
    }

    /// A fresh, all-missing state for `manifest`.
    pub fn fresh(manifest: &Manifest) -> Self {
        ResumeState {
            manifest_id: manifest.id.clone(),
            hash: manifest.hash,
            chunk_size: manifest.chunk_size,
            chunk_count: manifest.chunk_count,
            present: vec![false; manifest.chunk_count as usize],
        }
    }

    /// Whether this saved state can be reused to resume `manifest` (same id, hash,
    /// chunking, and a well-formed bitmap).
    pub fn matches(&self, manifest: &Manifest) -> bool {
        self.manifest_id == manifest.id
            && self.hash == manifest.hash
            && self.chunk_size == manifest.chunk_size
            && self.chunk_count == manifest.chunk_count
            && self.present.len() == manifest.chunk_count as usize
    }

    /// Number of chunks already present.
    pub fn received(&self) -> u32 {
        self.present.iter().filter(|p| **p).count() as u32
    }

    /// Index of the first missing chunk (the `?from=K` resume point), or
    /// `chunk_count` if complete.
    pub fn first_missing(&self) -> u32 {
        self.present
            .iter()
            .position(|p| !*p)
            .map(|i| i as u32)
            .unwrap_or(self.chunk_count)
    }

    /// Load the sidecar for `part`, if it exists and parses.
    pub async fn load(part: &Path) -> Option<ResumeState> {
        let bytes = tokio::fs::read(Self::sidecar_path(part)).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persist the sidecar next to `part`.
    pub async fn save(&self, part: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(crate::error::BlobError::encode)?;
        tokio::fs::write(Self::sidecar_path(part), bytes).await?;
        Ok(())
    }

    /// Remove the sidecar (best-effort).
    pub async fn remove(part: &Path) {
        let _ = tokio::fs::remove_file(Self::sidecar_path(part)).await;
    }
}
