//! Control-channel command primitives shared by sensors and the frontend.
//!
//! Sensors expose runtime control via two conventional channels under their
//! key prefix:
//! - commands (pub/sub): `zensight/<protocol>/@/commands/<topic>`
//! - status (queryable): `zensight/<protocol>/@/status/<topic>`
//!
//! A "topic" namespaces a control surface — e.g. `filter` (syslog),
//! `expectations` (the sentinel), `detectors` (netring). The payload type is
//! topic-specific; wrap it in [`Command`] when you want an optional correlation
//! id for matching an async reply.

use serde::{Deserialize, Serialize};

/// Build the command key for a sensor `prefix` and `topic`.
///
/// # Example
/// ```
/// use zensight_common::command::command_key;
/// assert_eq!(command_key("zensight/logs", "filter"), "zensight/logs/@/commands/filter");
/// ```
pub fn command_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/commands/{}", prefix, topic)
}

/// Build the status (queryable) key for a sensor `prefix` and `topic`.
///
/// # Example
/// ```
/// use zensight_common::command::status_key;
/// assert_eq!(status_key("zensight/logs", "filter"), "zensight/logs/@/status/filter");
/// ```
pub fn status_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/status/{}", prefix, topic)
}

/// Build the on-demand detail-query (queryable) key for a sensor `prefix` and
/// `topic`. High-cardinality detail (flow tables, socket lists, …) is served
/// here on request, never streamed onto the telemetry bus.
///
/// # Example
/// ```
/// use zensight_common::command::query_key;
/// assert_eq!(query_key("zensight/netring", "flows"), "zensight/netring/@/query/flows");
/// ```
pub fn query_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/query/{}", prefix, topic)
}

/// Build the artifact-request (subscriber) key: PUT an `ArtifactRequest` here to
/// ask a sensor to produce an artifact (report / snapshot / capture).
///
/// # Example
/// ```
/// use zensight_common::command::artifact_request_key;
/// assert_eq!(artifact_request_key("zensight/netlink"), "zensight/netlink/@/artifact/request");
/// ```
pub fn artifact_request_key(prefix: &str) -> String {
    format!("{prefix}/@/artifact/request")
}

/// Build the artifact-status (queryable) key: GET an `ArtifactStatus` to learn
/// the produced kinds and track each one's lifecycle.
pub fn artifact_status_key(prefix: &str) -> String {
    format!("{prefix}/@/artifact/status")
}

/// Build the artifact-cancel (subscriber) key: PUT an artifact id (ULID string)
/// to abort an in-flight production or free a ready artifact early.
pub fn artifact_cancel_key(prefix: &str) -> String {
    format!("{prefix}/@/artifact/cancel")
}

/// Build the key prefix of the `zenoh-blob` server that serves Tier-1 artifact
/// bytes. The blob lives under `<prefix>/@/artifact/blob/<id>/…` (its own `blob/`
/// segment so the blob queryable on `…/blob/**` cannot collide with the
/// request/status/cancel channels).
pub fn artifact_blob_prefix(prefix: &str) -> String {
    format!("{prefix}/@/artifact/blob")
}

/// Build the key prefix of the content-addressed chunk queryable (Tier-2 tree
/// delivery). Chunks live at `<prefix>/@/store/<algo>/<hash>` — immutable, so
/// cacheable fleet-wide. Shared by every tree-delivering artifact kind.
pub fn artifact_store_prefix(prefix: &str) -> String {
    format!("{prefix}/@/store")
}

/// Build the key prefix of the tree-index queryable (Tier-2 tree delivery). An
/// index lives at `<prefix>/@/tree/<id>`.
pub fn artifact_tree_prefix(prefix: &str) -> String {
    format!("{prefix}/@/tree")
}

/// Optional envelope carrying a correlation id alongside a command body.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        assert_eq!(
            command_key("zensight/netlink", "expectations"),
            "zensight/netlink/@/commands/expectations"
        );
        assert_eq!(
            status_key("zensight/netring", "detectors"),
            "zensight/netring/@/status/detectors"
        );
    }

    #[test]
    fn artifact_key_builders() {
        let p = "zensight/netlink";
        assert_eq!(
            artifact_request_key(p),
            "zensight/netlink/@/artifact/request"
        );
        assert_eq!(artifact_status_key(p), "zensight/netlink/@/artifact/status");
        assert_eq!(artifact_cancel_key(p), "zensight/netlink/@/artifact/cancel");
        assert_eq!(artifact_blob_prefix(p), "zensight/netlink/@/artifact/blob");
        // The blob server (queryable on `…/blob/**`) must not swallow the
        // request/status/cancel control channels.
        assert!(!artifact_request_key(p).starts_with(&artifact_blob_prefix(p)));
        // Tier-2 store/tree keys are shared, kind-agnostic delivery infrastructure.
        assert_eq!(artifact_store_prefix(p), "zensight/netlink/@/store");
        assert_eq!(artifact_tree_prefix(p), "zensight/netlink/@/tree");
        assert!(!artifact_request_key(p).starts_with(&artifact_store_prefix(p)));
        assert!(!artifact_request_key(p).starts_with(&artifact_tree_prefix(p)));
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
