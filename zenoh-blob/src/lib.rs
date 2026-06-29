//! `zenoh-blob` — generic resumable chunked blob transfer over Zenoh.
//!
//! A small, self-contained library for downloading a large artifact (a file, a
//! report bundle, a pcap) from one Zenoh peer to another with **progress**,
//! **SHA-256 integrity**, **range resume**, and **bounded memory**. It carries no
//! application-specific types — see `docs/LARGE-DATA-TRANSFER.md` for the design
//! and the ZenSight adapter that wraps it.
//!
//! # Model
//!
//! One queryable serves every blob under a key prefix:
//!
//! ```text
//! queryable on:   <prefix>/**
//! manifest GET:   <prefix>/<id>/manifest        -> the Manifest (one reply)
//! chunk GET:      <prefix>/<id>/**?from=<K>      -> chunk replies K.. (any order)
//! manifest reply: <prefix>/<id>/manifest
//! chunk reply:    <prefix>/<id>/chunk/<index>
//! ```
//!
//! A download is **two** queries: the client fetches the [`Manifest`] first, then
//! streams the chunks. The manifest-first step is not cosmetic — **Zenoh does not
//! order query replies**, so a chunk can be delivered before the manifest would
//! have been on a combined query. Knowing `chunk_size` up front lets the client
//! place every (out-of-order) chunk by its byte offset and write it straight to
//! disk, so memory stays O(chunk_size) regardless of blob size and arrival order.
//!
//! All sizing/integrity metadata lives in the [`Manifest`] (not in the chunk
//! keys), so there is no second source of truth. The server streams chunks lazily
//! from a reader (never `read_to_end`); the client writes each chunk to a `.part`
//! file at its offset and verifies the whole-blob hash at the end.
//!
//! # Two Zenoh facts this design relies on
//!
//! 1. **Backpressure is automatic.** `Session::get` defaults to
//!    `CongestionControl::Block`, and replies inherit the query's congestion
//!    control, so chunk replies block (rather than drop) when the link backs up.
//!    We therefore set **no** congestion control explicitly — the only setter is
//!    behind Zenoh's `internal` feature, which this crate deliberately does not
//!    enable. Do not "fix" this by enabling `internal`.
//! 2. **Reply keys must match the query.** Replies use `ReplyKeyExpr::MatchingQuery`
//!    by default, so the client **must** GET the wildcard `<prefix>/<id>/**` for
//!    the `chunk/<i>` replies to be accepted. A bare-`<id>` GET would silently
//!    reject every chunk. [`download_selector`] enforces the wildcard.

mod cancel;
mod chunk;
mod client;
mod error;
mod format;
mod hash;
mod manifest;
mod progress;
mod resume;
mod server;

pub use cancel::CancelToken;
pub use chunk::{
    ChunkSize, Chunker, DEFAULT_CHUNK_SIZE, FixedSizeChunker, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE,
};
pub use client::BlobClient;
pub use error::{BlobError, Result};
pub use format::{Format, decode, encode};
pub use hash::{Digest, Hash, Sha256Digest};
pub use manifest::Manifest;
pub use progress::{Progress, ProgressSink};
pub use server::{AsyncReadSeek, BlobServer, BlobSource, FileBlobSource, OpenFuture};

/// Key of the manifest reply for blob `id` under `prefix`.
pub fn manifest_key(prefix: &str, id: &str) -> String {
    format!("{prefix}/{id}/manifest")
}

/// Key of chunk `index` for blob `id` under `prefix`.
pub fn chunk_key(prefix: &str, id: &str, index: u32) -> String {
    format!("{prefix}/{id}/chunk/{index}")
}

/// Selector a client GETs to download blob `id` from chunk `from`.
///
/// Always ends in the `/**` wildcard so the manifest and chunk replies both match
/// the query (see the crate docs, fact 2).
pub fn download_selector(prefix: &str, id: &str, from: u32) -> String {
    format!("{prefix}/{id}/**?from={from}")
}

/// Extract the blob `id` from a query key expression seen by a server declared on
/// `<prefix>/**`. The id is the single segment following the prefix.
pub fn parse_id(prefix: &str, key_expr: &str) -> Option<String> {
    let rest = key_expr.strip_prefix(prefix)?.strip_prefix('/')?;
    let id = rest.split('/').next()?;
    if id.is_empty() || id == "**" {
        None
    } else {
        Some(id.to_string())
    }
}

/// Parse the `from=<K>` selector parameter (defaults to 0 if absent/malformed).
pub fn parse_from(params: &str) -> u32 {
    for pair in params.split('&') {
        if let Some(v) = pair.strip_prefix("from=") {
            return v.parse().unwrap_or(0);
        }
    }
    0
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn key_builders() {
        assert_eq!(
            manifest_key("zensight/x/@/report/A", "A"),
            "zensight/x/@/report/A/A/manifest"
        );
        assert_eq!(chunk_key("p", "A", 7), "p/A/chunk/7");
        assert_eq!(download_selector("p", "A", 3), "p/A/**?from=3");
        assert!(download_selector("p", "A", 0).ends_with("/**?from=0"));
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_id("p", "p/A/**").as_deref(), Some("A"));
        assert_eq!(parse_id("p", "p/A/manifest").as_deref(), Some("A"));
        assert_eq!(parse_id("p", "p/**"), None);
        assert_eq!(parse_id("other", "p/A/**"), None);

        assert_eq!(parse_from("from=5"), 5);
        assert_eq!(parse_from("foo=1&from=9"), 9);
        assert_eq!(parse_from(""), 0);
        assert_eq!(parse_from("from=bad"), 0);
    }
}
