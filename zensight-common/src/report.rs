//! Debug-report request/status wire types (the ZenSight side of `zenoh-blob`).
//!
//! An operator asks a sensor to package a debug bundle by PUTting a
//! [`ReportRequest`] to `<prefix>/@/report/request`; the sensor generates it
//! off-thread and exposes its [`ReportState`] on the `<prefix>/@/report/status`
//! queryable. Once `Ready`, the bundle is downloaded via `zenoh-blob` from the
//! blob queryable under `<prefix>/@/report/blob`. See `docs/KEYSPACE.md` §3 and
//! `docs/LARGE-DATA-TRANSFER.md` §5.
//!
//! The whole-blob [`Manifest`] is re-exported from `zenoh-blob` (the authoritative
//! source of sizing/integrity metadata) and embedded in [`ReportState::Ready`] so
//! the GUI can show size/progress before downloading.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub use zenoh_blob::Manifest;

/// What to package into a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    /// A sosreport-style debug bundle: redacted config + health + counters.
    DebugBundle,
    /// Any kind this sensor build doesn't understand (forward-compat). A sensor
    /// rejects it with [`ReportState::Failed`].
    #[serde(other)]
    Unsupported,
}

/// Operator-supplied options. All optional; the sensor clamps to its configured
/// limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportOptions {
    /// Restrict generation to one host/source. The control plane is per-protocol
    /// (shared by every host running that protocol), so this disambiguates which
    /// sensor should answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_source: Option<String>,
}

/// PUT to `<prefix>/@/report/request` — the single authorization trigger (R7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRequest {
    /// Client-chosen id correlating the request, status, and blob.
    pub id: Ulid,
    /// What to generate.
    pub kind: ReportKind,
    /// Options/bounds.
    #[serde(default)]
    pub opts: ReportOptions,
}

/// The lifecycle of one report, reported by the status queryable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReportState {
    /// The bundle is being generated.
    Generating {
        /// The report id.
        id: Ulid,
    },
    /// Ready to download from `blob_prefix` (a `zenoh-blob` server) using
    /// `manifest.id`; available until `expires_ms`.
    Ready {
        /// The report id (equals `manifest.id` as a ULID).
        id: Ulid,
        /// Whole-blob manifest (size, chunking, SHA-256, filename).
        manifest: Manifest,
        /// Key prefix of the `zenoh-blob` server serving this blob.
        blob_prefix: String,
        /// TTL deadline, Unix epoch milliseconds.
        expires_ms: i64,
    },
    /// Generation failed.
    Failed {
        /// The report id.
        id: Ulid,
        /// Human-readable reason.
        reason: String,
    },
    /// The report's TTL elapsed (or it was cancelled); the artifact is gone.
    Expired {
        /// The report id.
        id: Ulid,
    },
}

impl ReportState {
    /// The report id this state refers to.
    pub fn id(&self) -> Ulid {
        match self {
            ReportState::Generating { id }
            | ReportState::Ready { id, .. }
            | ReportState::Failed { id, .. }
            | ReportState::Expired { id } => *id,
        }
    }
}

/// Reply of the `<prefix>/@/report/status` queryable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportStatus {
    /// The current (or most recent) report's state, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ReportState>,
    /// Whether a generation is in flight (so the GUI can disable the button).
    pub busy: bool,
    /// Configured max bundle size in bytes.
    pub max_bytes: u64,
    /// Configured cooldown between generations, seconds.
    pub cooldown_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_decodes_to_unsupported() {
        let v: ReportKind = serde_json::from_str("\"pcap_future\"").unwrap();
        assert_eq!(v, ReportKind::Unsupported);
    }

    #[test]
    fn request_roundtrip() {
        let req = ReportRequest {
            id: Ulid::from_parts(1, 2),
            kind: ReportKind::DebugBundle,
            opts: ReportOptions {
                target_source: Some("host1".into()),
            },
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: ReportRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.id, req.id);
        assert_eq!(back.kind, ReportKind::DebugBundle);
        assert_eq!(back.opts.target_source.as_deref(), Some("host1"));
    }

    #[test]
    fn state_tag_and_id() {
        let id = Ulid::from_parts(9, 9);
        let s = ReportState::Generating { id };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"state\":\"generating\""));
        assert_eq!(s.id(), id);
    }
}
