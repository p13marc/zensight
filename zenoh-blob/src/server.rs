//! The blob server: a queryable that streams a manifest + chunks for any
//! registered blob, lazily and with backpressure.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, SeekFrom};
use tokio::sync::{RwLock, Semaphore};

use crate::chunk::{Chunker, FixedSizeChunker};
use crate::error::{BlobError, Result};
use crate::format::{Format, encode};
use crate::manifest::Manifest;
use crate::{chunk_key, manifest_key, parse_from, parse_id};

/// Max concurrent in-flight transfers a single server will stream at once. This
/// is a coarse anti-DoS backstop; real authorization is the caller's job.
const MAX_INFLIGHT: usize = 8;

/// A readable+seekable byte source. Blanket-implemented for any type that is
/// `AsyncRead + AsyncSeek + Unpin + Send`.
pub trait AsyncReadSeek: AsyncRead + AsyncSeek + Unpin + Send {}
impl<T: AsyncRead + AsyncSeek + Unpin + Send> AsyncReadSeek for T {}

/// The future returned by [`BlobSource::open`].
pub type OpenFuture = Pin<Box<dyn Future<Output = std::io::Result<Box<dyn AsyncReadSeek>>> + Send>>;

/// Opens a fresh reader over a blob's bytes. Called once per query so each
/// transfer can seek independently; the source can be reopened many times.
pub trait BlobSource: Send + Sync {
    /// Open a new reader positioned at the start of the blob.
    fn open(&self) -> OpenFuture;
}

/// A [`BlobSource`] backed by a file on disk (e.g. a TTL'd report bundle).
pub struct FileBlobSource {
    path: PathBuf,
}

impl FileBlobSource {
    /// Serve the file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileBlobSource { path: path.into() }
    }
}

impl BlobSource for FileBlobSource {
    fn open(&self) -> OpenFuture {
        let path = self.path.clone();
        Box::pin(async move {
            let file = tokio::fs::File::open(&path).await?;
            Ok(Box::new(file) as Box<dyn AsyncReadSeek>)
        })
    }
}

struct Registered {
    manifest: Manifest,
    source: Arc<dyn BlobSource>,
}

struct Inner {
    session: Arc<zenoh::Session>,
    prefix: String,
    format: Format,
    registry: RwLock<HashMap<String, Registered>>,
    inflight: Semaphore,
}

/// Serves registered blobs over a Zenoh queryable at `<prefix>/**`.
#[derive(Clone)]
pub struct BlobServer {
    inner: Arc<Inner>,
}

impl BlobServer {
    /// Build a server that will serve blobs under `key_prefix`, encoding the
    /// manifest with `format`.
    pub fn new(
        session: Arc<zenoh::Session>,
        key_prefix: impl Into<String>,
        format: Format,
    ) -> Self {
        BlobServer {
            inner: Arc::new(Inner {
                session,
                prefix: key_prefix.into(),
                format,
                registry: RwLock::new(HashMap::new()),
                inflight: Semaphore::new(MAX_INFLIGHT),
            }),
        }
    }

    /// Register a blob to be served under `manifest.id`.
    pub async fn register(&self, manifest: Manifest, source: Arc<dyn BlobSource>) {
        let id = manifest.id.clone();
        self.inner
            .registry
            .write()
            .await
            .insert(id, Registered { manifest, source });
    }

    /// Stop serving blob `id` (e.g. after its TTL expires).
    pub async fn unregister(&self, id: &str) {
        self.inner.registry.write().await.remove(id);
    }

    /// Declare the queryable and serve until the session closes. Each query is
    /// served on its own task so a slow client cannot block others.
    pub async fn run(self) -> Result<()> {
        let key = format!("{}/**", self.inner.prefix);
        let queryable = self
            .inner
            .session
            .declare_queryable(&key)
            .await
            .map_err(BlobError::zenoh)?;
        while let Ok(query) = queryable.recv_async().await {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_one(&inner, query).await {
                    tracing_error(&e);
                }
            });
        }
        Ok(())
    }
}

fn tracing_error(e: &BlobError) {
    // The crate has no logging dependency; surface serve errors on stderr in debug
    // builds without pulling `tracing` into a would-be-published library.
    #[cfg(debug_assertions)]
    eprintln!("zenoh-blob: serve error: {e}");
    let _ = e;
}

async fn serve_one(inner: &Inner, query: zenoh::query::Query) -> Result<()> {
    let _permit = inner
        .inflight
        .acquire()
        .await
        .map_err(|e| BlobError::Protocol(e.to_string()))?;

    let key_str = query.key_expr().as_str().to_string();
    let Some(id) = parse_id(&inner.prefix, &key_str) else {
        return Ok(()); // not a per-blob query; ignore.
    };
    let from = parse_from(query.parameters().as_str());

    // Snapshot the registration (clone the cheap manifest + Arc the source) so we
    // don't hold the registry lock across the stream.
    let (manifest, source) = {
        let reg = inner.registry.read().await;
        match reg.get(&id) {
            Some(r) => (r.manifest.clone(), r.source.clone()),
            None => return Ok(()), // unknown id → client times out → NotFound.
        }
    };

    // Manifest reply (always sent first).
    let payload = encode(&manifest, inner.format)?;
    query
        .reply(manifest_key(&inner.prefix, &id), payload)
        .await
        .map_err(BlobError::zenoh)?;

    // A manifest-only request (exact `.../manifest` GET) stops here. Clients fetch
    // the manifest first so they know `chunk_size` before any chunk arrives —
    // Zenoh does not order replies, so chunks of the streaming GET below can
    // arrive before the manifest would have.
    if key_str.ends_with("/manifest") {
        return Ok(());
    }

    // Chunk replies, streamed lazily from a freshly-opened reader.
    let chunker = FixedSizeChunker::new(manifest.chunk_size);
    let mut reader = source.open().await?;
    if from > 0 {
        reader.seek(SeekFrom::Start(chunker.offset(from))).await?;
    }
    let mut buf = vec![0u8; manifest.chunk_size as usize];
    for index in from..manifest.chunk_count {
        let len = chunker.chunk_len(index, manifest.total_len) as usize;
        reader.read_exact(&mut buf[..len]).await?;
        // A reply error means the client dropped the GET (query finalized): stop
        // promptly instead of streaming the rest into the void.
        if query
            .reply(chunk_key(&inner.prefix, &id, index), buf[..len].to_vec())
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}
