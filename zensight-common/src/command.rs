//! Control-plane key builders shared by sensors and the frontend — v1
//! (`@rpc` procedures + `@blob` tiers, RFC 05/07; epic #453).
//!
//! Runtime control is request/reply on the `@rpc` plane:
//! - write: `<base>/v1/<origin>/@rpc/<producer>/<topic>/set`
//! - read:  `<base>/v1/<origin>/@rpc/<producer>/<topic>`
//!
//! Every builder takes the **producer name** ("logs", "netring", …) and
//! derives the LOCAL host origin — these are serving-side builders; fleet
//! callers use [`crate::keyexpr::fleet_rpc_key`]/[`crate::keyexpr::origin_rpc_key`].

use serde::{Deserialize, Serialize};
use zenkey::V1Context;
use zenkey::grammar::BlobTier;

/// The write procedure for a control topic: `…/@rpc/<producer>/<topic>/set`.
///
/// # Example
/// ```
/// use zensight_common::command::command_key;
/// let k = command_key("logs", "filter");
/// assert!(k.starts_with("v1/h-"));
/// assert!(k.ends_with("/@rpc/logs/filter/set"));
/// ```
pub fn command_key(producer: &str, topic: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .rpc_key(&[topic, "set"])
        .into()
}

/// The read procedure for a control topic: `…/@rpc/<producer>/<topic>`
/// (the reply carries the topic's current configuration/status).
pub fn status_key(producer: &str, topic: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .rpc_key(&[topic])
        .into()
}

/// The on-demand detail-read procedure: `…/@rpc/<producer>/<topic>`.
/// High-cardinality detail (flow tables, socket lists, …) is served here on
/// request, never streamed onto the telemetry bus (RFC 04 R3). Same key
/// shape as [`status_key`] — reads are reads (RFC 05 §5).
pub fn query_key(producer: &str, topic: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .rpc_key(&[topic])
        .into()
}

/// The artifact-request write procedure (RFC 05 §3 long-running pattern).
pub fn artifact_request_key(producer: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .rpc_key(&["artifact", "request"])
        .into()
}

/// The artifact-status read procedure. (Residual: the RFC's ideal is the
/// observable `state/<producer>/artifact/<kind>` document; the read
/// procedure remains for the transition.)
pub fn artifact_status_key(producer: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .rpc_key(&["artifact", "status"])
        .into()
}

/// The artifact-cancel write procedure (`?id=<ulid>`).
pub fn artifact_cancel_key(producer: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .rpc_key(&["artifact", "cancel"])
        .into()
}

/// Tier-1 blob prefix: `<base>/v1/<origin>/@blob/artifact` — a produced
/// artifact's manifest + chunks live under `…/artifact/<id>/**` (RFC 07 §2).
pub fn artifact_blob_prefix(producer: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .blob_prefix(BlobTier::Artifact)
        .into()
}

/// Tier-2 content-store prefix: `<base>/v1/<origin>/@blob/store` — chunks
/// at `…/store/<algo>/<hash>`, immutable ⇒ cacheable fleet-wide.
pub fn artifact_store_prefix(producer: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .blob_prefix(BlobTier::Store)
        .into()
}

/// Tier-2 tree-index prefix: `<base>/v1/<origin>/@blob/tree`.
pub fn artifact_tree_prefix(producer: &str) -> String {
    V1Context::for_producer(&crate::PROFILE, producer)
        .blob_prefix(BlobTier::Tree)
        .into()
}

/// Optional envelope carrying a correlation id alongside a command body.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Command<T> {
    /// Optional correlation id, echoed in a reply for request/response matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The topic-specific command payload.
    pub body: T,
}

impl<T> Command<T> {
    pub fn new(body: T) -> Self {
        Self { id: None, body }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_builders() {
        let k = command_key("netlink", "expectations");
        assert!(k.starts_with("v1/h-"), "{k}");
        assert!(k.ends_with("/@rpc/netlink/expectations/set"), "{k}");
        let s = status_key("netring", "detectors");
        assert!(s.ends_with("/@rpc/netring/detectors"), "{s}");
        // Reads are reads: status and query share the shape.
        assert_eq!(s, query_key("netring", "detectors"));
    }

    #[test]
    fn artifact_key_builders() {
        let p = "netlink";
        assert!(artifact_request_key(p).ends_with("/@rpc/netlink/artifact/request"));
        assert!(artifact_status_key(p).ends_with("/@rpc/netlink/artifact/status"));
        assert!(artifact_cancel_key(p).ends_with("/@rpc/netlink/artifact/cancel"));
        assert!(artifact_blob_prefix(p).ends_with("/@blob/artifact"));
        assert!(artifact_store_prefix(p).ends_with("/@blob/store"));
        assert!(artifact_tree_prefix(p).ends_with("/@blob/tree"));
        // Control procedures and blob delivery live on different planes.
        assert!(!artifact_request_key(p).starts_with(&artifact_blob_prefix(p)));
    }

    #[test]
    fn command_envelope_roundtrip() {
        let cmd = Command::new(serde_json::json!({"a": 1})).with_id("x");
        let bytes = crate::encode(&cmd, crate::Format::Json).unwrap();
        let back: Command<serde_json::Value> = crate::decode(&bytes, crate::Format::Json).unwrap();
        assert_eq!(back.id.as_deref(), Some("x"));
        assert_eq!(back.body["a"], 1);
    }
}
