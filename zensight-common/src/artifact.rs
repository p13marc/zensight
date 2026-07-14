//! Unified artifact request/status wire types (the ZenSight side of `zenoh-blob`).
//!
//! One control channel subsumes what used to be the separate `@/report` (Tier-1
//! whole-file debug bundles) and `@/snapshot` (Tier-2 content-addressed directory
//! trees) surfaces, and hosts new artifact kinds (e.g. on-demand packet capture)
//! without a third copy of the same machinery.
//!
//! An operator asks a sensor to produce an artifact by sending an
//! [`ArtifactRequest`] to the `artifact/request` write procedure
//! (`@rpc/<producer>/artifact/request`, request/reply); the sensor produces it
//! off the poll/capture path and exposes per-kind [`ArtifactState`] on the
//! `artifact/status` read procedure (`artifact/cancel` aborts). Once `Ready`,
//! the artifact is fetched via `zenoh-blob` — a Tier-1 [`Delivery::Blob`] from
//! the blob queryable under the `@blob/artifact` prefix, or a Tier-2
//! [`Delivery::Tree`] from the content-addressed chunk
//! (`@blob/store/<algo>/<hash>`) + index (`@blob/tree/<id>`) queryables. See
//! `docs/KEYSPACE.md` §3 and
//! `docs/design/large-data-transfer.md`.
//!
//! Typed [`ArtifactKind`] (rather than opaque JSON params) is deliberate: this is
//! a single repo with small, stable param sets, so both ends get type safety.
//! Forward-compat is preserved by the `#[serde(other)]` [`ArtifactKind::Unsupported`]
//! / [`KindAdvert::Unknown`] fallbacks, so an old sensor or GUI degrades cleanly
//! against a peer that knows a newer kind.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub use zenoh_blob::{Entry, Manifest, TreeIndex};

/// What to produce. The variant carries the kind-specific parameters; the
/// discriminant is serialized under a `kind` tag (`report` / `snapshot` /
/// `capture`), so `ArtifactRequest` flattens it onto the request object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A sosreport-style debug bundle: redacted config + health + counters.
    /// Tier-1 (a single `tar.zst` file).
    Report {},
    /// An allowlisted directory, content-addressed. Tier-2. `dir` names an
    /// allowlisted directory (never an arbitrary path — that is the authz
    /// boundary).
    Snapshot {
        /// Logical name of the allowlisted directory to snapshot.
        dir: String,
    },
    /// A bounded on-demand packet capture. Tier-1 (a single `pcap[.zst]` file).
    /// Produced by the netring sensor; the params live here (accepting the
    /// documented coupling) so both ends stay typed.
    Capture {
        /// How long to capture, seconds. The sensor clamps to its configured max.
        duration_secs: u32,
        /// Optional early-stop byte cap; the sensor clamps to its configured max.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
        /// Optional capture filter expression (sensor-parsed, e.g. a BPF-like
        /// predicate). Rejected at request time if the sensor disallows filters
        /// or the expression does not parse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        /// Optional per-packet snap length (bytes captured per frame).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snaplen: Option<u32>,
        /// Whether to zstd-compress the finished capture before serving.
        #[serde(default)]
        compress: bool,
    },
    /// Any kind this sensor build doesn't understand (forward-compat). A sensor
    /// rejects it with [`ArtifactState::Failed`].
    #[serde(other)]
    Unsupported,
}

impl ArtifactKind {
    /// The stable slug identifying this kind, used to correlate a request with
    /// its per-kind status entry (`report` / `snapshot` / `capture` /
    /// `unsupported`).
    pub fn slug(&self) -> &'static str {
        match self {
            ArtifactKind::Report {} => "report",
            ArtifactKind::Snapshot { .. } => "snapshot",
            ArtifactKind::Capture { .. } => "capture",
            ArtifactKind::Unsupported => "unsupported",
        }
    }
}

/// Operator-supplied options common to every kind. The sensor clamps params to
/// its configured limits regardless.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtifactOptions {
    /// Restrict production to one host/source. The control plane is per-protocol
    /// (shared by every host running that protocol), so this disambiguates which
    /// sensor should answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_source: Option<String>,
}

/// Payload of the `artifact/request` write procedure — the single
/// authorization trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRequest {
    /// Client-chosen id correlating the request, status, and delivery.
    pub id: Ulid,
    /// What to produce (flattened: the `kind` tag rides on this object).
    #[serde(flatten)]
    pub kind: ArtifactKind,
    /// Options/bounds.
    #[serde(default)]
    pub opts: ArtifactOptions,
}

/// A lightweight summary of a built directory tree, shown before it is fetched.
/// (Formerly `SnapshotSummary`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeSummary {
    /// Number of regular files in the tree.
    pub file_count: u64,
    /// Total uncompressed size of all files, bytes.
    pub total_bytes: u64,
    /// Hex of the tree's Merkle-y root hash (integrity root).
    pub root_hash_hex: String,
}

/// How a `Ready` artifact is delivered — the client picks its transfer client
/// (whole-blob vs content-addressed tree) off this tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum Delivery {
    /// Tier-1 whole-blob: download with `zenoh_blob::BlobClient` from
    /// `blob_prefix` using `manifest.id`.
    Blob {
        /// Whole-blob manifest (size, chunking, SHA-256, filename).
        manifest: Manifest,
        /// Key prefix of the `zenoh-blob` server serving this blob.
        blob_prefix: String,
    },
    /// Tier-2 tree: download with `zenoh_blob::TreeClient` — fetch the index
    /// `tree_id` from `tree_prefix`, chunks from `store_prefix`.
    Tree {
        /// The `TreeIndex` id to GET (a single key segment under `tree_prefix`).
        tree_id: String,
        /// Key prefix of the content-addressed chunk queryable (`@/store`).
        store_prefix: String,
        /// Key prefix of the index queryable (`@/tree`).
        tree_prefix: String,
        /// What the tree contains (for display before download).
        summary: TreeSummary,
    },
}

/// The lifecycle of one artifact, reported per kind by the status queryable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ArtifactState {
    /// The artifact is being produced. `detail`/`progress` let a long-running
    /// producer (e.g. a multi-second capture) report incremental progress.
    Generating {
        /// The artifact id.
        id: Ulid,
        /// The kind slug (see [`ArtifactKind::slug`]).
        kind: String,
        /// Optional human-readable progress line (e.g. `"capturing 12s/30s"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// Optional fractional progress in `0.0..=1.0`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<f32>,
    },
    /// Ready to download via `delivery`; available until `expires_ms`.
    Ready {
        /// The artifact id.
        id: Ulid,
        /// The kind slug.
        kind: String,
        /// How to fetch the bytes.
        delivery: Delivery,
        /// TTL deadline, Unix epoch milliseconds.
        expires_ms: i64,
    },
    /// Production failed (or was cancelled mid-flight).
    Failed {
        /// The artifact id.
        id: Ulid,
        /// The kind slug.
        kind: String,
        /// Human-readable reason.
        reason: String,
    },
    /// The artifact's TTL elapsed (or it was cancelled after becoming ready); the
    /// bytes are gone.
    Expired {
        /// The artifact id.
        id: Ulid,
        /// The kind slug.
        kind: String,
    },
}

impl ArtifactState {
    /// The artifact id this state refers to.
    pub fn id(&self) -> Ulid {
        match self {
            ArtifactState::Generating { id, .. }
            | ArtifactState::Ready { id, .. }
            | ArtifactState::Failed { id, .. }
            | ArtifactState::Expired { id, .. } => *id,
        }
    }

    /// The kind slug this state refers to.
    pub fn kind(&self) -> &str {
        match self {
            ArtifactState::Generating { kind, .. }
            | ArtifactState::Ready { kind, .. }
            | ArtifactState::Failed { kind, .. }
            | ArtifactState::Expired { kind, .. } => kind,
        }
    }
}

/// Per-kind capabilities the sensor advertises so the GUI can render the right
/// request affordance (a button, a per-dir list, a capture form). Unknown kinds
/// are hidden by the GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind_advert", rename_all = "snake_case")]
pub enum KindAdvert {
    /// A debug bundle can be requested (no parameters).
    Report {},
    /// A directory snapshot can be requested for any of these allowlisted names.
    Snapshot {
        /// The allowlisted directory names (paths are never exposed).
        dirs: Vec<String>,
    },
    /// A packet capture can be requested within these bounds.
    Capture {
        /// Largest allowed capture duration, seconds.
        max_duration_secs: u32,
        /// Whether a capture filter expression is accepted.
        filter_allowed: bool,
        /// Largest allowed snap length, bytes (0 = full frame).
        snaplen_max: u32,
    },
    /// A kind this GUI build doesn't understand (forward-compat) — hidden.
    #[serde(other)]
    Unknown,
}

/// The status of one artifact kind on a sensor (one entry per registered
/// producer), reported by the status queryable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindStatus {
    /// The kind slug (`report` / `snapshot` / `capture`).
    pub kind: String,
    /// Whether a production of this kind is in flight.
    pub busy: bool,
    /// The current (or most recent) state for this kind, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ArtifactState>,
    /// Configured max artifact size in bytes.
    pub max_bytes: u64,
    /// Configured cooldown between productions, seconds.
    pub cooldown_secs: u64,
    /// What the GUI needs to render this kind's request affordance.
    pub advert: KindAdvert,
}

/// Reply of the `artifact/status` read procedure: one entry per kind the
/// sensor produces. Busy/cooldown/current are per kind, so a slow capture never
/// blocks a quick debug bundle.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtifactStatus {
    /// The artifact kinds this sensor produces and their per-kind state.
    #[serde(default)]
    pub kinds: Vec<KindStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_decodes_to_unsupported() {
        let req: ArtifactRequest =
            serde_json::from_str(r#"{"id":"00000000000000000000000000","kind":"flux_capacitor"}"#)
                .unwrap();
        assert_eq!(req.kind, ArtifactKind::Unsupported);
        assert_eq!(req.kind.slug(), "unsupported");
    }

    #[test]
    fn report_request_roundtrip_flattens_kind() {
        let req = ArtifactRequest {
            id: Ulid::from_parts(1, 2),
            kind: ArtifactKind::Report {},
            opts: ArtifactOptions {
                target_source: Some("host1".into()),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        // `kind` rides flattened on the request object.
        assert!(json.contains("\"kind\":\"report\""));
        let back: ArtifactRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn snapshot_and_capture_roundtrip() {
        for kind in [
            ArtifactKind::Snapshot { dir: "etc".into() },
            ArtifactKind::Capture {
                duration_secs: 30,
                max_bytes: Some(1 << 20),
                filter: Some("tcp port 443".into()),
                snaplen: Some(128),
                compress: true,
            },
        ] {
            let slug = kind.slug();
            let req = ArtifactRequest {
                id: Ulid::from_parts(3, 4),
                kind,
                opts: ArtifactOptions::default(),
            };
            let bytes = serde_json::to_vec(&req).unwrap();
            let back: ArtifactRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(back, req);
            assert_eq!(back.kind.slug(), slug);
        }
    }

    #[test]
    fn ready_blob_vs_tree_delivery_tag() {
        let blob = ArtifactState::Ready {
            id: Ulid::from_parts(9, 9),
            kind: "report".into(),
            delivery: Delivery::Blob {
                manifest: Manifest {
                    id: "00000000000000000000000009".into(),
                    filename: "zensight-debug.tar.zst".into(),
                    total_len: 1024,
                    chunk_size: 512,
                    chunk_count: 2,
                    hash_algo: "sha256".into(),
                    hash: zenoh_blob::Hash([0u8; 32]),
                    created_ms: 1,
                },
                blob_prefix: "zensight/v1/h-3fa9c2d41b7e/@blob/artifact".into(),
            },
            expires_ms: 1,
        };
        let json = serde_json::to_string(&blob).unwrap();
        assert!(json.contains("\"state\":\"ready\""));
        assert!(json.contains("\"delivery\":\"blob\""));
        let back: ArtifactState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, blob);
        assert_eq!(back.kind(), "report");

        let tree = ArtifactState::Ready {
            id: Ulid::from_parts(9, 9),
            kind: "snapshot".into(),
            delivery: Delivery::Tree {
                tree_id: "t".into(),
                store_prefix: "zensight/v1/h-3fa9c2d41b7e/@blob/store".into(),
                tree_prefix: "zensight/v1/h-3fa9c2d41b7e/@blob/tree".into(),
                summary: TreeSummary {
                    file_count: 3,
                    total_bytes: 42,
                    root_hash_hex: "ab".into(),
                },
            },
            expires_ms: 2,
        };
        let json = serde_json::to_string(&tree).unwrap();
        assert!(json.contains("\"delivery\":\"tree\""));
        assert_eq!(serde_json::from_str::<ArtifactState>(&json).unwrap(), tree);
    }

    #[test]
    fn status_roundtrip_and_unknown_advert() {
        let status = ArtifactStatus {
            kinds: vec![KindStatus {
                kind: "report".into(),
                busy: false,
                current: None,
                max_bytes: 1024,
                cooldown_secs: 30,
                advert: KindAdvert::Report {},
            }],
        };
        let bytes = serde_json::to_vec(&status).unwrap();
        assert_eq!(
            serde_json::from_slice::<ArtifactStatus>(&bytes).unwrap(),
            status
        );

        // An unknown advert tag decodes to Unknown (forward-compat for old GUIs).
        let a: KindAdvert = serde_json::from_str(r#"{"kind_advert":"hologram"}"#).unwrap();
        assert_eq!(a, KindAdvert::Unknown);
    }
}
