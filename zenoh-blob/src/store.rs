//! Content-addressed chunk store (the Tier-2 substrate).
//!
//! Chunks are named by their content hash, so a store is the dedup + resume
//! substrate at once: "progress" is simply *which hashes are on disk*. The trait
//! is sync (local, fast ops); a remote chunk is fetched once and `put` here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::hash::Hash;

/// A local store of content-addressed chunks.
pub trait ContentStore: Send + Sync {
    /// Whether chunk `hash` is present.
    fn has(&self, hash: &Hash) -> bool;
    /// Fetch chunk `hash`, if present.
    fn get(&self, hash: &Hash) -> Option<Vec<u8>>;
    /// Store chunk `hash` → `bytes` (idempotent).
    fn put(&self, hash: &Hash, bytes: &[u8]) -> std::io::Result<()>;
}

/// An in-memory [`ContentStore`] (tests, ephemeral caches).
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<Hash, Vec<u8>>>);

impl MemoryStore {
    /// A new empty store.
    pub fn new() -> Self {
        MemoryStore(Mutex::new(HashMap::new()))
    }

    /// Number of stored chunks.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ContentStore for MemoryStore {
    fn has(&self, hash: &Hash) -> bool {
        self.0.lock().unwrap().contains_key(hash)
    }
    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(hash).cloned()
    }
    fn put(&self, hash: &Hash, bytes: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().insert(*hash, bytes.to_vec());
        Ok(())
    }
}

/// A filesystem [`ContentStore`]: one file per chunk, named by hex hash, under a
/// directory. Survives process restart, so it's the natural resume/dedup backing.
pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(DirStore { root })
    }

    fn path(&self, hash: &Hash) -> PathBuf {
        self.root.join(hash.to_string())
    }
}

impl ContentStore for DirStore {
    fn has(&self, hash: &Hash) -> bool {
        self.path(hash).exists()
    }
    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        std::fs::read(self.path(hash)).ok()
    }
    fn put(&self, hash: &Hash, bytes: &[u8]) -> std::io::Result<()> {
        let dst = self.path(hash);
        // Atomic: write a temp file then rename (content-addressed ⇒ idempotent).
        let tmp = self.root.join(format!("{hash}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &dst)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{Digest, Sha256Digest};

    fn h(data: &[u8]) -> Hash {
        let mut d = Sha256Digest::default();
        d.update(data);
        d.finalize()
    }

    #[test]
    fn memory_store_roundtrip() {
        let s = MemoryStore::new();
        let hash = h(b"hello");
        assert!(!s.has(&hash));
        s.put(&hash, b"hello").unwrap();
        assert!(s.has(&hash));
        assert_eq!(s.get(&hash).unwrap(), b"hello");
    }

    #[test]
    fn dir_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = DirStore::open(dir.path()).unwrap();
        let hash = h(b"world");
        assert!(!s.has(&hash));
        s.put(&hash, b"world").unwrap();
        assert!(s.has(&hash));
        assert_eq!(s.get(&hash).unwrap(), b"world");
        // Reopening sees the persisted chunk (restart-proof).
        let s2 = DirStore::open(dir.path()).unwrap();
        assert!(s2.has(&hash));
    }
}
