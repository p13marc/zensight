//! Tier-2 — content-addressed **directory** transfer (the casync-over-Zenoh
//! model). A snapshot is a [`TreeIndex`] (a depth-first list of entries; files
//! reference their chunks by content hash) plus a content-addressed chunk store.
//!
//! The client GETs the index, computes the set of chunk hashes it is **missing**
//! (`needed − have`), fetches only those (re-hashing each on receipt), and
//! reconstructs the tree. Because progress *is* "which hashes are on disk", a
//! dropped connection or a process restart loses nothing, and identical chunks
//! across files/versions transfer once.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::chunk::Chunker;
use crate::error::{BlobError, Result};
use crate::format::{Format, decode, encode};
use crate::hash::{Digest, Hash, Sha256Digest};
use crate::progress::{Progress, ProgressSink};
use crate::store::ContentStore;
use crate::{store_key, tree_key};

/// A reference to one content-addressed chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    /// Content hash of the chunk.
    pub hash: Hash,
    /// Length of the chunk in bytes.
    pub len: u32,
}

/// One entry in a tree snapshot (paths are relative, `/`-separated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// A directory.
    Dir {
        /// Relative path.
        path: String,
        /// Unix mode bits (0 on non-unix).
        mode: u32,
        /// Modification time, Unix seconds (informational).
        mtime: i64,
    },
    /// A regular file, reconstructed by concatenating `chunks` in order.
    File {
        /// Relative path.
        path: String,
        /// Unix mode bits (0 on non-unix).
        mode: u32,
        /// Modification time, Unix seconds (informational).
        mtime: i64,
        /// Total file size in bytes.
        size: u64,
        /// Ordered chunk references.
        chunks: Vec<ChunkRef>,
    },
    /// A symbolic link.
    Symlink {
        /// Relative path.
        path: String,
        /// Link target.
        target: String,
    },
}

/// A directory snapshot: an ordered entry list + a digest over it (`root_hash`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeIndex {
    /// Snapshot id (a single key segment).
    pub id: String,
    /// Hash algorithm name (e.g. `"sha256"`).
    pub algo: String,
    /// Chunk size used to split files (nominal/average — see `chunk_policy`).
    pub chunk_size: u32,
    /// Self-describing chunking policy tag (e.g. `"fixed-524288"`,
    /// `"fastcdc-262144"`). Informational: a Tier-2 client fetches chunks by hash
    /// and never re-chunks, so it needn't share the producer's policy.
    pub chunk_policy: String,
    /// Depth-first entries.
    pub entries: Vec<Entry>,
    /// Digest over the canonical serialization of `entries` (integrity root).
    pub root_hash: Hash,
}

impl TreeIndex {
    /// All distinct chunk hashes referenced by file entries.
    pub fn needed_chunks(&self) -> Vec<Hash> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for e in &self.entries {
            if let Entry::File { chunks, .. } = e {
                for c in chunks {
                    if seen.insert(c.hash) {
                        out.push(c.hash);
                    }
                }
            }
        }
        out
    }

    /// Total size in bytes of all file entries (the reconstructed tree's payload).
    pub fn total_size(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| match e {
                Entry::File { size, .. } => *size,
                _ => 0,
            })
            .sum()
    }

    /// Number of file entries in the snapshot.
    pub fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::File { .. }))
            .count()
    }

    /// Recompute the root digest over `entries` and compare to `root_hash`.
    fn verify_root<D: Digest>(&self) -> bool {
        root_digest::<D>(&self.entries) == self.root_hash
    }
}

/// Digest over the canonical serialization of an entry list (the tree root hash).
fn root_digest<D: Digest>(entries: &[Entry]) -> Hash {
    let mut d = D::default();
    // serde_json is deterministic for these types (no maps), giving a stable
    // canonical form to hash.
    let bytes = serde_json::to_vec(entries).unwrap_or_default();
    d.update(&bytes);
    d.finalize()
}

/// A built snapshot: its index plus the deduplicated `(hash, bytes)` chunks to
/// populate a [`ContentStore`].
pub type BuiltTree = (TreeIndex, Vec<(Hash, Vec<u8>)>);

/// Build a [`TreeIndex`] + the unique chunks for the directory at `root`.
///
/// Synchronous (run it in `spawn_blocking`). Walks depth-first in a deterministic
/// (sorted) order so the snapshot is reproducible. Returns the index and the
/// deduplicated `(hash, bytes)` chunks to populate a store.
pub fn build_tree(
    root: &Path,
    id: impl Into<String>,
    chunker: &dyn Chunker,
) -> std::io::Result<BuiltTree> {
    let mut entries = Vec::new();
    let mut chunks: Vec<(Hash, Vec<u8>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(root, root, chunker, &mut entries, &mut chunks, &mut seen)?;
    let root_hash = root_digest::<Sha256Digest>(&entries);
    Ok((
        TreeIndex {
            id: id.into(),
            algo: Sha256Digest::name().to_string(),
            chunk_size: chunker.chunk_size(),
            chunk_policy: chunker.policy_tag(),
            entries,
            root_hash,
        },
        chunks,
    ))
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}
#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn walk(
    root: &Path,
    dir: &Path,
    chunker: &dyn Chunker,
    entries: &mut Vec<Entry>,
    chunks: &mut Vec<(Hash, Vec<u8>)>,
    seen: &mut std::collections::HashSet<Hash>,
) -> std::io::Result<()> {
    let mut items: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    items.sort_by_key(|e| e.file_name());
    for item in items {
        let path = item.path();
        let meta = std::fs::symlink_metadata(&path)?;
        let rel = rel_path(root, &path);
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?.to_string_lossy().to_string();
            entries.push(Entry::Symlink { path: rel, target });
        } else if meta.is_dir() {
            entries.push(Entry::Dir {
                path: rel,
                mode: mode_of(&meta),
                mtime: mtime_of(&meta),
            });
            walk(root, &path, chunker, entries, chunks, seen)?;
        } else if meta.is_file() {
            let data = std::fs::read(&path)?;
            let cuts = chunker.split(&data);
            let mut refs = Vec::with_capacity(cuts.len());
            for (start, len) in cuts {
                let slice = &data[start..start + len];
                let mut d = Sha256Digest::default();
                d.update(slice);
                let hash = d.finalize();
                refs.push(ChunkRef {
                    hash,
                    len: len as u32,
                });
                if seen.insert(hash) {
                    chunks.push((hash, slice.to_vec()));
                }
            }
            entries.push(Entry::File {
                path: rel,
                mode: mode_of(&meta),
                mtime: mtime_of(&meta),
                size: data.len() as u64,
                chunks: refs,
            });
        }
    }
    Ok(())
}

/// Serves a tree snapshot: an index queryable + a content-addressed chunk
/// queryable, both backed by a [`ContentStore`].
#[derive(Clone)]
pub struct TreeServer {
    session: Arc<zenoh::Session>,
    store_prefix: String,
    tree_prefix: String,
    format: Format,
    store: Arc<dyn ContentStore>,
    index: Arc<tokio::sync::RwLock<std::collections::HashMap<String, TreeIndex>>>,
}

impl TreeServer {
    /// Build a server. `store_prefix` serves `<prefix>/<algo>/<hash>`;
    /// `tree_prefix` serves `<prefix>/<id>`.
    pub fn new(
        session: Arc<zenoh::Session>,
        store_prefix: impl Into<String>,
        tree_prefix: impl Into<String>,
        format: Format,
        store: Arc<dyn ContentStore>,
    ) -> Self {
        TreeServer {
            session,
            store_prefix: store_prefix.into(),
            tree_prefix: tree_prefix.into(),
            format,
            store,
            index: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Register a snapshot index (its chunks must already be in the store).
    pub async fn register(&self, index: TreeIndex) {
        self.index.write().await.insert(index.id.clone(), index);
    }

    /// Drop a previously-registered snapshot index by id (e.g. on TTL expiry). The
    /// chunks themselves are owned by the [`ContentStore`] and freed separately.
    pub async fn unregister(&self, id: &str) {
        self.index.write().await.remove(id);
    }

    /// Declare both queryables and serve until the session closes.
    pub async fn run(self) -> Result<()> {
        let store_q = self
            .session
            .declare_queryable(format!("{}/**", self.store_prefix))
            .await
            .map_err(BlobError::zenoh)?;
        let tree_q = self
            .session
            .declare_queryable(format!("{}/**", self.tree_prefix))
            .await
            .map_err(BlobError::zenoh)?;
        loop {
            tokio::select! {
                Ok(query) = store_q.recv_async() => {
                    let key = query.key_expr().as_str();
                    if let Some(hash) = parse_store_key(&self.store_prefix, key)
                        && let Some(bytes) = self.store.get(&hash)
                    {
                        let _ = query.reply(query.key_expr().clone(), bytes).await;
                    }
                }
                Ok(query) = tree_q.recv_async() => {
                    let key = query.key_expr().as_str().to_string();
                    if let Some(id) = key.strip_prefix(&format!("{}/", self.tree_prefix))
                        && let Some(index) = self.index.read().await.get(id).cloned()
                        && let Ok(payload) = encode(&index, self.format)
                    {
                        let _ = query.reply(query.key_expr().clone(), payload).await;
                    }
                }
                else => break,
            }
        }
        Ok(())
    }
}

/// Parse the chunk hash from a `<store_prefix>/<algo>/<hex>` key.
fn parse_store_key(store_prefix: &str, key: &str) -> Option<Hash> {
    let rest = key.strip_prefix(store_prefix)?.strip_prefix('/')?;
    let (_algo, hex) = rest.split_once('/')?;
    hex.parse().ok()
}

/// Downloads a tree snapshot. Stateless server; persistent client (the store).
pub struct TreeClient {
    session: Arc<zenoh::Session>,
    store_prefix: String,
    tree_prefix: String,
    format: Format,
}

impl TreeClient {
    /// Build a client matching a [`TreeServer`]'s prefixes.
    pub fn new(
        session: Arc<zenoh::Session>,
        store_prefix: impl Into<String>,
        tree_prefix: impl Into<String>,
        format: Format,
    ) -> Self {
        TreeClient {
            session,
            store_prefix: store_prefix.into(),
            tree_prefix: tree_prefix.into(),
            format,
        }
    }

    /// Fetch the snapshot index `id`.
    pub async fn fetch_index(&self, id: &str) -> Result<TreeIndex> {
        let key = tree_key(&self.tree_prefix, id);
        let replies = self.session.get(&key).await.map_err(BlobError::zenoh)?;
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                return decode(&sample.payload().to_bytes(), self.format);
            }
        }
        Err(BlobError::NotFound(id.to_string()))
    }

    /// Download snapshot `id` into `dest_root`: fetch only the chunks missing from
    /// `store` (re-hashing each), reconstruct the tree, and verify the root hash.
    ///
    /// A thin wrapper over [`download_tree_cancellable`](Self::download_tree_cancellable)
    /// with no progress reporting and an un-cancellable token.
    pub async fn download_tree(
        &self,
        id: &str,
        dest_root: &Path,
        store: &dyn ContentStore,
    ) -> Result<()> {
        self.download_tree_cancellable(id, dest_root, store, &(), &CancelToken::new())
            .await
    }

    /// Like [`download_tree`](Self::download_tree) but reports [`Progress`] per
    /// chunk and stops early (returning [`BlobError::Cancelled`]) when `cancel`
    /// is signalled. Because progress *is* "which hashes are on disk", a cancelled
    /// transfer leaves `store` populated with whatever it fetched, so calling again
    /// resumes — across reconnect and process restart — for free.
    pub async fn download_tree_cancellable(
        &self,
        id: &str,
        dest_root: &Path,
        store: &dyn ContentStore,
        sink: &dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()> {
        let index = self.fetch_index(id).await?;
        if index.algo != Sha256Digest::name() {
            return Err(BlobError::Protocol(format!(
                "unsupported algo: {}",
                index.algo
            )));
        }

        let needed = index.needed_chunks();
        let total = needed.len() as u32;
        sink.emit(Progress::ManifestReceived {
            total_len: index.total_size(),
            chunk_count: total,
        });

        // Fetch the missing chunks (progress = which hashes are on disk). `received`
        // counts hashes *resolved*, including ones already present, so a resume
        // reports its real starting point immediately.
        let mut received: u32 = 0;
        for hash in needed {
            if cancel.is_cancelled() {
                return Err(BlobError::Cancelled { received, total });
            }
            if !store.has(&hash) {
                let bytes = self.fetch_chunk(&hash).await?;
                // Verify by re-hashing on receipt → corruption is impossible.
                let mut d = Sha256Digest::default();
                d.update(&bytes);
                if d.finalize() != hash {
                    return Err(BlobError::HashMismatch);
                }
                store.put(&hash, &bytes)?;
            }
            received += 1;
            sink.emit(Progress::Chunk {
                index: received - 1,
                received,
                total,
            });
        }

        // Reconstruct the tree.
        sink.emit(Progress::Verifying);
        tokio::fs::create_dir_all(dest_root).await?;
        for entry in &index.entries {
            reconstruct(dest_root, entry, store).await?;
        }

        // Verify the Merkle-y root over the entry list.
        if !index.verify_root::<Sha256Digest>() {
            return Err(BlobError::HashMismatch);
        }
        sink.emit(Progress::Completed {
            path: dest_root.to_path_buf(),
        });
        Ok(())
    }

    async fn fetch_chunk(&self, hash: &Hash) -> Result<Vec<u8>> {
        let key = store_key(&self.store_prefix, Sha256Digest::name(), hash);
        let replies = self.session.get(&key).await.map_err(BlobError::zenoh)?;
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                return Ok(sample.payload().to_bytes().to_vec());
            }
        }
        Err(BlobError::NotFound(hash.to_string()))
    }
}

async fn reconstruct(dest_root: &Path, entry: &Entry, store: &dyn ContentStore) -> Result<()> {
    match entry {
        Entry::Dir { path, mode, .. } => {
            let p = dest_root.join(path);
            tokio::fs::create_dir_all(&p).await?;
            set_mode(&p, *mode).await;
        }
        Entry::File {
            path, chunks, mode, ..
        } => {
            let p = dest_root.join(path);
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::File::create(&p).await?;
            for c in chunks {
                let bytes = store
                    .get(&c.hash)
                    .ok_or_else(|| BlobError::NotFound(c.hash.to_string()))?;
                f.write_all(&bytes).await?;
            }
            f.flush().await?;
            set_mode(&p, *mode).await;
        }
        Entry::Symlink { path, target } => {
            let p = dest_root.join(path);
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let _ = tokio::fs::remove_file(&p).await;
            symlink(target, &p).await?;
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) {
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await;
    }
}
#[cfg(not(unix))]
async fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(unix)]
async fn symlink(target: &str, path: &Path) -> std::io::Result<()> {
    tokio::fs::symlink(target, path).await
}
#[cfg(not(unix))]
async fn symlink(_target: &str, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{FixedSizeChunker, MIN_CHUNK_SIZE};

    #[test]
    fn build_dedups_identical_chunks() {
        let dir = tempfile::tempdir().unwrap();
        // Two files with identical content → one shared chunk.
        std::fs::write(dir.path().join("a.txt"), b"same").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"same").unwrap();
        let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
        let (index, chunks) = build_tree(dir.path(), "snap", &chunker).unwrap();
        // One unique chunk despite two files.
        assert_eq!(chunks.len(), 1);
        assert_eq!(index.needed_chunks().len(), 1);
        assert!(index.verify_root::<Sha256Digest>());
        // Two file entries.
        let files = index
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::File { .. }))
            .count();
        assert_eq!(files, 2);
    }
}
