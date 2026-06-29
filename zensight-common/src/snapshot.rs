//! Tier-2 directory-snapshot request/status wire types (the ZenSight side of
//! `zenoh-blob`'s tree transfer).
//!
//! An operator asks a sensor to package an allowlisted directory by PUTting a
//! [`SnapshotRequest`] to `<prefix>/@/snapshot/request`; the sensor walks the
//! directory off-thread into a content-addressed chunk store + a [`TreeIndex`]
//! and exposes its [`SnapshotState`] on the `<prefix>/@/snapshot/status`
//! queryable. Once `Ready`, the tree is downloaded via `zenoh-blob`'s
//! [`TreeClient`](zenoh_blob::TreeClient) from the chunk queryable
//! (`<prefix>/@/store/<algo>/<hash>`) + index queryable (`<prefix>/@/tree/<id>`).
//! See `docs/KEYSPACE.md` §3.1b and `docs/LARGE-DATA-TRANSFER.md` §5.7.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub use zenoh_blob::{Entry, TreeIndex};

/// Operator-supplied options for a snapshot request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotOptions {
    /// Restrict generation to one host/source. The control plane is per-protocol
    /// (shared by every host running that protocol), so this disambiguates which
    /// sensor should answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_source: Option<String>,
}

/// PUT to `<prefix>/@/snapshot/request` — the single authorization trigger. The
/// `dir` names an allowlisted directory (never an arbitrary path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRequest {
    /// Client-chosen id correlating the request, status, and tree.
    pub id: Ulid,
    /// Logical name of the allowlisted directory to snapshot.
    pub dir: String,
    /// Options/bounds.
    #[serde(default)]
    pub opts: SnapshotOptions,
}

/// A lightweight summary of a built snapshot, shown before the tree is fetched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSummary {
    /// Number of regular files in the snapshot.
    pub file_count: u64,
    /// Total uncompressed size of all files, bytes.
    pub total_bytes: u64,
    /// Hex of the snapshot's Merkle-y root hash (integrity root).
    pub root_hash_hex: String,
}

/// The lifecycle of one snapshot, reported by the status queryable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SnapshotState {
    /// The directory is being walked + chunked.
    Generating {
        /// The snapshot id.
        id: Ulid,
    },
    /// Ready to download. Fetch the index `tree_id` from `tree_prefix` and the
    /// chunks from `store_prefix` with `zenoh-blob`'s `TreeClient`.
    Ready {
        /// The snapshot id.
        id: Ulid,
        /// The `TreeIndex` id to GET (a single key segment under `tree_prefix`).
        tree_id: String,
        /// Key prefix of the content-addressed chunk queryable (`@/store`).
        store_prefix: String,
        /// Key prefix of the index queryable (`@/tree`).
        tree_prefix: String,
        /// What the tree contains (for display before download).
        summary: SnapshotSummary,
        /// TTL deadline, Unix epoch milliseconds.
        expires_ms: i64,
    },
    /// Generation failed (unknown dir, too big, I/O error, …).
    Failed {
        /// The snapshot id.
        id: Ulid,
        /// Human-readable reason.
        reason: String,
    },
    /// The snapshot's TTL elapsed (or it was cancelled); the chunks are gone.
    Expired {
        /// The snapshot id.
        id: Ulid,
    },
}

impl SnapshotState {
    /// The snapshot id this state refers to.
    pub fn id(&self) -> Ulid {
        match self {
            SnapshotState::Generating { id }
            | SnapshotState::Ready { id, .. }
            | SnapshotState::Failed { id, .. }
            | SnapshotState::Expired { id } => *id,
        }
    }
}

/// One allowlisted directory advertised by the status queryable so the GUI can
/// offer it for download (the path itself is never exposed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDirInfo {
    /// Logical name the operator requests.
    pub name: String,
}

/// Reply of the `<prefix>/@/snapshot/status` queryable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotStatus {
    /// The current (or most recent) snapshot's state, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<SnapshotState>,
    /// Whether a build is in flight (so the GUI can disable the buttons).
    pub busy: bool,
    /// The directories this sensor will snapshot (the allowlist's names).
    #[serde(default)]
    pub dirs: Vec<SnapshotDirInfo>,
    /// Configured max snapshot size in bytes.
    pub max_bytes: u64,
    /// Configured cooldown between builds, seconds.
    pub cooldown_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = SnapshotRequest {
            id: Ulid::from_parts(1, 2),
            dir: "etc".into(),
            opts: SnapshotOptions {
                target_source: Some("host1".into()),
            },
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: SnapshotRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.id, req.id);
        assert_eq!(back.dir, "etc");
        assert_eq!(back.opts.target_source.as_deref(), Some("host1"));
    }

    #[test]
    fn state_tag_and_id() {
        let id = Ulid::from_parts(9, 9);
        let s = SnapshotState::Ready {
            id,
            tree_id: id.to_string(),
            store_prefix: "zensight/sysinfo/@/store".into(),
            tree_prefix: "zensight/sysinfo/@/tree".into(),
            summary: SnapshotSummary {
                file_count: 3,
                total_bytes: 42,
                root_hash_hex: "ab".into(),
            },
            expires_ms: 1,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"state\":\"ready\""));
        assert_eq!(s.id(), id);
    }
}
