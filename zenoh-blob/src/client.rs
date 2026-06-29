//! The blob client: issues a download query, writes chunks to a partial file,
//! verifies the whole-blob hash, and resumes interrupted transfers.
//!
//! Resume state is a `.part` + a small JSON sidecar (see [`crate::resume`]); a
//! dropped connection or a process restart re-`download`s and continues from the
//! first missing chunk via the `?from=K` selector.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::error::{BlobError, Result};
use crate::format::{Format, decode};
use crate::hash::{Digest, Sha256Digest};
use crate::manifest::Manifest;
use crate::progress::{Progress, ProgressSink};
use crate::resume::ResumeState;
use crate::{chunk::Chunker, chunk::FixedSizeChunker, download_selector};

/// Downloads blobs served by a [`crate::BlobServer`] under the same key prefix.
pub struct BlobClient {
    session: Arc<zenoh::Session>,
    prefix: String,
    format: Format,
}

impl BlobClient {
    /// Build a client that downloads blobs under `key_prefix`, decoding the
    /// manifest with `format`.
    pub fn new(
        session: Arc<zenoh::Session>,
        key_prefix: impl Into<String>,
        format: Format,
    ) -> Self {
        BlobClient {
            session,
            prefix: key_prefix.into(),
            format,
        }
    }

    /// Download blob `id` into `dest_dir`, verifying its whole-blob hash, and
    /// return the path of the finished file. Progress events go to `sink`.
    pub async fn download(
        &self,
        id: &str,
        dest_dir: &Path,
        sink: &dyn ProgressSink,
    ) -> Result<PathBuf> {
        let result = self.download_inner(id, dest_dir, sink).await;
        if let Err(e) = &result {
            sink.emit(Progress::Failed {
                error: e.to_string(),
            });
        }
        result
    }

    async fn download_inner(
        &self,
        id: &str,
        dest_dir: &Path,
        sink: &dyn ProgressSink,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(dest_dir).await?;
        let part = dest_dir.join(format!("{id}.part"));

        // Phase 1 — fetch the manifest on its own GET. Zenoh does not order
        // replies, so the client must know `chunk_size` before any chunk arrives
        // (otherwise it cannot place out-of-order chunks by offset). This also
        // keeps memory bounded: chunks go straight to disk, never buffered.
        let manifest = self.fetch_manifest(id).await?;
        if manifest.hash_algo != Sha256Digest::name() {
            return Err(BlobError::Protocol(format!(
                "unsupported hash algo: {}",
                manifest.hash_algo
            )));
        }
        sink.emit(Progress::ManifestReceived {
            total_len: manifest.total_len,
            chunk_count: manifest.chunk_count,
        });

        let chunker = FixedSizeChunker::new(manifest.chunk_size);

        // Resume probe: reuse a matching partial, else start fresh. A sidecar that
        // doesn't match (different id/hash/chunking → a regenerated source) is
        // discarded rather than spliced.
        let mut state = match ResumeState::load(&part).await {
            Some(s) if s.matches(&manifest) && tokio::fs::try_exists(&part).await? => s,
            _ => {
                let file = tokio::fs::File::create(&part).await?;
                file.set_len(manifest.total_len).await?;
                let fresh = ResumeState::fresh(&manifest);
                fresh.save(&part).await?;
                fresh
            }
        };
        let mut received = state.received();
        let from = state.first_missing();

        // Open the partial for writing without truncating (we may be resuming).
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .await?;

        // Phase 2 — stream the missing chunks (any order); place each by offset.
        if from < manifest.chunk_count {
            let selector = download_selector(&self.prefix, id, from);
            let replies = self
                .session
                .get(&selector)
                .await
                .map_err(BlobError::zenoh)?;
            while let Ok(reply) = replies.recv_async().await {
                let Ok(sample) = reply.result() else { continue };
                let key = sample.key_expr().as_str();
                let Some(index) = parse_chunk_index(key) else {
                    continue; // the manifest reply also arrives here; ignore it.
                };
                if index >= manifest.chunk_count {
                    continue;
                }
                let bytes = sample.payload().to_bytes();
                let expected = chunker.chunk_len(index, manifest.total_len);
                if bytes.len() as u32 != expected {
                    return Err(BlobError::ChunkLen { index });
                }
                file.seek(SeekFrom::Start(chunker.offset(index))).await?;
                file.write_all(&bytes).await?;
                if let Some(slot) = state.present.get_mut(index as usize)
                    && !*slot
                {
                    *slot = true;
                    received += 1;
                    state.save(&part).await?;
                    sink.emit(Progress::Chunk {
                        index,
                        received,
                        total: manifest.chunk_count,
                    });
                }
            }
        }

        if received < manifest.chunk_count {
            // Persist progress so the next call resumes from where we stopped.
            file.flush().await?;
            state.save(&part).await?;
            return Err(BlobError::Incomplete {
                received,
                total: manifest.chunk_count,
            });
        }
        file.flush().await?;
        drop(file);

        // Verify the whole-blob hash by streaming the partial file once.
        sink.emit(Progress::Verifying);
        let actual = hash_file::<Sha256Digest>(&part).await?;
        if actual != manifest.hash {
            let _ = tokio::fs::remove_file(&part).await;
            ResumeState::remove(&part).await;
            return Err(BlobError::HashMismatch);
        }

        let final_path = dest_dir.join(&manifest.filename);
        tokio::fs::rename(&part, &final_path).await?;
        ResumeState::remove(&part).await;
        sink.emit(Progress::Completed {
            path: final_path.clone(),
        });
        Ok(final_path)
    }

    /// Fetch just the manifest for blob `id`.
    async fn fetch_manifest(&self, id: &str) -> Result<Manifest> {
        let key = crate::manifest_key(&self.prefix, id);
        let replies = self.session.get(&key).await.map_err(BlobError::zenoh)?;
        while let Ok(reply) = replies.recv_async().await {
            let Ok(sample) = reply.result() else { continue };
            if sample.key_expr().as_str().ends_with("/manifest") {
                return decode(&sample.payload().to_bytes(), self.format);
            }
        }
        Err(BlobError::NotFound(id.to_string()))
    }
}

/// Parse the chunk index from a `…/chunk/<index>` key, if present.
fn parse_chunk_index(key: &str) -> Option<u32> {
    let (head, idx) = key.rsplit_once('/')?;
    if head.ends_with("/chunk") {
        idx.parse().ok()
    } else {
        None
    }
}

/// Stream a file through a digest and return its hash, without loading it whole.
async fn hash_file<D: Digest>(path: &Path) -> Result<crate::hash::Hash> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut digest = D::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(digest.finalize())
}
