//! Serverless Tier-2 — publish a snapshot into a **Zenoh storage** (#201).
//!
//! Instead of running a [`crate::TreeServer`] for the lifetime of a transfer, a
//! producer can PUT its content-addressed chunks and tree index into a
//! router-hosted storage (the `zenoh-plugin-storage-manager`) and then exit. The
//! storage retains the keys, dedups them fleet-wide (a chunk PUT by any producer
//! is reused by all), and answers the GETs that [`crate::TreeClient`] already
//! issues — so the client needs no changes and the producer needn't stay alive.
//!
//! Content addressing makes this safe: a chunk key `<store>/<algo>/<hash>` only
//! ever maps to one byte string, so the storage's last-writer-wins reconciliation
//! is a no-op and re-publishing is idempotent.

use std::sync::Arc;

use crate::error::{BlobError, Result};
use crate::format::{Format, encode};
use crate::hash::{Digest, Hash, Sha256Digest};
use crate::tree::TreeIndex;
use crate::{store_key, tree_key};

/// PUT one content-addressed chunk into the storage under `store_prefix`.
///
/// The key is `<store_prefix>/<algo>/<hash>`; the value is the raw chunk bytes.
/// Idempotent — re-PUTting an identical chunk is a no-op for the storage.
pub async fn publish_chunk(
    session: &Arc<zenoh::Session>,
    store_prefix: &str,
    algo: &str,
    hash: &Hash,
    bytes: &[u8],
) -> Result<()> {
    session
        .put(store_key(store_prefix, algo, hash), bytes.to_vec())
        .await
        .map_err(BlobError::zenoh)
}

/// PUT every `(hash, bytes)` chunk of a built snapshot into the storage. Uses the
/// SHA-256 algorithm name (the only digest the crate ships). Stops at the first
/// PUT error.
pub async fn publish_chunks<I>(
    session: &Arc<zenoh::Session>,
    store_prefix: &str,
    chunks: I,
) -> Result<()>
where
    I: IntoIterator<Item = (Hash, Vec<u8>)>,
{
    let algo = Sha256Digest::name();
    for (hash, bytes) in chunks {
        publish_chunk(session, store_prefix, algo, &hash, &bytes).await?;
    }
    Ok(())
}

/// PUT a tree index into the storage at `<tree_prefix>/<id>`, encoded with
/// `format`. A [`crate::TreeClient`] with the matching `tree_prefix`/`format` then
/// GETs it like any other index.
pub async fn publish_index(
    session: &Arc<zenoh::Session>,
    tree_prefix: &str,
    index: &TreeIndex,
    format: Format,
) -> Result<()> {
    let payload = encode(index, format)?;
    session
        .put(tree_key(tree_prefix, &index.id), payload)
        .await
        .map_err(BlobError::zenoh)
}

/// Publish a whole snapshot — chunks then index — into the storage. After this
/// resolves the producer may exit: the storage serves the snapshot to clients.
pub async fn publish_snapshot(
    session: &Arc<zenoh::Session>,
    store_prefix: &str,
    tree_prefix: &str,
    index: &TreeIndex,
    chunks: Vec<(Hash, Vec<u8>)>,
    format: Format,
) -> Result<()> {
    publish_chunks(session, store_prefix, chunks).await?;
    publish_index(session, tree_prefix, index, format).await?;
    Ok(())
}
