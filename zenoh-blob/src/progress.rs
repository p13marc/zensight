//! Download progress events.

use std::path::PathBuf;

/// A progress event emitted by [`crate::BlobClient::download`].
#[derive(Clone, Debug)]
pub enum Progress {
    /// The manifest reply arrived; the total size and chunk count are now known.
    ManifestReceived {
        /// Total blob length in bytes.
        total_len: u64,
        /// Total number of chunks.
        chunk_count: u32,
    },
    /// A chunk was written to the partial file.
    Chunk {
        /// Index of the chunk just written.
        index: u32,
        /// How many distinct chunks are present so far.
        received: u32,
        /// Total chunks expected.
        total: u32,
    },
    /// All chunks are present; verifying the whole-blob hash.
    Verifying,
    /// The download finished and verified; the file is at `path`.
    Completed {
        /// Final path of the assembled, verified blob.
        path: PathBuf,
    },
    /// The download failed.
    Failed {
        /// Human-readable reason.
        error: String,
    },
}

/// A sink for [`Progress`] events. Implemented for any `Fn(Progress)` and for
/// `()` (a no-op), so callers can pass a closure or nothing.
pub trait ProgressSink: Send + Sync {
    /// Receive one progress event.
    fn emit(&self, progress: Progress);
}

impl<F: Fn(Progress) + Send + Sync> ProgressSink for F {
    fn emit(&self, progress: Progress) {
        self(progress)
    }
}

impl ProgressSink for () {
    fn emit(&self, _progress: Progress) {}
}
