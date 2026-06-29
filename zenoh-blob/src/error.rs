//! Error type for blob transfer.

/// Errors raised by the blob server and client.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// A Zenoh operation failed. `zenoh::Error` is a boxed `dyn Error`, so it is
    /// flattened to a string here.
    #[error("zenoh: {0}")]
    Zenoh(String),

    /// A local I/O operation failed (reading the source, writing the `.part`).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Serializing or deserializing a control message (e.g. the manifest) failed.
    #[error("encode: {0}")]
    Encode(String),

    /// The fully-downloaded blob's hash did not match the manifest (R4).
    #[error("integrity: blob hash mismatch")]
    HashMismatch,

    /// A chunk reply had an unexpected length for its index.
    #[error("integrity: chunk {index} length mismatch")]
    ChunkLen {
        /// The offending chunk index.
        index: u32,
    },

    /// A resume was attempted but the source's manifest id/hash changed since the
    /// partial download — splicing mismatched halves would corrupt the output
    /// (see `docs/LARGE-DATA-TRANSFER.md` §5.8). The partial is discarded.
    #[error("resume: manifest id/hash changed since partial download")]
    ResumeMismatch,

    /// The query returned chunk replies but no manifest reply.
    #[error("protocol: manifest reply missing")]
    NoManifest,

    /// The server has no blob registered under the requested id (TTL expired or
    /// never existed).
    #[error("not found: {0}")]
    NotFound(String),

    /// The download ended (link dropped / server stopped) before all chunks
    /// arrived. State is persisted; call `download` again to resume.
    #[error("incomplete: {received}/{total} chunks received")]
    Incomplete {
        /// Chunks received so far.
        received: u32,
        /// Total chunks expected.
        total: u32,
    },

    /// A generic protocol violation (malformed key, bad selector, …).
    #[error("protocol: {0}")]
    Protocol(String),
}

/// Convenience alias for fallible blob operations.
pub type Result<T> = std::result::Result<T, BlobError>;

impl BlobError {
    /// Map a `zenoh::Error` (a boxed `dyn Error + Send + Sync`) into [`BlobError::Zenoh`].
    pub(crate) fn zenoh(e: impl std::fmt::Display) -> Self {
        BlobError::Zenoh(e.to_string())
    }

    /// Map a serde error into [`BlobError::Encode`].
    pub(crate) fn encode(e: impl std::fmt::Display) -> Self {
        BlobError::Encode(e.to_string())
    }
}
